//! Codec 发布者信任和活动 FORMAT 卸载的终端确认界面。

use crate::application::{
    CodecManagementUi, CodecUninstallDecision, CodecUninstallPrompt, PublisherTrustDecision,
    PublisherTrustPrompt,
};
use crate::common::CancellationToken;
use std::io::{self, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

const ACTIVE_UNINSTALL_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) enum ConfirmationRead {
    Yes,
    No,
    Cancelled,
    TimedOut,
    Unavailable,
}

pub(crate) struct TerminalCodecManagementUi;

impl CodecManagementUi for TerminalCodecManagementUi {
    fn confirm_publisher(
        &self,
        prompt: &PublisherTrustPrompt,
        cancellation: &CancellationToken,
    ) -> PublisherTrustDecision {
        if cancellation.is_cancelled() {
            return PublisherTrustDecision::Cancelled;
        }
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return PublisherTrustDecision::Unavailable;
        }

        eprintln!("已识别 FORMAT：{}", prompt.format);
        eprintln!("Publisher：{}", prompt.publisher);
        eprintln!("Codec SHA-256：{}", prompt.codec_sha256);
        eprintln!("发布公钥指纹：{}", prompt.key_fingerprint);
        eprintln!("! 签名只证明该文件由此密钥签发，不证明发布者身份");
        eprintln!("! 授权后将信任该公钥后续签发的所有用户 Codec；每个制品仍会完整校验");
        if prompt.query_secret_warning {
            eprintln!("! 此 Codec 会把凭据放入 Provider URL 查询参数");
        }
        eprint!("? 是否信任该发布密钥并安装 Codec？[y/N] ");
        let _ = io::stderr().flush();
        let decision = read_confirmation(cancellation, None);
        eprintln!();
        match decision {
            ConfirmationRead::Yes => PublisherTrustDecision::Authorize,
            ConfirmationRead::No => PublisherTrustDecision::Decline,
            ConfirmationRead::Cancelled | ConfirmationRead::TimedOut => {
                PublisherTrustDecision::Cancelled
            }
            ConfirmationRead::Unavailable => PublisherTrustDecision::Unavailable,
        }
    }

    fn confirm_active_uninstall(
        &self,
        prompt: &CodecUninstallPrompt,
        cancellation: &CancellationToken,
    ) -> CodecUninstallDecision {
        if cancellation.is_cancelled() {
            return CodecUninstallDecision::Cancelled;
        }
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return CodecUninstallDecision::Unavailable;
        }

        eprintln!("! 当前活动 LLM 配置正在使用 {}", prompt.format);
        eprintln!(
            "! 卸载后保留配置，但 Agent 将不可用：{}",
            crate::common::terminal_safe_path(&prompt.artifact_path)
        );
        eprint!("? 确认卸载活动 Codec？[y/N] ");
        let _ = io::stderr().flush();
        let decision = read_confirmation(cancellation, Some(ACTIVE_UNINSTALL_TIMEOUT));
        eprintln!();
        match decision {
            ConfirmationRead::Yes => CodecUninstallDecision::Confirm,
            ConfirmationRead::No => CodecUninstallDecision::Decline,
            ConfirmationRead::Cancelled => CodecUninstallDecision::Cancelled,
            ConfirmationRead::TimedOut => CodecUninstallDecision::TimedOut,
            ConfirmationRead::Unavailable => CodecUninstallDecision::Unavailable,
        }
    }
}

#[cfg(unix)]
pub(super) fn read_confirmation(
    cancellation: &CancellationToken,
    timeout: Option<Duration>,
) -> ConfirmationRead {
    use std::os::fd::AsRawFd;

    struct RawGuard {
        fd: i32,
        original: libc::termios,
    }
    impl Drop for RawGuard {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            }
        }
    }

    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return ConfirmationRead::Unavailable;
    }
    let mut raw = original;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return ConfirmationRead::Unavailable;
    }
    let _guard = RawGuard { fd, original };
    let mut answer = Vec::new();
    let deadline = timeout.map(|duration| Instant::now() + duration);
    loop {
        if cancellation.is_cancelled() {
            return ConfirmationRead::Cancelled;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return ConfirmationRead::TimedOut;
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 50) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return ConfirmationRead::Unavailable;
        }
        if ready == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let mut byte = [0_u8; 1];
        match stdin.lock().read(&mut byte) {
            Ok(0) => return ConfirmationRead::Cancelled,
            Ok(_) => match byte[0] {
                b'\r' | b'\n' => {
                    let answer = String::from_utf8_lossy(&answer);
                    return if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        ConfirmationRead::Yes
                    } else {
                        ConfirmationRead::No
                    };
                }
                3 | 4 | 27 => return ConfirmationRead::Cancelled,
                8 | 127 => {
                    answer.pop();
                    eprint!("\x08 \x08");
                    let _ = io::stderr().flush();
                }
                value if !value.is_ascii_control() => {
                    answer.push(value);
                    if value.is_ascii() {
                        eprint!("{}", value as char);
                        let _ = io::stderr().flush();
                    }
                }
                _ => {}
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return ConfirmationRead::Unavailable,
        }
    }
}

#[cfg(not(unix))]
pub(super) fn read_confirmation(
    cancellation: &CancellationToken,
    timeout: Option<Duration>,
) -> ConfirmationRead {
    if cancellation.is_cancelled() {
        return ConfirmationRead::Cancelled;
    }
    if timeout.is_some() {
        return ConfirmationRead::Unavailable;
    }
    let mut answer = String::new();
    match io::stdin().read_line(&mut answer) {
        Ok(0) | Err(_) => ConfirmationRead::Cancelled,
        Ok(_) if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") => {
            ConfirmationRead::Yes
        }
        Ok(_) => ConfirmationRead::No,
    }
}
