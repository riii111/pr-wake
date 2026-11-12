use std::{
    fs,
    path::{Path, PathBuf},
};

use super::record::{self, BaseDirectories, JsonLayout, ProcessLock, RecordError};
use crate::app::review::{
    config::{ConfigFile, ReviewConfig},
    state::ReviewState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewPaths {
    pub(crate) config: PathBuf,
    pub(crate) state: PathBuf,
}

const STATE: &str = "state.json";
const LOCK: &str = "lock";

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

pub(crate) fn load_config(
    path: &Path,
    home: &Path,
    codex_home: &Path,
) -> Result<ReviewConfig, String> {
    let in_file = |error: String| format!("{}: {error}", path.display());
    let config = record::load_required_json::<ConfigFile>(path)
        .map_err(|error| error.to_string())?
        .resolve(home, codex_home)
        .map_err(in_file)?;

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
