//! `zhsh` 是一个保留 Bash 执行能力、同时支持中文自然语言任务的交互式 Shell。
//!
//! 库入口 [`run`] 使用默认启动策略；[`run_with_options`] 只用于显式进程级选项。
//! 二进制和嵌入调用共享同一套启动、历史、配置恢复与 REPL 生命周期。输入先经过
//! 独立的二元首字符路由：ASCII 开头进入内建命令或 Bash，
//! 非 ASCII 开头进入 LLM Agent。Agent 每个执行阶段严格最多六轮，澄清回答作为追加状态
//! 保留在当前任务中并开启新的六轮阶段；历史始终只保留用户在主提示符提交的原文。
//!
//! # Quick start
//!
//! ```no_run
//! let status = zhsh::run();
//! std::process::exit(status);
//! ```
//!
//! # Architecture decisions
//!
//! - **当前过渡版本不复制 Bash parser。** 命令层只解析足以安全识别内建命令和绑定
//!   Agent 静态目标的字面参数。用户命令仅额外识别“输出型 zhsh 内建命令作为最左
//!   管道源”这一窄边界，右侧及其余展开、重定向和完整语法仍交给 Bash。这不是未来
//!   Native Zhsh Language 的永久限制。
//! - **会话状态只有一个内存所有者。** 环境、目录、别名、函数和当前 LLM 配置由
//!   `SessionState` 保存，执行器在创建子进程时显式投影这些状态。
//! - **内建命令只有一份注册表。** 路由、帮助、补全和 Agent 权限策略共同读取
//!   `command` 模块中的命令描述，具体行为按命令拆分在 `builtin` 中。
//! - **交互、用例和持久化分层。** LLM 向导只收集选择，应用服务完成保存或启用并
//!   返回候选，Shell 再提交内存；store 负责路径约束、私有权限和原子写入。
//! - **取消优先于命令启动。** Agent 取消检查、子进程创建和进程组登记由同一状态锁
//!   串行化，避免 Ctrl-C 与命令启动之间的竞态；已经完成的外部副作用不能回滚。
//! - **Agent 判断与执行绑定同一计划。** 静态外部命令按计划中核验的绝对路径直接启动；
//!   动态 Bash、目标身份不明、越界披露和后台执行形态失败关闭或要求明确确认。
//! - **用户状态根只在启动时确定。** 缺失、非 UTF-8 或非绝对 `HOME` 会禁用用户持久状态，
//!   不会把当前目录当作隐式 HOME；Codec 局部故障只禁用受影响的 Agent 能力。
//! - **网络响应按任务隔离。** 每次 LLM 完成都有独立请求身份和返回通道；异步取消
//!   优先于迟到响应，并立即释放串行请求许可，不让旧响应进入后续任务。
//! - **供应商协议由无能力 Codec 隔离。** 配置固定 Codec 版本与 Base URL；Core 独占
//!   密钥注入、HTTP、响应上限和取消，声明式 Codec 只描述无状态协议转换。
//! - **职责包可独立提取。** `common → llm → application → shell → agent → repl` 保持
//!   单向依赖；`src` 根目录只保留 crate 入口，新实现进入最窄职责包。
//!
//! 终端用法、安全边界和当前支持范围参见项目 README 与发行包中的 `zhsh(1)` 手册。

#![warn(missing_docs)]
#![warn(rustdoc::all)]
#![deny(rustdoc::broken_intra_doc_links)]

mod agent;
mod application;
mod common;
mod llm;
mod repl;
mod shell;

pub use repl::{run, run_with_options, RunOptions};
