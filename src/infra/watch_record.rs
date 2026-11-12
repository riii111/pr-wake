use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use super::record::{self, JsonLayout, RecordError};
use crate::app::watch::{pr::PrRef, state::WatchState};

// A PR the watch has not read yet starts from a fresh state; a damaged record is refused rather
// than read as empty, so the tracking in it is never reset by the next save.
pub(crate) fn load_watch_state(root: &Path, pr: &PrRef) -> Result<WatchState, RecordError> {
    record::load_json(&state_path(root, pr)).map(Option::unwrap_or_default)
}

pub(crate) fn save_watch_state(
    root: &Path,
    pr: &PrRef,
    state: &WatchState,
) -> Result<(), RecordError> {
    record::save_json(&state_path(root, pr), state, JsonLayout::OneSpace)
}

// Keyed by the PR, so the latest worker to name it is the one told about it.
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
    root.join(pr_key(pr)).join("state.json")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rstest::rstest;
    use serde_json::{Value, json};

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

    #[test]
    fn keys_match_the_directories_earlier_watches_wrote() {
        for (repo, number, expected) in [
            ("org/pay", 7, "02bff83354bf169d"),
            ("org/pay", 8, "3d0a4555db96a9b0"),
            ("ORG/Pay", 9, "6db070b27f9194fb"),
            ("org/pay", 10, "e4fa9c7ae05e70a7"),
            ("riii111/pr-wake", 10, "62ba7cefd0fd7837"),
        ] {
            let pr = PrRef {
                repo: repo.to_owned(),
                number,
            };

            assert_eq!(pr_key(&pr), expected, "case: {repo}#{number}");
        }
    }

    #[test]
    fn records_harnexus_task_left_load_from_their_directories() {
        let root = tempfile::tempdir().expect("create a directory");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-watch");
        for number in [7, 8, 9, 10] {
            let key = pr_key(&pay(number));
            fs::create_dir(root.path().join(&key)).expect("create the PR directory");
            fs::copy(
                fixtures.join(&key).join("state.json"),
                root.path().join(&key).join("state.json"),
            )
            .expect("copy the record");
        }

        let statuses = [7, 8, 9, 10].map(|number| {
            load_watch_state(root.path(), &pay(number))
                .map(|state| (state.status, state.pending.len(), state.worker))
                .expect("load the record")
        });

        assert_eq!(
            statuses,
            [
                (WatchStatus::Watching, 0, Some("old-chat".to_owned())),
                (WatchStatus::Watching, 0, Some("w8".to_owned())),
                (WatchStatus::Watching, 1, Some("w9".to_owned())),
                (WatchStatus::Halted, 0, Some("w10".to_owned())),
            ]
        );
    }

    #[rstest]
    #[case::json_null(json!(null))]
    #[case::json_array(json!([tracked()]))]
    #[case::empty_object(json!({}))]
    #[case::unknown_status(with(tracked(), "status", json!("sleeping")))]
    #[case::seen_not_a_list(with(tracked(), "seen", json!("inline:1")))]
    #[case::pending_event_of_unknown_type(with(
        tracked(),
        "pending",
        json!([{ "type": "merged", "head": HEAD }])
    ))]
    #[case::pending_finding_without_an_id(with(
        tracked(),
        "pending",
        json!([{ "type": "finding", "finding": { "kind": "inline" } }])
    ))]
    #[case::summary_draft_flag_not_a_boolean(with(
        tracked(),
        "summary",
        with(tracked()["summary"].clone(), "isDraft", json!("no"))
    ))]
    #[case::delivery_without_the_sent_prompt(with(
        tracked(),
        "delivered",
        json!({ "at": "2026-10-23T02:00:00.000Z", "sender": "sender" })
    ))]
    #[case::missing_reason(without(tracked(), "reason"))]
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
    fn damaged_record_error_names_the_field() {
        let root = tempfile::tempdir().expect("create a directory");
        let directory = root.path().join(pr_key(&pay(7)));
        fs::create_dir(&directory).expect("create the PR directory");
        let record = with(tracked(), "pending", json!([{ "type": "merged" }]));
        fs::write(directory.join("state.json"), record.to_string()).expect("write the record");

        let loaded = load_watch_state(root.path(), &pay(7));

        let error = loaded.expect_err("the record is refused").to_string();
        assert!(error.contains("pending"), "{error}");
    }

    fn pay(number: u64) -> PrRef {
        PrRef {
            repo: "org/pay".to_owned(),
            number,
        }
    }

    // A state that has been tracking the PR, so a refused record is one that would cost
    // something to reset.
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
