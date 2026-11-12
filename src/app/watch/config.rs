use std::{
    collections::{BTreeMap, HashSet},
    sync::LazyLock,
};

use regex::Regex;
use serde::{
    Deserialize, Deserializer,
    de::{Error as _, Unexpected},
};
use serde_json::Value;

use super::{
    field,
    pr::ItemKind,
    reviewers::{self, BotProgress, PRESET_NAMES, ReviewBot, Severity},
};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "ConfigFile")]
pub(crate) struct WatchConfig {
    // Keyed in lower case, since GitHub matches owners and names without case.
    pub(crate) repositories: BTreeMap<String, Repository>,
    pub(crate) review_bots: Vec<ReviewBot>,
    pub(crate) ignore: Vec<IgnoreRule>,
    pub(crate) poll_interval_seconds: u64,
    pub(crate) idle_interval_seconds: u64,
    pub(crate) idle_after_hours: f64,
    pub(crate) quiet_hours: Option<QuietHours>,
    // Put at the top of every message, as a skill to use or the way to report.
    pub(crate) instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Repository {
    // Fixed per repository, so switching the active gh account never points a poll at a repository it cannot see.
    #[serde(deserialize_with = "login")]
    pub(crate) account: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct IgnoreRule {
    #[serde(deserialize_with = "login")]
    pub(crate) author: String,
    #[serde(default, deserialize_with = "field::present")]
    pub(crate) kind: Option<ItemKind>,
    #[serde(default, deserialize_with = "present_text")]
    pub(crate) body_includes: Option<String>,
}

// Minutes after local midnight; an end before the start runs over midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuietHours {
    #[serde(deserialize_with = "clock")]
    pub(crate) start: u16,
    #[serde(deserialize_with = "clock")]
    pub(crate) end: u16,
}

// Unknown keys are refused, so a misspelled setting is reported instead of silently ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ConfigFile {
    repositories: BTreeMap<String, Repository>,
    #[serde(default = "default_review_bots")]
    review_bots: Vec<ConfiguredBot>,
    #[serde(default)]
    ignore: Vec<IgnoreRule>,
    #[serde(default = "default_poll_interval")]
    poll_interval_seconds: u64,
    #[serde(default = "default_idle_interval")]
    idle_interval_seconds: u64,
    #[serde(default = "default_idle_after")]
    idle_after_hours: f64,
    #[serde(default = "default_quiet_hours")]
    quiet_hours: Option<QuietHours>,
    #[serde(default)]
    instructions: Option<String>,
}

// A preset by name, or a bot described in full.
struct ConfiguredBot(ReviewBot);

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DescribedBot {
    #[serde(deserialize_with = "bot_id")]
    id: String,
    #[serde(deserialize_with = "text")]
    name: String,
    #[serde(deserialize_with = "login")]
    login: String,
    #[serde(default)]
    status: Option<DescribedProgress>,
    #[serde(default, deserialize_with = "nullable_text")]
    actionable_review: Option<String>,
    #[serde(default, deserialize_with = "nullable_text")]
    no_actionable: Option<String>,
    #[serde(default, deserialize_with = "severity")]
    severity: Option<Severity>,
}

