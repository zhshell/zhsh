# zhsh local patch

Upstream: rustyline 18.0.1 (MIT), copied from the locked Cargo registry artifact.

`src/tty/unix.rs`: consume buffered input before blocking in `select`. This fixes
batched and UTF-8 input with `ExternalPrinter`; the `signal-hook` feature remains
disabled. Covered by zhsh terminal_input and native_job_control PTY tests.

Remove this override when an upstream release includes the fix and those tests pass.

Independent reproduction and rationale: `docs/features/native-job-control/05-rustyline-buffered-input-report.md`
in the zhsh repository (including upstream/patched PTY results).
