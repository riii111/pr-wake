use std::io::{self, Write};

// Every result, usage message and log line goes out through these two handles, so a closed
// stream is the one failure to decide about: nothing is left to tell it to, so it is dropped.
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
