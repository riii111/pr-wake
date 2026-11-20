use std::{
    fs,
    path::{Path, PathBuf},
    process,
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
        state::ReviewState,
    },
    watch::pr::PrRef,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewPaths {
    pub(crate) config: PathBuf,
    pub(crate) state: PathBuf,
}

// The review chat a PR's re-requests go to, kept apart from the watch's link to the PR's worker.
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

// `None` before the review has saved one; a damaged state is refused rather than read as empty.
pub(crate) fn load_state(paths: &ReviewPaths) -> Result<Option<ReviewState>, RecordError> {
    record::load_json(&paths.state_file())
}

pub(crate) fn save_state(paths: &ReviewPaths, state: &ReviewState) -> Result<(), RecordError> {
    record::save_json(&paths.state_file(), state, JsonLayout::TwoSpacesAndNewline)
}

// A chat links its PR while the review runs, so each link has its own file rather than a place in the state.
pub(crate) fn record_chat(paths: &ReviewPaths, chat: &ReviewChat) -> Result<(), RecordError> {
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

// The running review holds the lock on the state, so each request gets its own file and none overwrites another.
pub(crate) fn request_now(
    paths: &ReviewPaths,
    request: &NowRequest,
    at: Timestamp,
) -> Result<(), RecordError> {
    let name = format!("{}-{}.json", at.as_nanosecond(), process::id());
    record::save_json(
        &paths.state.join(NOW).join(name),
        request,
        JsonLayout::TwoSpacesAndNewline,
    )
}

// Each request is removed before it is handed over, so one that cannot be removed is never run again and again.
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
            record::remove_record(&path)?;
            request
        })
        .collect())
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

// Held for the life of the review, so a second one cannot notify or launch the same request.
// `None` while another review holds it.
pub(crate) fn hold_lock(paths: &ReviewPaths) -> Result<Option<ProcessLock>, RecordError> {
    record::hold_lock(&paths.state.join(LOCK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_naming_a_missing_skill_is_refused_with_the_skill() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("review.json");
        fs::write(
            &path,
            r#"{"choices":[{"id":"adr","label":"ADR","skill":"no-such-skill","prompt":"x"}]}"#,
        )
        .expect("write the config");

        let loaded = load_config(&path, directory.path(), &directory.path().join(".codex"));

        let error = loaded.expect_err("the skill is missing");
        assert!(error.contains("no-such-skill"), "{error}");
        assert!(error.contains("review.json"), "{error}");
    }

    #[test]
    fn config_whose_skills_exist_loads() {
        let directory = tempfile::tempdir().expect("create a directory");
        let codex_home = directory.path().join(".codex");
        let skill = codex_home.join("skills/code-review/SKILL.md");
        fs::create_dir_all(skill.parent().expect("a parent")).expect("create the skill");
        fs::write(&skill, "# code-review").expect("write the skill");
        let path = directory.path().join("review.json");
        fs::write(
            &path,
            r#"{"choices":[{"id":"code","label":"コード","skill":"code-review","prompt":"x"}]}"#,
        )
        .expect("write the config");

        let loaded = load_config(&path, directory.path(), &codex_home);

        assert_eq!(
            loaded.map(|config| config.choices[0].skill_path.clone()),
            Ok(skill)
        );
    }

    #[test]
    fn state_is_saved_in_the_go_layout_and_read_back() {
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
    fn lock_allows_one_review() {
        let directory = tempfile::tempdir().expect("create a directory");
        let paths = ReviewPaths {
            config: directory.path().join("review.json"),
            state: directory.path().join("review"),
        };

        let first = hold_lock(&paths).expect("lock");
        let second = hold_lock(&paths).expect("try the lock again");

        assert!(first.is_some());
        assert!(second.is_none(), "a second review must not get the lock");
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
