use super::{config::Choice, one_line, state::Found};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) key: String,
    pub(crate) title: &'static str,
    pub(crate) subtitle: String,
    pub(crate) message: String,
    // The choices' labels in config order.
    pub(crate) actions: Vec<String>,
    pub(crate) decline: &'static str,
    // Several actions are offered in a dropdown, which needs its own label.
    pub(crate) dropdown: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Selection {
    // The index of the choice in the config.
    Chosen(usize),
    Declined,
    TimedOut,
    Dismissed,
}

const TITLE: &str = "GitHub レビュー依頼";
const DROPDOWN_LABEL: &str = "レビュー方法を選択";
const DECLINE_LABEL: &str = "見送る";
const UNKNOWN_REQUESTER: &str = "依頼者不明";

impl Notice {
    #[must_use]
    pub(crate) fn of(request: &Found, choices: &[Choice]) -> Self {
        let requester = request.requester.as_ref().map_or_else(
            || UNKNOWN_REQUESTER.to_owned(),
            |login| format!("依頼者: @{login}"),
        );
        let team = request
            .team
            .as_ref()
            .map(|team| format!("（{team} 宛て）"))
            .unwrap_or_default();
        Self {
            key: request.key.clone(),
            title: TITLE,
            subtitle: one_line(&request.title),
            message: format!("{requester}{team}\n{} #{}", request.repo, request.number),
            actions: choices.iter().map(|choice| choice.label.clone()).collect(),
            decline: DECLINE_LABEL,
            dropdown: (choices.len() > 1).then_some(DROPDOWN_LABEL),
        }
    }
}

impl Selection {
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Chosen(_) => "chosen",
            Self::Declined => "declined",
            Self::TimedOut => "timeout",
            Self::Dismissed => "dismissed",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize as _;

    use super::*;
    use crate::app::review::config::ConfigFile;

    #[test]
    fn notice_shows_the_choices_in_config_order_and_the_requester() {
        let choices = choices(&["コードレビュー", "AIレビュー", "ADRレビュー"]);

        let notice = Notice::of(&request(Some("tanaka"), None), &choices);

        assert_eq!(
            notice.actions,
            ["コードレビュー", "AIレビュー", "ADRレビュー"]
        );
        assert_eq!(notice.message, "依頼者: @tanaka\nacme/app #7");
        assert_eq!(notice.subtitle, "Fix the bug");
        assert_eq!(notice.dropdown, Some(DROPDOWN_LABEL));
    }

    #[test]
    fn requester_line_tells_an_unknown_requester_and_a_team() {
        for (name, requester, team, expected) in [
            ("unknown", None, None, "依頼者不明\nacme/app #7"),
            (
                "team",
                Some("sato"),
                Some("acme/backend"),
                "依頼者: @sato（acme/backend 宛て）\nacme/app #7",
            ),
        ] {
            let notice = Notice::of(&request(requester, team), &choices(&["コードレビュー"]));

            assert_eq!(notice.message, expected, "case: {name}");
        }
    }

    #[test]
    fn single_choice_needs_no_dropdown() {
        let notice = Notice::of(&request(None, None), &choices(&["コードレビュー"]));

        assert_eq!(notice.dropdown, None);
    }

    fn request(requester: Option<&str>, team: Option<&str>) -> Found {
        Found {
            key: "acme/app#7@1".to_owned(),
            repo: "acme/app".to_owned(),
            number: 7,
            title: " Fix\nthe  bug".to_owned(),
            requester: requester.map(str::to_owned),
            team: team.map(str::to_owned),
        }
    }

    fn choices(labels: &[&str]) -> Vec<Choice> {
        let choices = labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                serde_json::json!({"id": format!("c{index}"), "label": label, "skill": "s", "prompt": "x"})
            })
            .collect::<Vec<_>>();
        ConfigFile::deserialize(serde_json::json!({ "choices": choices }))
            .expect("read the config")
            .resolve("/h".as_ref(), "/c".as_ref())
            .expect("a valid config")
            .choices
    }
}
