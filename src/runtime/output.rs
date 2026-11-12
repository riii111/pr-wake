use std::io::{self, Write};

// A failed write to a closed stream leaves nowhere to report it, so the error is dropped.
pub(crate) fn write_stdout(text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
}

pub(crate) fn write_stderr(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr
        .write_all(text.as_bytes())
        .and_then(|()| stderr.flush());
}
