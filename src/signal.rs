//! Signals.
//!
//! The running driver answers `SIGUSR1` with a one-shot sync pass. The
//! handler does nothing but set a flag — writing to the socket or a pipe
//! from a signal handler is not safe, and the control loop checks the flag
//! often enough.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set when a sync was requested.
static SYNC_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sync_request(_signal: libc::c_int) {
    SYNC_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install the handler for `SIGUSR1`.
pub fn install_sync_handler() {
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            handle_sync_request as *const () as libc::sighandler_t,
        );
    }
}

/// Take the pending request, if there is one.
pub fn take_sync_request() -> bool {
    SYNC_REQUESTED.swap(false, Ordering::SeqCst)
}

/// Ask the driver that runs with this pid to sync now.
pub fn request_sync(pid: u32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR1) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_set_and_cleared() {
        assert!(!take_sync_request());
        SYNC_REQUESTED.store(true, Ordering::SeqCst);
        assert!(take_sync_request());
        assert!(!take_sync_request());
    }
}