#[derive(Deserialize)]
#[serde(
    untagged,
    expecting = r#"a status as {"context", "completed"} or {"checkRun"}"#
)]
enum DescribedProgress {
    CommitStatus(CommitStatus),
    CheckRun(CheckRun),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitStatus {
    #[serde(deserialize_with = "text")]
    context: String,
    #[serde(deserialize_with = "text")]
    completed: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CheckRun {
    #[serde(deserialize_with = "text")]
    check_run: String,
}

const MIN_POLL_INTERVAL: u64 = 30;
const MIN_IDLE_INTERVAL: u64 = 60;

static REPOSITORY: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$"));
static LOGIN: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^[A-Za-z0-9][A-Za-z0-9_.-]*(\[bot\])?$"));
static BOT_ID: LazyLock<Regex> = LazyLock::new(|| pattern(r"^[a-z0-9-]+$"));
static CLOCK: LazyLock<Regex> = LazyLock::new(|| pattern(r"^([01][0-9]|2[0-3]):([0-5][0-9])$"));

impl WatchConfig {
    pub(crate) fn account_for(&self, repo: &str) -> Option<&str> {
        self.repositories
            .get(&repo.to_lowercase())
            .map(|repository| repository.account.as_str())
    }
}

impl TryFrom<ConfigFile> for WatchConfig {
    type Error = String;

    fn try_from(file: ConfigFile) -> Result<Self, Self::Error> {
        let repositories = file
            .repositories
            .into_iter()
            .map(|(name, repository)| {
                if REPOSITORY.is_match(&name) {
                    Ok((name.to_lowercase(), repository))
                } else {
                    Err(format!("repositories: {name:?} is not owner/name"))
                }
            })
            .collect::<Result<_, _>>()?;

        let review_bots = file
            .review_bots
            .into_iter()
            .map(|ConfiguredBot(bot)| bot)
            .collect::<Vec<_>>();
        if repeats(review_bots.iter().map(|bot| bot.id.clone())) {
            return Err("reviewBots repeats an id".to_owned());
        }
        // A comment is credited to one bot by its login, so two bots cannot share one.
        if repeats(review_bots.iter().map(|bot| bot.login.to_lowercase())) {
            return Err("reviewBots repeats a login".to_owned());
        }

        if file.poll_interval_seconds < MIN_POLL_INTERVAL {
            return Err(format!(
                "pollIntervalSeconds must be at least {MIN_POLL_INTERVAL}"
            ));
        }
        if file.idle_interval_seconds < MIN_IDLE_INTERVAL {
            return Err(format!(
                "idleIntervalSeconds must be at least {MIN_IDLE_INTERVAL}"
            ));
        }
        if file.idle_after_hours <= 0.0 {
            return Err("idleAfterHours must be positive".to_owned());
        }

        let instructions = file
            .instructions
            .map(|instructions| instructions.trim().to_owned());
        if instructions.as_ref().is_some_and(String::is_empty) {
            return Err("instructions must not be blank".to_owned());
        }

        Ok(Self {
            repositories,
            review_bots,
            ignore: file.ignore,
            poll_interval_seconds: file.poll_interval_seconds,
            idle_interval_seconds: file.idle_interval_seconds,
            idle_after_hours: file.idle_after_hours,
            quiet_hours: file.quiet_hours,
            instructions,
        })
    }
}

impl<'de> Deserialize<'de> for ConfiguredBot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(name) => reviewers::preset(&name).map(Self).ok_or_else(|| {
                D::Error::custom(format!(
                    "unknown preset {name:?}, expected one of {}",
                    PRESET_NAMES.join(", ")
                ))
            }),
            // Read on its own, so an error inside the bot still names the field it is in.
            described => serde_path_to_error::deserialize::<_, DescribedBot>(described)
                .map(|bot| Self(bot.into()))
                .map_err(D::Error::custom),
        }
    }
}

impl From<DescribedBot> for ReviewBot {
    fn from(bot: DescribedBot) -> Self {
        Self {
            id: bot.id,
            name: bot.name,
            login: bot.login,
            status: bot.status.map(|status| match status {
                DescribedProgress::CommitStatus(status) => BotProgress::CommitStatus {
                    context: status.context,
                    completed: status.completed,
                },
                DescribedProgress::CheckRun(run) => BotProgress::CheckRun(run.check_run),
            }),
            actionable_review: bot.actionable_review,
            no_actionable: bot.no_actionable,
            severity: bot.severity,
        }
    }
}

fn default_review_bots() -> Vec<ConfiguredBot> {
    vec![ConfiguredBot(reviewers::coderabbit())]
}

const fn default_poll_interval() -> u64 {
    30
}

const fn default_idle_interval() -> u64 {
    600
}

const fn default_idle_after() -> f64 {
    72.0
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "serde fills the optional field with what this returns"
)]
const fn default_quiet_hours() -> Option<QuietHours> {
    Some(QuietHours {
        start: 22 * 60,
        end: 5 * 60,
    })
}

fn login<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    matching(deserializer, &LOGIN, "a GitHub login")
}

fn bot_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    matching(
        deserializer,
        &BOT_ID,
        "an id of lower-case letters, digits and hyphens",
    )
}

