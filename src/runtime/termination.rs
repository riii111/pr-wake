use std::{io, sync::OnceLock};

use crate::infra::stop::StopRequest;

static STOP: OnceLock<StopRequest> = OnceLock::new();

// launchd stops an agent with SIGTERM and a terminal with SIGINT. The handler only records the
// request; every wait observes it and shuts down through the same path as a timeout.
pub(crate) fn install(stop: StopRequest) -> io::Result<()> {
    STOP.set(stop)
        .map_err(|_| io::Error::other("termination handling is already installed"))?;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        install_recorder(signal)?;
    }
    Ok(())
}

fn install_recorder(signal: libc::c_int) -> io::Result<()> {
    extern "C" fn record(_signal: libc::c_int) {
        // `OnceLock::get` is an atomic load and the request an atomic store, so neither can
        // deadlock or allocate inside the handler.
        if let Some(stop) = STOP.get() {
            stop.request();
        }
    }

    // SAFETY: a zeroed sigaction is a valid output buffer for querying the current disposition.
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: a null new action only reads the current disposition into `previous`.
    if unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut previous) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // A signal ignored by the parent (for example under nohup) stays ignored.
    if previous.sa_sigaction == libc::SIG_IGN {
        return Ok(());
    }
    // SAFETY: a zeroed sigaction has no flags; the mask is cleared below before use.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = record as *const () as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: the mask belongs to the local action and the handler only performs atomic operations.
    let result = unsafe {
        libc::sigemptyset(&raw mut action.sa_mask);
        libc::sigaction(signal, &raw const action, std::ptr::null_mut())
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The only test that installs the recorder, since a process can install it once.
    #[test]
    fn sigterm_requests_a_stop_once_installed() {
        let stop = StopRequest::default();
        install(stop.clone()).expect("install the termination recorder");

        // SAFETY: the recorder is installed, so the signal only sets the stop request.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);

        assert!(stop.is_requested());
        assert!(install(StopRequest::default()).is_err());
    }
}
