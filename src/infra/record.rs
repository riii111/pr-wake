use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt::{self, Display, Formatter},
    fs::{self, DirBuilder, File, OpenOptions, TryLockError},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::ser::PrettyFormatter;
use serde_path_to_error::Track;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseDirectories {
    config_home: PathBuf,
    state_home: PathBuf,
}

// Each feature keeps the layout its records already have, so old and new files compare line by
// line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonLayout {
    // `JSON.stringify(value, null, 1)`, as the watch records are written.
    OneSpace,
    // Go's `json.MarshalIndent(value, "", "  ")` and a newline, as the review state is written.
    TwoSpacesAndNewline,
}

// The kernel releases the lock when the file closes, so a crashed holder never leaves it behind.
#[derive(Debug)]
pub(crate) struct ProcessLock {
    _file: File,
}

#[derive(Debug)]
pub(crate) struct RecordError {
    path: PathBuf,
    pub(crate) kind: RecordErrorKind,
}

#[derive(Debug)]
pub(crate) enum RecordErrorKind {
    Read(io::Error),
    // The error names the damaged field, so whoever reads it knows what to repair.
    Invalid(serde_path_to_error::Error<serde_json::Error>),
    Write(io::Error),
    Lock(io::Error),
}

const OWNER_ONLY_FILE: u32 = 0o600;
const OWNER_ONLY_DIRECTORY: u32 = 0o700;

impl BaseDirectories {
    #[must_use]
    pub(crate) fn from_environment() -> Option<Self> {
        Self::resolve(
            env::home_dir().as_deref(),
            env::var_os("XDG_CONFIG_HOME"),
            env::var_os("XDG_STATE_HOME"),
        )
    }

    #[must_use]
    pub(crate) fn config_home(&self) -> &Path {
        &self.config_home
    }

    #[must_use]
    pub(crate) fn state_home(&self) -> &Path {
        &self.state_home
    }

    fn resolve(
        home: Option<&Path>,
        config_home: Option<OsString>,
        state_home: Option<OsString>,
    ) -> Option<Self> {
        // A relative directory is ignored, as the XDG spec has it, so the working directory never
        // picks the config or the state.
        let absolute =
            |value: Option<OsString>| value.map(PathBuf::from).filter(|path| path.is_absolute());
        let under_home = |relative: &str| home.map(|home| home.join(relative));
        Some(Self {
            config_home: absolute(config_home).or_else(|| under_home(".config"))?,
            state_home: absolute(state_home).or_else(|| under_home(".local/state"))?,
        })
    }
}

// A missing file is `None`; a file that exists but does not hold a `T` is an error, never an empty
// record.
pub(crate) fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, RecordError> {
    let Some(content) = read_regular_file(path)
        .map_err(|error| RecordError::new(path, RecordErrorKind::Read(error)))?
    else {
        return Ok(None);
    };
    decode(&content)
        .map(Some)
        .map_err(|error| RecordError::new(path, RecordErrorKind::Invalid(error)))
}

pub(crate) fn save_json(
    path: &Path,
    value: &impl Serialize,
    layout: JsonLayout,
) -> Result<(), RecordError> {
    encode(value, layout)
        .map_err(io::Error::from)
        .and_then(|content| write_atomically(path, &content))
        .map_err(|error| RecordError::new(path, RecordErrorKind::Write(error)))
}

// `None` while another holder has the lock.
pub(crate) fn hold_lock(path: &Path) -> Result<Option<ProcessLock>, RecordError> {
    acquire_lock(path).map_err(|error| RecordError::new(path, RecordErrorKind::Lock(error)))
}

fn read_regular_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Opening a FIFO or a device could wait forever, so only a regular file is opened.
    if !metadata.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    fs::read(path).map(Some)
}

fn decode<T: DeserializeOwned>(
    content: &[u8],
) -> Result<T, serde_path_to_error::Error<serde_json::Error>> {
    let mut deserializer = serde_json::Deserializer::from_slice(content);
    let value = serde_path_to_error::deserialize(&mut deserializer)?;
    deserializer
        .end()
        .map_err(|error| serde_path_to_error::Error::new(Track::new().path(), error))?;
    Ok(value)
}

fn encode(value: &impl Serialize, layout: JsonLayout) -> serde_json::Result<Vec<u8>> {
    let indent: &[u8] = match layout {
        JsonLayout::OneSpace => b" ",
        JsonLayout::TwoSpacesAndNewline => b"  ",
    };
    let mut content = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut content, PrettyFormatter::with_indent(indent));
    value.serialize(&mut serializer)?;
    if layout == JsonLayout::TwoSpacesAndNewline {
        content.push(b'\n');
    }
    Ok(content)
}