fn clock<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u16, D::Error> {
    let clock = String::deserialize(deserializer)?;
    let minutes = CLOCK.captures(&clock).and_then(|parts| {
        let hours = parts.get(1)?.as_str().parse::<u16>().ok()?;
        let minutes = parts.get(2)?.as_str().parse::<u16>().ok()?;
        Some(hours * 60 + minutes)
    });
    minutes.ok_or_else(|| D::Error::invalid_value(Unexpected::Str(&clock), &"a time as HH:MM"))
}

fn text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.is_empty() {
        return Err(D::Error::invalid_value(
            Unexpected::Str(&text),
            &"a non-empty string",
        ));
    }
    Ok(text)
}

fn present_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    text(deserializer).map(Some)
}

fn nullable_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    struct Text(#[serde(deserialize_with = "text")] String);

    Option::<Text>::deserialize(deserializer).map(|text| text.map(|Text(text)| text))
}

fn severity<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Severity>, D::Error> {
    let Some(source) = nullable_text(deserializer)? else {
        return Ok(None);
    };
    Severity::new(&source)
        .map(Some)
        .map_err(|error| D::Error::custom(format!("is not a regular expression: {error}")))
}

fn matching<'de, D: Deserializer<'de>>(
    deserializer: D,
    pattern: &Regex,
    expected: &str,
) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    if !pattern.is_match(&text) {
        return Err(D::Error::invalid_value(Unexpected::Str(&text), &expected));
    }
    Ok(text)
}

