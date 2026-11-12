use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::infra::stop::StopRequest;

// Tells a newer program installed over the running one, so the agent can end and let launchd
// start the new one.
#[derive(Debug)]
pub(crate) struct ProgramWatch {
    path: PathBuf,
    started: FileIdentity,
}

// The device and inode tell a file swapped in at the path, as an install does, from one changed
// in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_ms: i64,
}

static STOP: OnceLock<StopRequest> = OnceLock::new();

impl ProgramWatch {
    // `None` when the program cannot be inspected, so the caller can say so once rather than run
    // stale code without a word.
    #[must_use]
    pub(crate) fn new(path: PathBuf) -> Option<Self> {
        let started = identity(&path)?;
        Some(Self { path, started })
    }

    // A program missing for now is being reinstalled, and there is nothing to start until it is
    // back.
    #[must_use]
    pub(crate) fn is_replaced(&self) -> bool {
        identity(&self.path).is_some_and(|now| now != self.started)
    }
}

fn identity(path: &Path) -> Option<FileIdentity> {
    let found = fs::metadata(path).ok().filter(fs::Metadata::is_file)?;
    Some(FileIdentity {
        device: found.dev(),
        inode: found.ino(),
        size: found.size(),
        modified_ms: found.mtime() * 1000 + found.mtime_nsec() / 1_000_000,
    })
}

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

    #[test]
    fn program_swapped_in_at_its_path_is_told_and_one_left_alone_is_not() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("pr-wake-engine");
        fs::write(&path, "old").expect("write the program");
        let watch = ProgramWatch::new(path.clone()).expect("inspect the program");
        let before = watch.is_replaced();

        fs::write(directory.path().join("next"), "old").expect("write the next program");
        fs::rename(directory.path().join("next"), &path).expect("install it");

        assert_eq!((before, watch.is_replaced()), (false, true));
    }

    #[test]
    fn missing_program_is_waited_for() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("pr-wake-engine");
        fs::write(&path, "old").expect("write the program");
        let watch = ProgramWatch::new(path.clone()).expect("inspect the program");

        fs::remove_file(&path).expect("remove the program");

        assert!(!watch.is_replaced());
    }

    #[test]
    fn program_that_cannot_be_inspected_is_not_watched() {
        let directory = tempfile::tempdir().expect("create a directory");

        let watch = ProgramWatch::new(directory.path().join("pr-wake-engine"));

        assert!(watch.is_none());
    }

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
