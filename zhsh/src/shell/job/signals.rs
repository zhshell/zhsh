//! HUP only records intent in the handler. The Native supervisor forwards it.
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};
static HUP: AtomicBool = AtomicBool::new(false);
extern "C" fn hup(_: libc::c_int) {
    HUP.store(true, Ordering::Relaxed);
}
pub(super) fn take_hup() -> bool {
    HUP.swap(false, Ordering::Relaxed)
}
pub(super) fn install() -> io::Result<()> {
    // SAFETY: initialized sigaction with an async-signal-safe handler.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = hup as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut()) < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

static INTERRUPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) fn interrupt() {
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);
}
pub(super) fn interrupt_epoch() -> u64 {
    INTERRUPTS.load(Ordering::Relaxed)
}
