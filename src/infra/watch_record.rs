use std::{
    collections::HashSet,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::record::{self, BaseDirectories, JsonLayout, ProcessLock, RecordError};
use crate::app::{
    time,
    watch::{
        config::WatchConfig,
        pr::{PrLink, PrRef},
        state::WatchState,
    },
};

// The record directory is shared with harnexus-crew, which reads none of the PR states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchPaths {
    pub(crate) config: PathBuf,
    pub(crate) state: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredLink {
    repo: String,
    number: u64,
    url: String,
    worker: String,
    #[serde(with = "time::record")]
    linked_at: Timestamp,
}

#[derive(Serialize)]
struct StoredSender<'record> {
    thread: &'record str,
    at: &'record str,
}

const LINK: &str = "link.json";
const STATE: &str = "state.json";
const SENDER: &str = "sender.json";
const LOCK: &str = "watch.lock";

impl WatchPaths {
    #[must_use]
    pub(crate) fn of(directories: &BaseDirectories) -> Self {
        Self {
            config: directories
                .config_home()
                .join("harnexus")
                .join("pr-watch.json"),
            state: directories.state_home().join("harnexus-crew").join("watch"),
        }
    }
}

pub(crate) fn load_config(path: &Path) -> Result<WatchConfig, RecordError> {
    record::load_required_json(path)
}

pub(crate) fn list_links(root: &Path, skip: &HashSet<String>) -> Result<Vec<PrLink>, RecordError> {
    let names = record::list_directory(root)?.unwrap_or_default();
    Ok(names
        .iter()
        .filter_map(|name| name.to_str())
        .filter(|key| !skip.contains(*key) && root.join(key).is_dir())
        // An unreadable link is skipped, so one broken record cannot stop the watch of every other PR.
        .filter_map(|key| read_link(&root.join(key).join(LINK)).ok().flatten())
        .collect())
}

// `None` for a PR no chat has linked, or whose link does not hold one.
pub(crate) fn load_link(root: &Path, pr: &PrRef) -> Result<Option<PrLink>, RecordError> {
    read_link(&pr_directory(root, pr).join(LINK))
}

pub(crate) fn record_link(root: &Path, link: &PrLink) -> Result<(), RecordError> {
    let stored = StoredLink {
        repo: link.pr.repo.clone(),
        number: link.pr.number,
        url: link.url.clone(),
        worker: link.worker.clone(),
        linked_at: link.linked_at,
    };
    // The running watch alone writes a PR's state and notices the new chat itself, so only the link is written.
    record::save_json(
        &pr_directory(root, &link.pr).join(LINK),
        &stored,
        JsonLayout::OneSpace,
    )
}

pub(crate) fn record_sender(root: &Path, thread: &str, at: &str) -> Result<(), RecordError> {
    record::save_json(
        &root.join(SENDER),
        &StoredSender { thread, at },
        JsonLayout::OneSpace,
    )
}

pub(crate) fn load_sender(root: &Path) -> Result<Option<String>, RecordError> {
    let recorded = record::load_json::<Value>(&root.join(SENDER))?;
    Ok(recorded.and_then(|recorded| recorded.get("thread")?.as_str().map(str::to_owned)))
}

// `None` while another watch holds it.
pub(crate) fn hold_watch_lock(root: &Path) -> Result<Option<ProcessLock>, RecordError> {
    record::hold_lock(&root.join(LOCK))
}

// A PR the watch has not read yet starts from a fresh state.
pub(crate) fn load_watch_state(root: &Path, pr: &PrRef) -> Result<WatchState, RecordError> {
    // A damaged record is refused rather than read as empty, so the next save never resets its tracking.
    record::load_json(&state_path(root, pr)).map(Option::unwrap_or_default)
}

pub(crate) fn save_watch_state(
    root: &Path,
    pr: &PrRef,
    state: &WatchState,
) -> Result<(), RecordError> {
    record::save_json(&state_path(root, pr), state, JsonLayout::OneSpace)
}

pub(crate) fn pr_key(pr: &PrRef) -> String {
    let digest = Sha256::digest(format!("{}#{}", pr.repo.to_lowercase(), pr.number));
    digest
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut key, byte| {
            let _ = write!(key, "{byte:02x}");
            key
        })
}

fn state_path(root: &Path, pr: &PrRef) -> PathBuf {
    pr_directory(root, pr).join(STATE)
}

fn pr_directory(root: &Path, pr: &PrRef) -> PathBuf {
    root.join(pr_key(pr))
}