fn write_atomically(path: &Path, content: &[u8]) -> io::Result<()> {
    let directory = parent(path)?;
    prepare_directory(directory)?;
    let (temporary, mut file) = create_temporary(path)?;
    // The rename happens only after the content is on disk, so readers see either the previous
    // or the new content.
    let replaced = file
        .write_all(content)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            fs::rename(&temporary, path)
        });
    if let Err(error) = replaced {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    // The rename itself survives a crash only once the directory is synced.
    sync_directory(directory)
}

fn create_temporary(path: &Path) -> io::Result<(PathBuf, File)> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the record path has no file name"))?;
    // The process ID keeps two processes apart and the sequence two writes in one process; a
    // file left by an earlier process with the same ID only moves on to the next number.
    loop {
        let mut temporary = OsString::from(".");
        temporary.push(name);
        temporary.push(format!(
            ".{}.{}.tmp",
            process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = path.with_file_name(temporary);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(OWNER_ONLY_FILE)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn acquire_lock(path: &Path) -> io::Result<Option<ProcessLock>> {
    prepare_directory(parent(path)?)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(OWNER_ONLY_FILE)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(error)) => return Err(error),
    }
    // The file is emptied only once the lock is held, so a waiting process never clears the
    // holder's PID.
    file.set_len(0)?;
    writeln!(file, "{}", process::id())?;
    Ok(Some(ProcessLock { _file: file }))
}

fn prepare_directory(directory: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(OWNER_ONLY_DIRECTORY)
        .create(directory)?;
    // A new directory entry that never reached the disk would lose the files created inside.
    directory.parent().map_or(Ok(()), sync_directory)
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::other("the record path has no parent directory"))
}

impl RecordError {
    fn new(path: &Path, kind: RecordErrorKind) -> Self {
        Self {
            path: path.to_owned(),
            kind,
        }
    }
}

impl Display for RecordError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.kind {
            RecordErrorKind::Read(error) => write!(formatter, "cannot read {path}: {error}"),
            RecordErrorKind::Invalid(error) => write!(formatter, "{path} is not valid: {error}"),
            RecordErrorKind::Write(error) => write!(formatter, "cannot write {path}: {error}"),
            RecordErrorKind::Lock(error) => write!(formatter, "cannot lock {path}: {error}"),
        }
    }
}

