use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

use jiff::{SignedDuration, civil::Time};
use regex::Regex;
use serde::Deserialize;

use super::{
    PullRequest, is_repo_name,
    prompt::{Prompt, PromptData},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewConfig {
    // `owner/repo` maps to a checkout and `owner/*` maps to their parent directory.
    pub(crate) repositories: BTreeMap<String, PathBuf>,
    // `org/team-slug` whose review requests are notified too; direct requests always are.
    pub(crate) teams: Vec<String>,
    pub(crate) poll_interval_seconds: u64,
    pub(crate) worktree_root: PathBuf,
    pub(crate) notification_timeout_seconds: u64,
    // One chat home for every review, so Codex asks to trust one project rather than one per PR.
    pub(crate) chat_home: PathBuf,
    pub(crate) max_notifications: usize,
    pub(crate) commands: ConfiguredCommands,
    pub(crate) choices: Vec<Choice>,
    pub(crate) batches: Option<Batches>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Batches {
    // Local times in order, each the time the owner reads the batch's notification.
    pub(crate) ready_at: Vec<Time>,
    pub(crate) lead: SignedDuration,
}

// Absolute paths that replace the lookup in the fixed directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConfiguredCommands {
    pub(crate) gh: Option<PathBuf>,
    pub(crate) git: Option<PathBuf>,
    pub(crate) alerter: Option<PathBuf>,
    pub(crate) open: Option<PathBuf>,
    pub(crate) osascript: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) skill: String,
    pub(crate) skill_path: PathBuf,
    prompt: Prompt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ConfigFile {
    #[serde(default)]
    repositories: BTreeMap<String, String>,
    #[serde(default)]
    teams: Vec<String>,
    // Existing configs may write 0 for a default, so 0 takes the default as a number left out does.
    #[serde(default)]
    poll_interval_seconds: i64,
    #[serde(default)]
    worktree_root: String,
    #[serde(default)]
    notification_timeout_seconds: i64,
    #[serde(default)]
    max_notifications: i64,
    #[serde(default)]
    commands: CommandsFile,
    #[serde(default)]
    choices: Vec<ChoiceFile>,
    #[serde(default)]
    batches: Option<BatchesFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BatchesFile {
    #[serde(default)]
    ready_at: Vec<String>,
    // Unlike the older numbers, 0 is a valid lead, so only a missing one takes the default.
    #[serde(default)]
    lead_minutes: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandsFile {
    #[serde(default)]
    gh: String,
    #[serde(default)]
    git: String,
    #[serde(default)]
    alerter: String,
    #[serde(default)]
    open: String,
    #[serde(default)]
    osascript: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoiceFile {
    id: String,
    label: String,
    skill: String,
    prompt: String,
}

const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
const DEFAULT_POLL_SECONDS: u64 = 30;
const MIN_POLL_SECONDS: i64 = 30;
const DEFAULT_NOTIFICATIONS: usize = 3;
const MAX_NOTIFICATIONS: i64 = 10;
const DEFAULT_LEAD_MINUTES: i64 = 10;
const MINUTES_PER_DAY: i64 = 24 * 60;

const CHOICE_ID_PATTERN: &str = "^[a-z0-9][a-z0-9-]*$";
static CHOICE_ID: LazyLock<Regex> = LazyLock::new(|| pattern(CHOICE_ID_PATTERN));
static SKILL_NAME: LazyLock<Regex> = LazyLock::new(|| pattern("^[A-Za-z0-9][A-Za-z0-9_-]*$"));
static CLOCK_TIME: LazyLock<Regex> = LazyLock::new(|| pattern("^([01][0-9]|2[0-3]):([0-5][0-9])$"));

impl ConfigFile {
    // Checking that each skill exists reads the disk, so it is left to the caller.
    pub(crate) fn resolve(self, home: &Path, codex_home: &Path) -> Result<ReviewConfig, String> {
        Ok(ReviewConfig {
            notification_timeout_seconds: notification_timeout(self.notification_timeout_seconds)?,
            poll_interval_seconds: poll_interval(self.poll_interval_seconds)?,
            max_notifications: max_notifications(self.max_notifications)?,
            teams: checked_teams(self.teams)?,
            worktree_root: worktree_root(&self.worktree_root, home)?,
            chat_home: home.join("review-home"),
            repositories: checked_repositories(self.repositories, home)?,
            commands: self.commands.resolve()?,
            choices: checked_choices(self.choices, codex_home)?,
            batches: self.batches.map(BatchesFile::resolve).transpose()?,
        })
    }
}

impl ReviewConfig {
    // Each owner once, so one search covers all its configured repositories.
    #[must_use]
    pub(crate) fn owners(&self) -> Vec<String> {
        self.repositories
            .keys()
            .filter_map(|repo| repo.split_once('/').map(|(owner, _)| owner.to_owned()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub(crate) fn checkout_of(&self, repo: &str) -> Option<PathBuf> {
        let exact = self
            .repositories
            .iter()
            // GitHub matches repository names without case.
            .find(|(name, _)| name.eq_ignore_ascii_case(repo));
        if let Some((_, checkout)) = exact {
            return Some(checkout.clone());
        }
        let (owner, name) = repo.split_once('/')?;
        if !is_repo_name(repo) || matches!(name, "." | "..") {
            return None;
        }
        self.repositories.iter().find_map(|(pattern, root)| {
            let configured_owner = pattern.strip_suffix("/*")?;
            configured_owner
                .eq_ignore_ascii_case(owner)
                .then(|| root.join(name))
        })
    }
}

impl Choice {
    #[must_use]
    pub(crate) fn render(&self, pr: &PullRequest, requester: Option<&str>) -> String {
        self.prompt.render(&PromptData {
            pr,
            requester,
            skill: &self.skill,
            skill_path: &self.skill_path,
        })
    }
}

impl CommandsFile {
    fn resolve(self) -> Result<ConfiguredCommands, String> {
        let absolute = |name: &str, path: String| {
            if path.is_empty() {
                Ok(None)
            } else if Path::new(&path).is_absolute() {
                Ok(Some(PathBuf::from(path)))
            } else {
                Err(format!(
                    "commands.{name} must be an absolute path: {path:?}"
                ))
            }
        };
        Ok(ConfiguredCommands {
            gh: absolute("gh", self.gh)?,
            git: absolute("git", self.git)?,
            alerter: absolute("alerter", self.alerter)?,
            open: absolute("open", self.open)?,
            osascript: absolute("osascript", self.osascript)?,
        })
    }
}

impl BatchesFile {
    fn resolve(self) -> Result<Batches, String> {
        if self.ready_at.is_empty() {
            return Err("batches.readyAt must have at least one time".to_owned());
        }
        let minutes = self.lead_minutes.unwrap_or(DEFAULT_LEAD_MINUTES);
        if !(0..MINUTES_PER_DAY).contains(&minutes) {
            return Err(format!(
                "batches.leadMinutes must be between 0 and {}",
                MINUTES_PER_DAY - 1
            ));
        }
        let lead = SignedDuration::from_mins(minutes);

        let mut ready_at = Vec::with_capacity(self.ready_at.len());
        for (index, text) in self.ready_at.iter().enumerate() {
            let time = clock_time(text)
                .ok_or_else(|| format!("batches.readyAt[{index}] is not HH:MM: {text:?}"))?;
            // A batch that would start the day before cannot be told apart from that day's batches.
            if time.duration_since(Time::midnight()) < lead {
                return Err(format!(
                    "batches.leadMinutes puts readyAt[{index}] {text:?} before midnight"
                ));
            }
            if ready_at.contains(&time) {
                return Err(format!("batches.readyAt: duplicate time {text:?}"));
            }
            ready_at.push(time);
        }
        ready_at.sort();
        Ok(Batches { ready_at, lead })
    }
}

fn clock_time(text: &str) -> Option<Time> {
    let captures = CLOCK_TIME.captures(text)?;
    let hour = captures.get(1)?.as_str().parse().ok()?;
    let minute = captures.get(2)?.as_str().parse().ok()?;
    Time::new(hour, minute, 0, 0).ok()
}

fn notification_timeout(seconds: i64) -> Result<u64, String> {
    match seconds {
        0 => Ok(DEFAULT_TIMEOUT_SECONDS),
        _ => u64::try_from(seconds)
            .map_err(|_| "notificationTimeoutSeconds must not be negative".to_owned()),
    }
}

fn poll_interval(seconds: i64) -> Result<u64, String> {
    match seconds {
        0 => Ok(DEFAULT_POLL_SECONDS),
        _ if seconds < MIN_POLL_SECONDS => Err(format!(
            "pollIntervalSeconds must be at least {MIN_POLL_SECONDS}"
        )),
        _ => Ok(seconds.unsigned_abs()),
    }
}

fn max_notifications(count: i64) -> Result<usize, String> {
    match count {
        0 => Ok(DEFAULT_NOTIFICATIONS),
        1..=MAX_NOTIFICATIONS => usize::try_from(count).map_err(|error| error.to_string()),
        _ => Err(format!(
            "maxNotifications must be between 1 and {MAX_NOTIFICATIONS}"
        )),
    }
}

fn checked_teams(teams: Vec<String>) -> Result<Vec<String>, String> {
    if let Some((index, team)) = teams
        .iter()
        .enumerate()
        .find(|(_, team)| !is_repo_name(team))
    {
        return Err(format!("teams[{index}] is not org/team-slug: {team:?}"));
    }
    Ok(teams)
}

fn worktree_root(configured: &str, home: &Path) -> Result<PathBuf, String> {
    let root = if configured.is_empty() {
        home.join(".local/share/pr-wake/worktrees")
    } else {
        expand_home(configured, home)
    };
    if !root.is_absolute() {
        return Err(format!("worktreeRoot must be absolute: {configured:?}"));
    }
    Ok(clean(&root))
}

fn checked_repositories(
    repositories: BTreeMap<String, String>,
    home: &Path,
) -> Result<BTreeMap<String, PathBuf>, String> {
    repositories
        .into_iter()
        .map(|(repo, path)| {
            let name = repo.strip_suffix("/*").map(|owner| format!("{owner}/repo"));
            if !is_repo_name(name.as_deref().unwrap_or(&repo)) {
                return Err(format!(
                    "repositories: {repo:?} is not owner/repo or owner/*"
                ));
            }
            let expanded = expand_home(&path, home);
            if !expanded.is_absolute() {
                return Err(format!(
                    "repositories.{repo} must be an absolute path: {path:?}"
                ));
            }
            Ok((repo, clean(&expanded)))
        })
        .collect()
}

fn checked_choices(choices: Vec<ChoiceFile>, codex_home: &Path) -> Result<Vec<Choice>, String> {
    if choices.is_empty() {
        return Err("choices must have at least one entry".to_owned());
    }

    let mut ids = HashSet::new();
    let mut labels = HashSet::new();
    choices
        .into_iter()
        .enumerate()
        .map(|(index, choice)| {
            if !CHOICE_ID.is_match(&choice.id) {
                return Err(format!(
                    "choices[{index}].id must match {CHOICE_ID_PATTERN}: {:?}",
                    choice.id
                ));
            }
            if !ids.insert(choice.id.clone()) {
                return Err(format!("choices: duplicate id {:?}", choice.id));
            }

            let label = choice.label.trim();
            // alerter splits its actions on commas.
            if label.is_empty() || label.contains([',', '\n']) {
                return Err(format!(
                    "choices[{index}].label must be one line without commas: {:?}",
                    choice.label
                ));
            }
            if !labels.insert(label.to_owned()) {
                return Err(format!("choices: duplicate label {label:?}"));
            }

            if !SKILL_NAME.is_match(&choice.skill) {
                return Err(format!(
                    "choices[{index}].skill is not a skill name: {:?}",
                    choice.skill
                ));
            }

            if choice.prompt.trim().is_empty() {
                return Err(format!("choices[{index}].prompt is empty"));
            }
            // An unknown field must fail before the owner picks the choice, so the prompt is parsed at load.
            let prompt = Prompt::parse(&choice.prompt)
                .map_err(|error| format!("choices[{index}].prompt: {error}"))?;

            Ok(Choice {
                id: choice.id,
                label: label.to_owned(),
                skill_path: codex_home
                    .join("skills")
                    .join(&choice.skill)
                    .join("SKILL.md"),
                skill: choice.skill,
                prompt,
            })
        })
        .collect()
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_owned();
    }
    path.strip_prefix("~/")
        .map_or_else(|| PathBuf::from(path), |rest| home.join(rest))
}

// Lexically, as the worktree root may not exist until the first review adds a worktree.
fn clean(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                cleaned.pop();
            }
            other => cleaned.push(other),
        }
    }
    cleaned
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("the config patterns are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/me";
    const CODEX: &str = "/Users/me/.codex";

    #[test]
    fn minimal_config_takes_the_defaults() {
        let config = resolve(r#"{"repositories":{"acme/app":"~/src/./app/"},"choices":[CODE]}"#)
            .expect("a valid config");

        assert_eq!(config.poll_interval_seconds, 30);
        assert_eq!(config.notification_timeout_seconds, 1800);
        assert_eq!(config.max_notifications, 3);
        assert_eq!(
            config.worktree_root,
            Path::new("/Users/me/.local/share/pr-wake/worktrees")
        );
        assert_eq!(config.chat_home, Path::new("/Users/me/review-home"));
        assert_eq!(
            config.repositories["acme/app"],
            Path::new("/Users/me/src/app")
        );
        assert_eq!(
            config.choices[0].skill_path,
            Path::new("/Users/me/.codex/skills/code-review/SKILL.md")
        );
        assert_eq!(config.commands, ConfiguredCommands::default());
        assert_eq!(config.batches, None);
    }

    #[test]
    fn batches_are_read_in_time_order_with_the_lead() {
        for (name, batches, expected_lead) in [
            ("default_lead", r#"{"readyAt":["16:00","11:00"]}"#, 10),
            (
                "explicit_lead",
                r#"{"readyAt":["16:00","11:00"],"leadMinutes":0}"#,
                0,
            ),
        ] {
            let config = resolve(&format!(r#"{{"batches":{batches},"choices":[CODE]}}"#))
                .expect("a valid config");

            assert_eq!(
                config.batches,
                Some(Batches {
                    ready_at: vec![Time::constant(11, 0, 0, 0), Time::constant(16, 0, 0, 0)],
                    lead: SignedDuration::from_mins(expected_lead),
                }),
                "case: {name}"
            );
        }
    }

    #[test]
    fn settings_that_would_misbehave_are_refused() {
        for (name, content) in [
            (
                "unsupported_key",
                r#"{"choices":[{"id":"adr","label":"ADR","skill":"adr-review","prompt":"x","model":"gpt"}]}"#,
            ),
            (
                "comma_in_label",
                r#"{"choices":[{"id":"adr","label":"ADR,レビュー","skill":"adr-review","prompt":"x"}]}"#,
            ),
            (
                "duplicate_label",
                r#"{"choices":[CODE,{"id":"other","label":" コードレビュー ","skill":"code-review","prompt":"x"}]}"#,
            ),
            ("duplicate_id", r#"{"choices":[CODE,CODE]}"#),
            (
                "unknown_prompt_field",
                r#"{"choices":[{"id":"adr","label":"ADR","skill":"adr-review","prompt":"{{.PR.Nope}}"}]}"#,
            ),
            (
                "blank_prompt",
                r#"{"choices":[{"id":"adr","label":"ADR","skill":"adr-review","prompt":" \n"}]}"#,
            ),
            (
                "bad_choice_id",
                r#"{"choices":[{"id":"ADR","label":"ADR","skill":"adr-review","prompt":"x"}]}"#,
            ),
            (
                "bad_skill_name",
                r#"{"choices":[{"id":"adr","label":"ADR","skill":"../x","prompt":"x"}]}"#,
            ),
            ("no_choices", r#"{"choices":[]}"#),
            (
                "relative_checkout",
                r#"{"repositories":{"acme/app":"src/app"},"choices":[CODE]}"#,
            ),
            (
                "bad_repository",
                r#"{"repositories":{"acme":"/src"},"choices":[CODE]}"#,
            ),
            ("bad_team", r#"{"teams":["backend"],"choices":[CODE]}"#),
            (
                "relative_command",
                r#"{"commands":{"gh":"gh"},"choices":[CODE]}"#,
            ),
            (
                "relative_worktree_root",
                r#"{"worktreeRoot":"w","choices":[CODE]}"#,
            ),
            (
                "short_poll",
                r#"{"pollIntervalSeconds":29,"choices":[CODE]}"#,
            ),
            (
                "negative_timeout",
                r#"{"notificationTimeoutSeconds":-1,"choices":[CODE]}"#,
            ),
            (
                "too_many_notifications",
                r#"{"maxNotifications":11,"choices":[CODE]}"#,
            ),
        ] {
            let resolved = resolve(content);

            assert!(resolved.is_err(), "case: {name}: {resolved:?}");
        }
    }

    #[test]
    fn batches_that_would_misbehave_are_refused() {
        for (name, content) in [
            (
                "no_batch_time",
                r#"{"batches":{"readyAt":[]},"choices":[CODE]}"#,
            ),
            (
                "batch_time_past_the_day",
                r#"{"batches":{"readyAt":["24:00"]},"choices":[CODE]}"#,
            ),
            (
                "batch_time_with_seconds",
                r#"{"batches":{"readyAt":["11:00:00"]},"choices":[CODE]}"#,
            ),
            (
                "duplicate_batch_time",
                r#"{"batches":{"readyAt":["11:00","11:00"]},"choices":[CODE]}"#,
            ),
            (
                "negative_lead",
                r#"{"batches":{"readyAt":["11:00"],"leadMinutes":-1},"choices":[CODE]}"#,
            ),
            (
                "overflowing_lead",
                r#"{"batches":{"readyAt":["11:00"],"leadMinutes":9223372036854775807},"choices":[CODE]}"#,
            ),
            (
                "lead_before_midnight",
                r#"{"batches":{"readyAt":["00:05"]},"choices":[CODE]}"#,
            ),
            (
                "unsupported_batch_key",
                r#"{"batches":{"readyAt":["11:00"],"days":["mon"]},"choices":[CODE]}"#,
            ),
        ] {
            let resolved = resolve(content);

            assert!(resolved.is_err(), "case: {name}: {resolved:?}");
        }
    }

    #[test]
    fn explicit_settings_are_kept_and_paths_cleaned() {
        let config = resolve(
            r#"{
              "repositories": {"Acme/App": "/src/app/../app2"},
              "teams": ["acme/backend"],
              "pollIntervalSeconds": 30,
              "notificationTimeoutSeconds": 5,
              "maxNotifications": 10,
              "worktreeRoot": "~",
              "commands": {"gh": "/opt/gh", "alerter": ""},
              "choices": [{"id":"code","label":"  コードレビュー ","skill":"code-review","prompt":"x"}]
            }"#,
        )
        .expect("a valid config");

        assert_eq!(config.poll_interval_seconds, 30);
        assert_eq!(config.notification_timeout_seconds, 5);
        assert_eq!(config.max_notifications, 10);
        assert_eq!(config.worktree_root, Path::new(HOME));
        assert_eq!(
            config.checkout_of("acme/app"),
            Some(PathBuf::from("/src/app2"))
        );
        assert_eq!(config.commands.gh.as_deref(), Some(Path::new("/opt/gh")));
        assert_eq!(config.commands.alerter, None);
        assert_eq!(config.owners(), ["Acme"]);
        assert_eq!(config.choices[0].label, "コードレビュー");
    }

    #[test]
    fn owners_are_listed_once_in_order() {
        let config = resolve(
            r#"{"repositories":{"zeta/one":"/a","acme/app":"/b","acme/lib":"/c"},"choices":[CODE]}"#,
        )
        .expect("a valid config");

        assert_eq!(config.owners(), ["acme", "zeta"]);
    }

    #[test]
    fn organization_directory_resolves_new_repositories_and_respects_explicit_checkouts() {
        let config = resolve(
            r#"{"repositories":{"acme/*":"~/src/acme","acme/app":"/custom/app"},"choices":[CODE]}"#,
        )
        .expect("a valid config");

        assert_eq!(config.owners(), ["acme"]);
        assert_eq!(
            config.checkout_of("ACME/new"),
            Some(PathBuf::from("/Users/me/src/acme/new"))
        );
        assert_eq!(
            config.checkout_of("acme/app"),
            Some(PathBuf::from("/custom/app"))
        );
        for repo in ["other/new", "acme/..", "acme/.", "acme/other/new"] {
            assert_eq!(config.checkout_of(repo), None, "{repo}");
        }
    }

    #[test]
    fn example_config_loads() {
        let content = include_str!("../../../config.example.json");

        let resolved = resolve(content);

        assert!(resolved.is_ok(), "{resolved:?}");
    }

    fn resolve(content: &str) -> Result<ReviewConfig, String> {
        let content = content.replace(
            "CODE",
            r#"{"id":"code","label":"コードレビュー","skill":"code-review","prompt":"x"}"#,
        );
        serde_json::from_str::<ConfigFile>(&content)
            .map_err(|error| error.to_string())?
            .resolve(Path::new(HOME), Path::new(CODEX))
    }
}
