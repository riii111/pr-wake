use std::{
    fs,
    path::{Path, PathBuf},
    process,
    time::SystemTime,
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::{
    record::{self, BaseDirectories, JsonLayout, ProcessLock, RecordError},
    watch_record::pr_key,
};
use crate::app::{
    review::{
        batch::NowRequest,
        config::{ConfigFile, ReviewConfig},
        selection::SelectionRecord,
        state::ReviewState,
    },
    watch::pr::PrRef,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewPaths {
    pub(crate) config: PathBuf,
    pub(crate) state: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewChat {
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) thread: String,
    pub(crate) linked_at: String,
}

const STATE: &str = "state.json";
const LOCK: &str = "lock";
const CHATS: &str = "chats";
const NOW: &str = "now";
const SELECTIONS: &str = "selections";
const RUN: &str = "run.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Run {
    id: String,
}

impl ReviewPaths {
    #[must_use]
    pub(crate) fn of(directories: &BaseDirectories) -> Self {
        Self {
            config: directories
                .config_home()
                .join("pr-wake")
                .join("review.json"),
            state: directories.state_home().join("pr-wake").join("review"),
        }
    }

    #[must_use]
    pub(crate) fn state_file(&self) -> PathBuf {
        self.state.join(STATE)
    }
}

// A missing skill does not matter to commands that never launch a review, so the skills are not checked.
pub(crate) fn read_config(
    path: &Path,
    home: &Path,
    codex_home: &Path,
) -> Result<ReviewConfig, String> {
    record::load_required_json::<ConfigFile>(path)
        .map_err(|error| error.to_string())?
        .resolve(home, codex_home)
        .map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn load_config(
    path: &Path,
    home: &Path,
    codex_home: &Path,
) -> Result<ReviewConfig, String> {
    let in_file = |error: String| format!("{}: {error}", path.display());
    let config = read_config(path, home, codex_home)?;

    // A missing skill would otherwise fail only after the owner picked it, so each one is checked at load.
    for (index, choice) in config.choices.iter().enumerate() {
        fs::metadata(&choice.skill_path).map_err(|error| {
            in_file(format!(
                "choices[{index}].skill {:?}: {}: {error}",
                choice.skill,
                choice.skill_path.display()
            ))
        })?;
    }
    Ok(config)
}

pub(crate) fn load_state(paths: &ReviewPaths) -> Result<Option<ReviewState>, RecordError> {
    record::load_json(&paths.state_file())
}

pub(crate) fn save_state(paths: &ReviewPaths, state: &ReviewState) -> Result<(), RecordError> {
    record::save_json(&paths.state_file(), state, JsonLayout::TwoSpacesAndNewline)
}

pub(crate) fn record_chat(paths: &ReviewPaths, chat: &ReviewChat) -> Result<(), RecordError> {
    // A chat links its PR while the review runs, so each link has its own file rather than a place in the state.
    record::save_json(
        &chat_path(paths, &chat.repo, chat.number),
        chat,
        JsonLayout::TwoSpacesAndNewline,
    )
}

pub(crate) fn load_chat(
    paths: &ReviewPaths,
    repo: &str,
    number: u64,
) -> Result<Option<ReviewChat>, RecordError> {
    record::load_json(&chat_path(paths, repo, number))
}

pub(crate) fn request_now(
    paths: &ReviewPaths,
    request: &NowRequest,
    at: Timestamp,
) -> Result<(), RecordError> {
    // The running review holds the lock on the state, so each request gets its own file and none overwrites another.
    let name = format!("{}-{}.json", at.as_nanosecond(), process::id());
    record::save_json(
        &paths.state.join(NOW).join(name),
        request,
        JsonLayout::TwoSpacesAndNewline,
    )
}

pub(crate) fn take_now_requests(
    paths: &ReviewPaths,
) -> Result<Vec<Result<NowRequest, RecordError>>, RecordError> {
    let directory = paths.state.join(NOW);
    let names = record::list_directory(&directory)?.unwrap_or_default();
    Ok(names
        .into_iter()
        .filter_map(|name| {
            let name = name.into_string().ok()?;
            // Dot files are the temporary files of a request still being written.
            let json = Path::new(&name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
            (!name.starts_with('.') && json).then(|| directory.join(name))
        })
        .map(|path| {
            let request = record::load_required_json::<NowRequest>(&path);
            // Each request is removed before it is handed over, so one that cannot be removed is never run again and again.
            record::remove_record(&path)?;
            request
        })
        .collect())
}

pub(crate) fn save_selection(
    paths: &ReviewPaths,
    id: &str,
    record: &SelectionRecord,
) -> Result<(), RecordError> {
    record::save_json(
        &selection_path(paths, id),
        record,
        JsonLayout::TwoSpacesAndNewline,
    )
}

pub(crate) fn load_selection(
    paths: &ReviewPaths,
    id: &str,
) -> Result<Option<SelectionRecord>, RecordError> {
    record::load_json(&selection_path(paths, id))
}

// `false` when a record of the id exists, which is left untouched.
pub(crate) fn create_selection(
    paths: &ReviewPaths,
    id: &str,
    record: &SelectionRecord,
) -> Result<bool, RecordError> {
    record::save_new_json(
        &selection_path(paths, id),
        record,
        JsonLayout::TwoSpacesAndNewline,
    )
}

pub(crate) fn remove_selection(paths: &ReviewPaths, id: &str) -> Result<(), RecordError> {
    record::remove_record(&selection_path(paths, id))
}

// The ids of the recorded selections, with when each record was last written.
pub(crate) fn selection_ids(
    paths: &ReviewPaths,
) -> Result<Vec<(String, Option<SystemTime>)>, RecordError> {
    let directory = paths.state.join(SELECTIONS);
    let names = record::list_directory(&directory)?.unwrap_or_default();
    Ok(names
        .into_iter()
        .filter_map(|name| {
            let name = name.into_string().ok()?;
            let id = name
                .strip_suffix(".json")
                .filter(|id| !id.starts_with('.'))?;
            let written = fs::metadata(directory.join(&name))
                .and_then(|metadata| metadata.modified())
                .ok();
            Some((id.to_owned(), written))
        })
        .collect())
}

pub(crate) fn record_run(paths: &ReviewPaths, run: &str) -> Result<(), RecordError> {
    record::save_json(
        &paths.state.join(RUN),
        &Run { id: run.to_owned() },
        JsonLayout::TwoSpacesAndNewline,
    )
}

pub(crate) fn load_run(paths: &ReviewPaths) -> Result<Option<String>, RecordError> {
    Ok(record::load_json::<Run>(&paths.state.join(RUN))?.map(|run| run.id))
}

fn selection_path(paths: &ReviewPaths, id: &str) -> PathBuf {
    paths.state.join(SELECTIONS).join(format!("{id}.json"))
}

fn chat_path(paths: &ReviewPaths, repo: &str, number: u64) -> PathBuf {
    let pr = PrRef {
        repo: repo.to_owned(),
        number,
    };
    paths
        .state
        .join(CHATS)
        .join(format!("{}.json", pr_key(&pr)))
}

// `None` while another review holds it.
pub(crate) fn hold_lock(paths: &ReviewPaths) -> Result<Option<ProcessLock>, RecordError> {
    record::hold_lock(&paths.state.join(LOCK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_loads_only_once_its_skills_exist() {
        let directory = tempfile::tempdir().expect("create a directory");
        let codex_home = directory.path().join(".codex");
        let skill = codex_home.join("skills/code-review/SKILL.md");
        let path = directory.path().join("review.json");
        fs::write(
            &path,
            r#"{"choices":[{"id":"code","label":"コード","skill":"code-review","prompt":"x"}]}"#,
        )
        .expect("write the config");

        let missing = load_config(&path, directory.path(), &codex_home);
        fs::create_dir_all(skill.parent().expect("a parent")).expect("create the skill");
        fs::write(&skill, "# code-review").expect("write the skill");
        let loaded = load_config(&path, directory.path(), &codex_home);

        let error = missing.expect_err("the skill is missing");
        assert!(error.contains("code-review"), "{error}");
        assert!(error.contains("review.json"), "{error}");
        assert_eq!(
            loaded.map(|config| config.choices[0].skill_path.clone()),
            Ok(skill)
        );
    }

    #[test]
    fn state_is_saved_in_its_layout_and_read_back() {
        let directory = tempfile::tempdir().expect("create a directory");
        let paths = ReviewPaths {
            config: directory.path().join("review.json"),
            state: directory.path().join("review"),
        };
        let missing = load_state(&paths).expect("read the missing state");

        save_state(&paths, &ReviewState::default()).expect("save the state");

        assert_eq!(missing, None);
        assert_eq!(
            fs::read_to_string(paths.state_file()).expect("read the file"),
            "{\n  \"version\": 1,\n  \"login\": \"\",\n  \"requests\": {},\n  \"prs\": {},\n  \"lastPoll\": \"0001-01-01T00:00:00Z\"\n}\n"
        );
        assert_eq!(
            load_state(&paths).expect("read the state"),
            Some(ReviewState::default())
        );
    }

    #[test]
    fn chat_is_found_under_any_case_of_the_repository_and_replaced_by_a_later_link() {
        let directory = tempfile::tempdir().expect("create a directory");
        let paths = ReviewPaths {
            config: directory.path().join("review.json"),
            state: directory.path().join("review"),
        };
        let chat = |thread: &str| ReviewChat {
            repo: "Acme/App".to_owned(),
            number: 12,
            thread: thread.to_owned(),
            linked_at: "2026-11-13T00:00:00.000Z".to_owned(),
        };

        record_chat(&paths, &chat("first")).expect("link the first chat");
        record_chat(&paths, &chat("second")).expect("link the second chat");

        assert_eq!(
            load_chat(&paths, "acme/app", 12).expect("read the chat"),
            Some(chat("second"))
        );
        assert_eq!(
            load_chat(&paths, "acme/app", 13).expect("read no chat"),
            None
        );
    }

    #[test]
    fn now_requests_are_taken_once_in_the_order_they_came() {
        let directory = tempfile::tempdir().expect("create a directory");
        let paths = ReviewPaths {
            config: directory.path().join("review.json"),
            state: directory.path().join("review"),
        };
        let at = |text: &str| text.parse::<Timestamp>().expect("parse the time");
        let one_pr = NowRequest {
            pr: Some("acme/app#12".to_owned()),
        };
        request_now(&paths, &one_pr, at("2026-11-18T01:00:01Z")).expect("ask for one PR");
        request_now(&paths, &NowRequest::default(), at("2026-11-18T01:00:00Z"))
            .expect("ask for a batch");
        fs::write(paths.state.join("now/0-damaged.json"), "{").expect("write a damaged one");

        let taken = take_now_requests(&paths).expect("take the requests");
        let again = take_now_requests(&paths).expect("take nothing");

        let taken = taken
            .into_iter()
            .map(|request| request.map_err(|error| error.to_string()))
            .collect::<Vec<_>>();
        assert!(taken[0].is_err(), "{taken:?}");
        assert_eq!(taken[1..], [Ok(NowRequest::default()), Ok(one_pr)]);
        assert_eq!(again.len(), 0);
    }

    #[test]
    fn selection_is_created_only_once_and_the_first_record_kept() {
        let directory = tempfile::tempdir().expect("create a directory");
        let paths = ReviewPaths {
            config: directory.path().join("review.json"),
            state: directory.path().join("review"),
        };
        let first = SelectionRecord {
            run: "run-1".to_owned(),
            ..SelectionRecord::of(&["acme/app#12@30".to_owned()])
        };
        let second = SelectionRecord {
            run: "run-2".to_owned(),
            ..SelectionRecord::of(&["acme/app#13@40".to_owned()])
        };

        let created = create_selection(&paths, "one", &first).expect("create the selection");
        let again = create_selection(&paths, "one", &second).expect("find it created");

        assert!(created && !again);
        assert_eq!(
            load_selection(&paths, "one").expect("read the selection"),
            Some(first)
        );
    }

    #[test]
    fn paths_follow_the_xdg_directories() {
        let directories = BaseDirectories::from_environment().expect("HOME is set for tests");

        let paths = ReviewPaths::of(&directories);

        assert_eq!(
            paths.config,
            directories.config_home().join("pr-wake/review.json")
        );
        assert_eq!(
            paths.state_file(),
            directories.state_home().join("pr-wake/review/state.json")
        );
    }
}
