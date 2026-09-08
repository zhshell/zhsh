//! 单次 Agent 任务的取消状态、进程组中断与活动任务登记。
//!
//! 取消检查、子进程创建和进程组登记共用令牌内部的同一把锁，因此 Ctrl-C 要么发生在
//! 创建之前并阻止命令启动，要么发生在登记之后并向完整进程组发送 SIGINT，不存在
//! “已经启动但尚不可取消”的窗口。相同令牌还通过 watch 通道通知异步 HTTP 请求，
//! 使网络 future 与命令进程共享一次 Agent 任务的取消边界。

use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use tokio::sync::watch;

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct State {
    cancelled: bool,
    interrupted: bool,
    active_process_group: Option<u32>,
}

/// 每次 Agent 请求独享的取消令牌。
pub(crate) struct CancellationToken {
    id: u64,
    state: Mutex<State>,
    async_cancelled: watch::Sender<bool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        let (async_cancelled, _) = watch::channel(false);
        Self {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(State::default()),
            async_cancelled,
        }
    }
}

impl CancellationToken {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 返回任务是否已收到取消请求。
    pub(crate) fn is_cancelled(&self) -> bool {
        let state = self.state();
        state.cancelled || state.interrupted
    }

    /// 区分 Ctrl-C 的整任务取消与宿主为切换 Agent 流状态而发出的操作中断。
    pub(crate) fn is_hard_cancelled(&self) -> bool {
        self.state().cancelled
    }

    pub(crate) fn is_interrupted(&self) -> bool {
        self.state().interrupted
    }

    /// 返回只读任务编号，用于隔离异步请求句柄和迟到响应。
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// 订阅不会遗漏既有取消状态的异步通知。
    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.async_cancelled.subscribe()
    }

    /// 标记任务取消，并中断已经登记的 Agent 命令进程组。
    ///
    /// 本操作是幂等的。它不能回滚已完成的命令副作用或供应商已经接收的 HTTP 请求。
    pub(crate) fn cancel(&self) {
        let process_group = {
            let mut state = self.state();
            state.cancelled = true;
            state.active_process_group
        };
        #[cfg(unix)]
        if let Some(process_group) = process_group {
            // SAFETY: `spawn` 把子进程 PID 登记为独立进程组 ID；负 PID 按 POSIX
            // 语义向该进程组发送 SIGINT。这里不解引用指针，失败只会返回错误码。
            unsafe {
                libc::kill(-(process_group as i32), libc::SIGINT);
            }
        }
        #[cfg(not(unix))]
        let _ = process_group;
        self.async_cancelled.send_replace(true);
    }

    /// 只中断当前 HTTP/进程组操作，不把 Agent 任务提交为取消终态。
    ///
    /// 该标志只承担底层执行机械效果；阶段、轮次和恢复策略仍由 AgentFlowState 决定。
    pub(crate) fn interrupt(&self) {
        let process_group = {
            let mut state = self.state();
            state.interrupted = true;
            state.active_process_group
        };
        #[cfg(unix)]
        if let Some(process_group) = process_group {
            // SAFETY: process_group 由 spawn 登记为独立 PGID。
            unsafe {
                libc::kill(-(process_group as i32), libc::SIGINT);
            }
        }
        #[cfg(not(unix))]
        let _ = process_group;
        self.async_cancelled.send_replace(true);
    }

    /// 在持有取消状态锁时执行一个不能越过取消点的同步操作。
    ///
    /// # Arguments
    ///
    /// - `action`：仅在尚未取消时执行的短操作；不得在闭包内再次锁定同一令牌。
    ///
    /// # Returns
    ///
    /// 已取消时返回 [`None`]，否则返回闭包结果。持锁覆盖闭包是刻意的竞态边界。
    pub(crate) fn run_if_active<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let state = self.state();
        if state.cancelled || state.interrupted {
            return None;
        }
        let result = action();
        drop(state);
        Some(result)
    }

    /// 原子完成取消检查、子进程创建和活动进程组登记。
    ///
    /// # Arguments
    ///
    /// - `command`：已配置 stdio、cwd 和环境，但尚未启动的命令。
    ///
    /// # Returns
    ///
    /// 启动成功返回子进程；取消已经发生时返回 `Ok(None)`，不会创建进程。
    ///
    /// # Errors
    ///
    /// 操作系统拒绝创建子进程时返回原始 I/O 错误。
    pub(crate) fn spawn(&self, command: &mut Command) -> std::io::Result<Option<Child>> {
        self.spawn_in_process_group(command, None)
    }

    /// 启动同一条静态 pipeline 的 leader 或成员，并把整个 pipeline 作为一个取消边界。
    pub(crate) fn spawn_in_process_group(
        &self,
        command: &mut Command,
        process_group: Option<u32>,
    ) -> std::io::Result<Option<Child>> {
        let mut state = self.state();
        if state.cancelled || state.interrupted {
            return Ok(None);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(process_group.unwrap_or(0) as i32);
        }
        if let Some(process_group) = process_group {
            if state.active_process_group != Some(process_group) {
                return Err(std::io::Error::other(
                    "pipeline process group is no longer active",
                ));
            }
        }
        let child = command.spawn()?;
        if process_group.is_none() {
            state.active_process_group = Some(child.id());
        }
        Ok(Some(child))
    }

    /// 在完整进程组监督结束后注销匹配的活动进程组。
    ///
    /// 仅观察到 leader 退出不足以调用本方法；调用者必须已确认原 PGID
    /// 和捕获管道都已清理，或者已在有界清理后明确返回监督失败。
    ///
    /// # Arguments
    ///
    /// - `process_group`：此前由 [`Self::spawn`] 登记的子进程/进程组 ID。
    pub(crate) fn finish(&self, process_group: u32) {
        let mut state = self.state();
        if state.active_process_group == Some(process_group) {
            state.active_process_group = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn active_process_group(&self) -> Option<u32> {
        self.state().active_process_group
    }
}

static ACTIVE_TASK: LazyLock<Mutex<Option<Weak<CancellationToken>>>> =
    LazyLock::new(|| Mutex::new(None));

/// 将一个取消令牌登记为当前前台 Agent 任务，守卫析构时自动注销。
pub(crate) struct ActiveCancellation {
    token: Arc<CancellationToken>,
}

impl ActiveCancellation {
    /// 把令牌登记为 Ctrl-C 当前唯一可见的 Agent 任务。
    ///
    /// 返回的守卫必须覆盖整个任务生命周期；析构时只注销仍指向同一令牌的登记。
    pub(crate) fn register(token: Arc<CancellationToken>) -> Self {
        let mut active = ACTIVE_TASK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = Some(Arc::downgrade(&token));
        drop(active);
        Self { token }
    }
}

impl Drop for ActiveCancellation {
    fn drop(&mut self) {
        let mut active = ACTIVE_TASK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let belongs_to_this_task = active
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|token| Arc::ptr_eq(&token, &self.token));
        if belongs_to_this_task {
            *active = None;
        }
    }
}

/// Ctrl-C 信号处理器只取消当前已登记的任务。
pub(crate) fn cancel_active() {
    let token = ACTIVE_TASK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(Weak::upgrade);
    if let Some(token) = token {
        token.cancel();
    }
}