impl Error for RecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            RecordErrorKind::Read(error)
            | RecordErrorKind::Write(error)
            | RecordErrorKind::Lock(error) => Some(error),
            RecordErrorKind::Invalid(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, process::Command};

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Record {
        status: String,
        seen: Vec<String>,
        pending: Vec<Pending>,
        summary: Option<String>,
    }

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Pending {
        #[serde(rename = "type")]
        kind: String,
        head: String,
    }

    struct DirectoryCase {
        name: &'static str,
        home: Option<&'static str>,
        config_home: Option<&'static str>,
        state_home: Option<&'static str>,
        expected: Option<(&'static str, &'static str)>,
    }

    #[test]
    fn base_directories_take_absolute_xdg_values_and_otherwise_home() {
        for case in [
            DirectoryCase {
                name: "absolute_xdg",
                home: Some("/Users/me"),
                config_home: Some("/conf"),
                state_home: Some("/st"),
                expected: Some(("/conf", "/st")),
            },
            DirectoryCase {
                name: "relative_and_empty_xdg",
                home: Some("/Users/me"),
                config_home: Some("relative"),
                state_home: Some(""),
                expected: Some(("/Users/me/.config", "/Users/me/.local/state")),
            },
            DirectoryCase {
                name: "unset_xdg",
                home: Some("/Users/me"),
                config_home: None,
                state_home: None,
                expected: Some(("/Users/me/.config", "/Users/me/.local/state")),
            },
            DirectoryCase {
                name: "absolute_xdg_without_home",
                home: None,
                config_home: Some("/conf"),
                state_home: Some("/st"),
                expected: Some(("/conf", "/st")),
            },
            DirectoryCase {
                name: "config_without_home",
                home: None,
                config_home: None,
                state_home: Some("/st"),
                expected: None,
            },
        ] {
            let resolved = BaseDirectories::resolve(
                case.home.map(Path::new),
                case.config_home.map(OsString::from),
                case.state_home.map(OsString::from),
            );

            let expected = case.expected.map(|(config, state)| BaseDirectories {
                config_home: PathBuf::from(config),
                state_home: PathBuf::from(state),
            });
            assert_eq!(resolved, expected, "case: {}", case.name);
        }
    }

    #[test]
    fn process_environment_gives_absolute_directories() {
        let directories = BaseDirectories::from_environment().expect("HOME is set for tests");

        assert!(directories.config_home().is_absolute());
        assert!(directories.state_home().is_absolute());
    }

    #[test]
    fn layouts_match_the_files_each_feature_already_writes() {
        let one_space = "{\n \"status\": \"watching\",\n \"seen\": [],\n \"pending\": [\n  {\n   \"type\": \"conflict\",\n   \"head\": \"abc\"\n  }\n ],\n \"summary\": null\n}";
        let two_spaces = "{\n  \"status\": \"watching\",\n  \"seen\": [],\n  \"pending\": [\n    {\n      \"type\": \"conflict\",\n      \"head\": \"abc\"\n    }\n  ],\n  \"summary\": null\n}\n";
        for (name, layout, expected) in [
            ("json_stringify_one_space", JsonLayout::OneSpace, one_space),
            (
                "go_marshal_indent",
                JsonLayout::TwoSpacesAndNewline,
                two_spaces,
            ),
        ] {
            let directory = tempfile::tempdir().expect("create a directory");
            let path = directory.path().join("state.json");

            save_json(&path, &record("watching"), layout).expect("save the record");

            let written = fs::read_to_string(&path).expect("read the record");
            assert_eq!(written, expected, "case: {name}");
        }
    }

    #[test]
    fn saved_record_is_private_and_leaves_no_temporary_file() {
        let root = tempfile::tempdir().expect("create a directory");
        let directory = root.path().join("watch").join("62ba7cefd0fd7837");
        let path = directory.join("state.json");

        save_json(&path, &record("watching"), JsonLayout::OneSpace).expect("save the record");

        assert_eq!(mode(&directory), OWNER_ONLY_DIRECTORY);
        assert_eq!(mode(&path), OWNER_ONLY_FILE);
        let entries = fs::read_dir(&directory)
            .expect("list the directory")
            .map(|entry| entry.expect("read an entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, ["state.json"]);
    }

    #[test]
    fn saving_again_replaces_the_record() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("state.json");
        save_json(&path, &record("watching"), JsonLayout::OneSpace).expect("save the record");

        save_json(&path, &record("ended"), JsonLayout::OneSpace).expect("save it again");

        let loaded = load_json::<Record>(&path).expect("load the record");
        assert_eq!(loaded, Some(record("ended")));
    }

    #[test]
    fn missing_record_loads_as_none() {
        let directory = tempfile::tempdir().expect("create a directory");

        let loaded = load_json::<Record>(&directory.path().join("state.json"));

        assert!(matches!(loaded, Ok(None)), "{loaded:?}");
    }

    #[test]
    fn record_without_the_expected_shape_is_invalid() {
        for (name, content) in [
            ("not_json", "{"),
            ("json_null", "null"),
            (
                "wrong_field_type",
                r#"{"status":1,"seen":[],"pending":[],"summary":null}"#,
            ),
            (
                "trailing_content",
                r#"{"status":"watching","seen":[],"pending":[],"summary":null} {}"#,
            ),
        ] {
            let directory = tempfile::tempdir().expect("create a directory");
            let path = directory.path().join("state.json");
            fs::write(&path, content).expect("write the record");

            let loaded = load_json::<Record>(&path);

            assert!(
                matches!(
                    &loaded,
                    Err(RecordError {
                        kind: RecordErrorKind::Invalid(_),
                        ..
                    })
                ),
                "case: {name}: {loaded:?}"
            );
        }
    }

    #[test]
    fn invalid_record_error_names_the_damaged_field() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("state.json");
        fs::write(
            &path,
            r#"{"status":"watching","seen":[],"pending":[{"type":1}],"summary":null}"#,
        )
        .expect("write the record");

        let loaded = load_json::<Record>(&path);

        let error = loaded.expect_err("the record is refused").to_string();
        assert!(error.contains("pending[0].type"), "{error}");
    }

    #[test]
    fn fifo_is_refused_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("state.json");
        let made = Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("run mkfifo");
        assert!(made.success());

        let loaded = load_json::<Record>(&path);

        assert!(
            matches!(
                &loaded,
                Err(RecordError {
                    kind: RecordErrorKind::Read(_),
                    ..
                })
            ),
            "{loaded:?}"
        );
    }

    #[test]
    fn lock_is_exclusive_until_its_holder_drops_it() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("watch").join("watch.lock");

        let first = hold_lock(&path)
            .expect("lock")
            .expect("the first holder gets the lock");
        let second = hold_lock(&path).expect("try the lock again");
        drop(first);
        let third = hold_lock(&path).expect("lock after the release");

        assert!(second.is_none());
        assert!(third.is_some());
        let holder = fs::read_to_string(&path).expect("read the lock file");
        assert_eq!(holder, format!("{}\n", process::id()));
    }

    fn record(status: &str) -> Record {
        Record {
            status: status.to_owned(),
            seen: Vec::new(),
            pending: vec![Pending {
                kind: "conflict".to_owned(),
                head: "abc".to_owned(),
            }],
            summary: None,
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("read the metadata")
            .permissions()
            .mode()
            & 0o777
    }
}