fn repeats(mut keys: impl Iterator<Item = String>) -> bool {
    let mut seen = HashSet::new();
    keys.any(|key| !seen.insert(key))
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("the pattern compiles")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn omitted_keys_take_their_defaults_and_watch_coderabbit() {
        let config = parse(&json!({})).expect("parse the config");

        assert_eq!(
            config,
            WatchConfig {
                repositories: BTreeMap::from([(
                    "org/pay".to_owned(),
                    Repository {
                        account: "owner".to_owned(),
                    },
                )]),
                review_bots: vec![reviewers::coderabbit()],
                ignore: Vec::new(),
                poll_interval_seconds: 30,
                idle_interval_seconds: 600,
                idle_after_hours: 72.0,
                quiet_hours: Some(QuietHours {
                    start: 22 * 60,
                    end: 5 * 60,
                }),
                instructions: None,
            }
        );
    }

    #[test]
    fn instructions_are_trimmed_and_refused_when_blank() {
        let given = parse(&json!({ "instructions": " $task-worker\n" }));
        let blank = parse(&json!({ "instructions": "  " }));

        assert_eq!(
            given.expect("parse the config").instructions.as_deref(),
            Some("$task-worker")
        );
        assert!(blank.is_err());
    }

    #[test]
    fn repositories_are_found_without_case() {
        let config = parse(&json!({
            "repositories": { "Org/Pay": { "account": "owner" } },
        }))
        .expect("parse the config");

        assert_eq!(config.account_for("org/PAY"), Some("owner"));
        assert_eq!(config.account_for("org/other"), None);
    }

    #[test]
    fn preset_by_name_and_described_bot_are_both_taken() {
        let config = parse(&json!({
            "reviewBots": [
                "coderabbit",
                { "id": "other", "name": "OtherBot", "login": "other-reviewer[bot]" },
            ],
        }))
        .expect("parse the config");

        assert_eq!(
            config.review_bots,
            [
                reviewers::coderabbit(),
                ReviewBot {
                    id: "other".to_owned(),
                    name: "OtherBot".to_owned(),
                    login: "other-reviewer[bot]".to_owned(),
                    status: None,
                    actionable_review: None,
                    no_actionable: None,
                    severity: None,
                },
            ]
        );
    }

    #[test]
    fn described_bot_may_wait_for_a_check_run() {
        let config = parse(&json!({
            "reviewBots": [{
                "id": "other",
                "name": "OtherBot",
                "login": "other-reviewer[bot]",
                "status": { "checkRun": "Other Review" },
            }],
        }))
        .expect("parse the config");

        let statuses = config
            .review_bots
            .into_iter()
            .map(|bot| bot.status)
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            [Some(BotProgress::CheckRun("Other Review".to_owned()))]
        );
    }

    #[test]
    fn ignore_rule_reads_its_optional_kind_and_text() {
        let config = parse(&json!({
            "ignore": [
                { "author": "blacksmith-sh[bot]" },
                {
                    "author": "github-actions[bot]",
                    "kind": "inline",
                    "bodyIncludes": "[phase-timing]",
                },
            ],
        }))
        .expect("parse the config");

        assert_eq!(
            config.ignore,
            [
                IgnoreRule {
                    author: "blacksmith-sh[bot]".to_owned(),
                    kind: None,
                    body_includes: None,
                },
                IgnoreRule {
                    author: "github-actions[bot]".to_owned(),
                    kind: Some(ItemKind::Inline),
                    body_includes: Some("[phase-timing]".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn quiet_hours_are_read_as_minutes_or_turned_off_by_null() {
        let given = parse(&json!({ "quietHours": { "start": "23:30", "end": "06:05" } }));
        let off = parse(&json!({ "quietHours": null }));

        assert_eq!(
            given.expect("parse the config").quiet_hours,
            Some(QuietHours {
                start: 23 * 60 + 30,
                end: 6 * 60 + 5,
            })
        );
        assert_eq!(off.expect("parse the config").quiet_hours, None);
    }

    #[test]
    fn invalid_config_is_refused_with_the_reason() {
        for (name, fields, message) in [
            (
                "status_with_both_kinds",
                json!({ "reviewBots": [{
                    "id": "x",
                    "name": "X",
                    "login": "x[bot]",
                    "status": { "context": "X", "completed": "done", "checkRun": "X" },
                }] }),
                "reviewBots",
            ),
            (
                "severity_not_a_regular_expression",
                json!({ "reviewBots": [
                    { "id": "x", "name": "X", "login": "x[bot]", "severity": "(unclosed" },
                ] }),
                "is not a regular expression",
            ),
            (
                "id_used_twice",
                json!({ "reviewBots": [
                    "coderabbit",
                    { "id": "coderabbit", "name": "X", "login": "x" },
                ] }),
                "reviewBots repeats an id",
            ),
            (
                "login_used_twice",
                json!({ "reviewBots": [
                    "coderabbit",
                    { "id": "other", "name": "X", "login": "CodeRabbitAI[bot]" },
                ] }),
                "reviewBots repeats a login",
            ),
            (
                "unknown_preset",
                json!({ "reviewBots": ["copilot"] }),
                "reviewBots",
            ),
            (
                "unknown_key",
                json!({ "pollInterval": 60 }),
                "unknown field `pollInterval`",
            ),
            (
                "unknown_key_in_a_described_bot",
                json!({ "reviewBots": [{ "id": "x", "name": "X", "login": "x", "model": "y" }] }),
                "reviewBots[0]: model: unknown field `model`",
            ),
            (
                "ignore_kind_null",
                json!({ "ignore": [{ "author": "x", "kind": null }] }),
                "ignore[0].kind",
            ),
            (
                "login_not_a_login",
                json!({ "ignore": [{ "author": "[bot]" }] }),
                "ignore[0].author",
            ),
            (
                "repository_not_owner_and_name",
                json!({ "repositories": { "pay": { "account": "owner" } } }),
                "repositories",
            ),
            (
                "poll_interval_too_short",
                json!({ "pollIntervalSeconds": 29 }),
                "pollIntervalSeconds must be at least 30",
            ),
            (
                "idle_after_not_positive",
                json!({ "idleAfterHours": 0 }),
                "idleAfterHours must be positive",
            ),
            (
                "quiet_hours_not_a_clock",
                json!({ "quietHours": { "start": "24:00", "end": "05:00" } }),
                "quietHours.start",
            ),
        ] {
            let parsed = parse(&fields);

            let error = parsed.expect_err(name);
            assert!(error.contains(message), "case: {name}: {error}");
        }
    }

    // Read as the watch reads its config file, so an error names the field it is in.
    fn parse(fields: &Value) -> Result<WatchConfig, String> {
        let mut config = json!({ "repositories": { "org/pay": { "account": "owner" } } });
        if let (Some(config), Some(fields)) = (config.as_object_mut(), fields.as_object()) {
            config.extend(fields.clone());
        }
        serde_path_to_error::deserialize(config).map_err(|error| error.to_string())
    }
}
