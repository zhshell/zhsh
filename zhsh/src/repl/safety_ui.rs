//! Safety 批量覆盖的终端确认界面。

use super::codec_ui::{read_confirmation, ConfirmationRead};
use crate::common::CancellationToken;
use crate::shell::{SafetyManagementUi, SafetyOverwriteDecision, SafetyOverwritePrompt};
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

const OVERWRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct TerminalSafetyManagementUi;

impl SafetyManagementUi for TerminalSafetyManagementUi {
    fn confirm_overwrite(
        &self,
        prompt: &SafetyOverwritePrompt,
        cancellation: &CancellationToken,
    ) -> SafetyOverwriteDecision {
        if cancellation.is_cancelled() {
            return SafetyOverwriteDecision::Cancelled;
        }
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return SafetyOverwriteDecision::Unavailable;
        }
        eprintln!(
            "Safety install: {} new, {} conflict",
            prompt.new_rules, prompt.conflicts
        );
        eprint!("? 覆盖全部冲突规则并继续？[y/N] ");
        let _ = io::stderr().flush();
        let decision = read_confirmation(cancellation, Some(OVERWRITE_TIMEOUT));
        eprintln!();
        match decision {
            ConfirmationRead::Yes => SafetyOverwriteDecision::Confirm,
            ConfirmationRead::No => SafetyOverwriteDecision::Decline,
            ConfirmationRead::Cancelled => SafetyOverwriteDecision::Cancelled,
            ConfirmationRead::TimedOut => SafetyOverwriteDecision::TimedOut,
            ConfirmationRead::Unavailable => SafetyOverwriteDecision::Unavailable,
        }
    }
}
