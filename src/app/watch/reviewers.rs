use regex::Regex;

// Moving to another AI reviewer changes the config rather than the watch, so a bot is described by data alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewBot {
    // The key the watch records the bot's reviews under; keep it when other fields change.
    pub(crate) id: String,
    // Shown to the worker.
    pub(crate) name: String,
    pub(crate) login: String,
    // Without one, the bot's findings are sent as they come and no completion is told.
    pub(crate) status: Option<BotProgress>,
    // A review body without this text carries no findings of its own; `None` takes every body.
    pub(crate) actionable_review: Option<String>,
    // A PR comment with this text says the bot found nothing, which the watch checks against the findings it saw.
    pub(crate) no_actionable: Option<String>,
    pub(crate) severity: Option<Severity>,
}

// What the bot keeps on a head while it reviews it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BotProgress {
    CommitStatus { context: String, completed: String },
    // A check run has no description, so its success alone says the review ended.
    CheckRun(String),
}

// Read from an inline finding; the first group is the severity shown to the worker.
#[derive(Debug, Clone)]
pub(crate) struct Severity(Regex);

pub(crate) const PRESET_NAMES: [&str; 2] = ["coderabbit", "claude"];

// JavaScript's `\b` is an ASCII word boundary and Rust's a Unicode one, so the preset asks for the ASCII one.
// It still reads `**éMajor**` as the TypeScript watch did.
const CODERABBIT_SEVERITY: &str = r"\*\*[^*\n]*?(?-u:\b)(Critical|Major|Minor|Trivial|Info)\*\*";

const CLAUDE_SEVERITY: &str = r"^\[(critical|warning|suggestion)\]";

pub(crate) fn preset(name: &str) -> Option<ReviewBot> {
    match name {
        "coderabbit" => Some(coderabbit()),
        "claude" => Some(claude()),
        _ => None,
    }
}

pub(crate) fn coderabbit() -> ReviewBot {
    ReviewBot {
        id: "coderabbit".to_owned(),
        name: "CodeRabbit".to_owned(),
        login: "coderabbitai[bot]".to_owned(),
        status: Some(BotProgress::CommitStatus {
            context: "CodeRabbit".to_owned(),
            completed: "Review completed".to_owned(),
        }),
        actionable_review: Some("Actionable comments posted".to_owned()),
        no_actionable: Some("No actionable comments were generated".to_owned()),
        severity: Some(Severity::new(CODERABBIT_SEVERITY).expect("the preset pattern compiles")),
    }
}

// Claude posts its review with no body and a free-form summary comment, so only its inline findings are read.
fn claude() -> ReviewBot {
    ReviewBot {
        id: "claude".to_owned(),
        name: "Claude".to_owned(),
        login: "claude[bot]".to_owned(),
        status: Some(BotProgress::CheckRun("Claude AI Review".to_owned())),
        actionable_review: None,
        no_actionable: None,
        severity: Some(Severity::new(CLAUDE_SEVERITY).expect("the preset pattern compiles")),
    }
}

pub(crate) fn bot_of<'bots>(bots: &'bots [ReviewBot], login: &str) -> Option<&'bots ReviewBot> {
    bots.iter()
        .find(|bot| bot.login.eq_ignore_ascii_case(login))
}

pub(crate) fn severity_of<'body>(bot: &ReviewBot, body: &'body str) -> Option<&'body str> {
    let captures = bot.severity.as_ref()?.0.captures(body)?;
    captures.get(1).map(|severity| severity.as_str())
}

impl Severity {
    // The regex crate refuses look-around and backreferences, so every pattern runs in linear time.
    pub(crate) fn new(pattern: &str) -> Result<Self, regex::Error> {
        Regex::new(pattern).map(Self)
    }
}

impl PartialEq for Severity {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for Severity {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coderabbit_severity_reads_the_bold_label_with_an_ascii_word_boundary() {
        let bot = coderabbit();
        for (name, body, expected) in [
            (
                "emoji_label",
                "**🔒 Security & Privacy** | **🟠 Major** | **⚡ Quick win**",
                Some("Major"),
            ),
            ("letter_before_label", "**éMajor**", Some("Major")),
            ("word_before_label", "**NotMajor**", None),
            ("no_label", "x", None),
        ] {
            assert_eq!(severity_of(&bot, body), expected, "case: {name}");
        }
    }

    #[test]
    fn claude_severity_is_read_only_at_the_start_of_the_body() {
        let bot = claude();
        for (name, body, expected) in [
            ("leading_label", "[warning] unused import", Some("warning")),
            ("later_line", "note\n[warning] unused import", None),
        ] {
            assert_eq!(severity_of(&bot, body), expected, "case: {name}");
        }
    }

    #[test]
    fn severity_with_look_around_is_refused() {
        let compiled = Severity::new(r"(?=Major)Major");

        assert!(compiled.is_err());
    }

    #[test]
    fn bot_is_found_by_its_login_without_case() {
        let bots = [coderabbit(), claude()];

        let found = bot_of(&bots, "CodeRabbitAI[bot]").map(|bot| bot.id.as_str());

        assert_eq!(found, Some("coderabbit"));
        assert!(bot_of(&bots, "reviewer").is_none());
    }

    #[test]
    fn preset_names_each_name_a_preset() {
        let found = PRESET_NAMES.map(|name| preset(name).map(|bot| bot.id));

        assert_eq!(found, PRESET_NAMES.map(|name| Some(name.to_owned())));
        assert!(preset("copilot").is_none());
    }
}