// A stored value of another shape is no link, but a file that is not JSON is an error.
fn read_link(path: &Path) -> Result<Option<PrLink>, RecordError> {
    let recorded = record::load_json::<Value>(path)?;
    Ok(recorded
        .filter(Value::is_object)
        .and_then(|recorded| serde_json::from_value::<StoredLink>(recorded).ok())
        .map(|stored| PrLink {
            pr: PrRef {
                repo: stored.repo,
                number: stored.number,
            },
            url: stored.url,
            worker: stored.worker,
            linked_at: stored.linked_at,
        }))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::{
        app::watch::state::{WatchEvent, WatchStatus, bot_key},
        infra::record::RecordErrorKind,
    };

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn pr_without_a_record_starts_fresh() {
        let root = tempfile::tempdir().expect("create a directory");

        let loaded = load_watch_state(root.path(), &pay(7)).expect("load the state");

        assert_eq!(loaded, WatchState::default());
    }

    #[test]
    fn saved_state_is_written_with_one_space_and_read_back_intact() {
        let root = tempfile::tempdir().expect("create a directory");
        let saved = WatchState {
            status: WatchStatus::Halted,
            reason: Some("the send could not be confirmed".to_owned()),
            seen: vec![bot_key("coderabbit"), format!("coderabbit:{HEAD}")],
            pending: vec![WatchEvent::Conflict {
                head: HEAD.to_owned(),
            }],
            worker: Some("worker".to_owned()),
            halted: json!({ "at": "2026-10-23T02:00:00.000Z", "prompt": "go", "answer": null })
                .as_object()
                .cloned(),
            ..WatchState::default()
        };

        save_watch_state(root.path(), &pay(7), &saved).expect("save the state");

        let path = root.path().join("02bff83354bf169d").join("state.json");
        let written = fs::read_to_string(path).expect("read the record");
        assert!(
            written.starts_with("{\n \"status\": \"halted\",\n"),
            "{written}"
        );
        let loaded = load_watch_state(root.path(), &pay(7)).expect("load the state");
        assert_eq!(loaded, saved);
    }

    #[rstest]
    #[case::empty_object(json!({}))]
    #[case::unknown_status(with(tracked(), "status", json!("sleeping")))]
    #[case::pending_event_of_unknown_type(with(
        tracked(),
        "pending",
        json!([{ "type": "merged", "head": HEAD }])
    ))]
    #[case::missing_reason(without(tracked(), "reason"))]
    #[case::unreadable_time(with(tracked(), "checkedAt", json!("soon")))]
    fn damaged_record_is_refused_and_left_on_disk(#[case] record: Value) {
        let root = tempfile::tempdir().expect("create a directory");
        let directory = root.path().join(pr_key(&pay(7)));
        fs::create_dir(&directory).expect("create the PR directory");
        let path = directory.join("state.json");
        let content = serde_json::to_string_pretty(&record).expect("write the record");
        fs::write(&path, &content).expect("write the record");

        let loaded = load_watch_state(root.path(), &pay(7));

        assert!(
            matches!(
                &loaded,
                Err(RecordError {
                    kind: RecordErrorKind::Invalid(_),
                    ..
                })
            ),
            "{loaded:?}"
        );
        assert_eq!(fs::read_to_string(&path).expect("read the record"), content);
    }

    #[test]
    fn link_is_written_with_one_space_and_read_back() {
        let root = tempfile::tempdir().expect("create a directory");
        let link = link_of(7, "worker");

        record_link(root.path(), &link).expect("record the link");

        let written = fs::read_to_string(root.path().join("02bff83354bf169d").join(LINK))
            .expect("read the link");
        assert_eq!(
            written,
            "{\n \"repo\": \"org/pay\",\n \"number\": 7,\n \"url\": \"https://github.com/org/pay/pull/7\",\n \"worker\": \"worker\",\n \"linkedAt\": \"2026-10-20T00:00:00.000Z\"\n}"
        );
        let mixed_case = PrRef {
            repo: "ORG/Pay".to_owned(),
            number: 7,
        };
        assert_eq!(
            load_link(root.path(), &mixed_case).expect("load"),
            Some(link)
        );
        assert_eq!(load_link(root.path(), &pay(8)).expect("load"), None);
    }

    #[test]
    fn listing_skips_unreadable_links_skipped_keys_and_stray_files() {
        let root = tempfile::tempdir().expect("create a directory");
        for number in [7, 8, 9, 12] {
            record_link(root.path(), &link_of(number, "worker")).expect("record the link");
        }
        let broken = root.path().join(pr_key(&pay(10)));
        fs::create_dir(&broken).expect("create the PR directory");
        fs::write(broken.join(LINK), "{").expect("write a broken link");
        let other_shape = root.path().join(pr_key(&pay(11)));
        fs::create_dir(&other_shape).expect("create the PR directory");
        fs::write(other_shape.join(LINK), r#"["org/pay", 11]"#).expect("write a list");
        // harnexus-crew shares the directory and may add fields this version does not know.
        let later = root.path().join(pr_key(&pay(12))).join(LINK);
        let mut recorded: Value =
            serde_json::from_str(&fs::read_to_string(&later).expect("read the link"))
                .expect("parse the link");
        recorded["addedLater"] = json!(true);
        fs::write(&later, recorded.to_string()).expect("add a field");
        fs::write(root.path().join(SENDER), r#"{"thread":"s"}"#).expect("write the sender");
        let skip = HashSet::from([pr_key(&pay(8))]);

        let links = list_links(root.path(), &skip).expect("list the links");

        let mut numbers = links.iter().map(|link| link.pr.number).collect::<Vec<_>>();
        numbers.sort_unstable();
        assert_eq!(numbers, [7, 9, 12]);
    }

    #[test]
    fn missing_record_directory_lists_no_link() {
        let root = tempfile::tempdir().expect("create a directory");

        let links = list_links(&root.path().join("watch"), &HashSet::new());

        assert_eq!(links.expect("list the links"), []);
    }

    #[test]
    fn recorded_sender_is_read_back_and_a_damaged_one_refused() {
        let root = tempfile::tempdir().expect("create a directory");
        let missing = load_sender(root.path()).expect("load without a record");

        record_sender(root.path(), "sender-chat", "2026-10-19T09:00:00.000Z")
            .expect("record the sender");
        let recorded = load_sender(root.path()).expect("load the sender");
        fs::write(root.path().join(SENDER), "{").expect("damage the sender");
        let damaged = load_sender(root.path());

        assert_eq!(missing, None);
        assert_eq!(recorded.as_deref(), Some("sender-chat"));
        assert!(damaged.is_err(), "{damaged:?}");
    }

    #[test]
    fn config_is_required_and_read_with_its_paths() {
        let directories = BaseDirectories::from_environment().expect("HOME is set for tests");
        let paths = WatchPaths::of(&directories);
        let root = tempfile::tempdir().expect("create a directory");
        let path = root.path().join("pr-watch.json");
        let missing = load_config(&path);
        fs::write(&path, r#"{"repositories":{"Org/Pay":{"account":"owner"}}}"#)
            .expect("write the config");

        let config = load_config(&path).expect("load the config");

        assert!(paths.config.ends_with("harnexus/pr-watch.json"));
        assert!(paths.state.ends_with("harnexus-crew/watch"));
        assert!(
            matches!(
                missing,
                Err(RecordError {
                    kind: RecordErrorKind::Read(_),
                    ..
                })
            ),
            "{missing:?}"
        );
        assert_eq!(config.account_for("org/pay"), Some("owner"));
    }

    fn link_of(number: u64, worker: &str) -> PrLink {
        PrLink {
            pr: pay(number),
            url: format!("https://github.com/org/pay/pull/{number}"),
            worker: worker.to_owned(),
            linked_at: "2026-10-20T00:00:00Z".parse().expect("parse the time"),
        }
    }

    fn pay(number: u64) -> PrRef {
        PrRef {
            repo: "org/pay".to_owned(),
            number,
        }
    }

    // A state that has been tracking the PR, so a refused record is one whose reset would lose something.
    fn tracked() -> Value {
        json!({
            "status": "watching",
            "reason": null,
            "summary": {
                "state": "OPEN",
                "isDraft": false,
                "head": HEAD,
                "updatedAt": "2026-10-23T01:00:00Z",
                "mergeable": "MERGEABLE",
                "bots": { "coderabbit": null },
                "rollup": "SUCCESS",
            },
            "checkedAt": null,
            "changedAt": null,
            "seen": [bot_key("coderabbit"), "inline:1"],
            "pending": [],
            "followUp": [],
            "conflictHead": null,
            "delivered": null,
            "worker": null,
            "halted": null,
        })
    }

    fn with(mut record: Value, key: &str, value: Value) -> Value {
        record[key] = value;
        record
    }

    fn without(mut record: Value, key: &str) -> Value {
        if let Some(record) = record.as_object_mut() {
            record.remove(key);
        }
        record
    }
}
