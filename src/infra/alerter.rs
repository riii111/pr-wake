use std::{ffi::OsString, path::Path, time::Duration};

use serde::Deserialize;

use super::{
    command::{RunTool, ToolCall},
    stop::StopRequest,
};
use crate::app::review::notice::{Notice, Selection};

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "activationType")]
    kind: String,
    #[serde(default, rename = "activationValue")]
    value: Option<String>,
    #[serde(default, rename = "activationValueIndex")]
    index: Option<String>,
}

// alerter closes its notification at its own timeout; this much longer means it hangs.
const HANG_MARGIN: Duration = Duration::from_mins(1);
const FAILURE_TIMEOUT: Duration = Duration::from_secs(30);
const GROUP_PREFIX: &str = "pr-wake:";

// Waits for the owner's answer; a stop request removes the notification without an answer.
pub(crate) fn notify(
    run: &RunTool<'_>,
    alerter: &Path,
    notice: &Notice,
    timeout_seconds: u64,
    stop: &StopRequest,
) -> Result<Selection, String> {
    let output = run(&ToolCall {
        program: alerter,
        arguments: arguments(notice, timeout_seconds),
        timeout: Duration::from_secs(timeout_seconds).saturating_add(HANG_MARGIN),
        stop,
    })?;
    parse_selection(&output, &notice.actions)
}

// A plain notification for a failure after the owner chose a review; it offers nothing and does
// not wait.
pub(crate) fn notify_failure(
    run: &RunTool<'_>,
    osascript: &Path,
    subtitle: &str,
    message: &str,
    stop: &StopRequest,
) -> Result<(), String> {
    run(&ToolCall {
        program: osascript,
        arguments: [
            "-e",
            "on run argv",
            "-e",
            "display notification (item 1 of argv) with title (item 2 of argv) subtitle (item 3 of argv)",
            "-e",
            "end run",
            message,
            "pr-wake",
            subtitle,
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        timeout: FAILURE_TIMEOUT,
        stop,
    })
    .map(drop)
}

fn arguments(notice: &Notice, timeout_seconds: u64) -> Vec<OsString> {
    let mut arguments = vec![
        "--title".to_owned(),
        notice.title.to_owned(),
        "--subtitle".to_owned(),
        notice.subtitle.clone(),
        "--message".to_owned(),
        notice.message.clone(),
        "--actions".to_owned(),
        // The labels hold no commas, as the config checks.
        notice.actions.join(","),
        "--close-label".to_owned(),
        notice.decline.to_owned(),
        "--group".to_owned(),
        format!("{GROUP_PREFIX}{}", notice.key),
        "--timeout".to_owned(),
        timeout_seconds.to_string(),
        "--json".to_owned(),
    ];
    if let Some(dropdown) = notice.dropdown {
        arguments.extend(["--dropdown-label".to_owned(), dropdown.to_owned()]);
    }
    arguments.into_iter().map(OsString::from).collect()
}

// Anything but a click on one of the offered actions starts nothing, and an action that is not
// offered never starts some other review.
fn parse_selection(output: &[u8], actions: &[String]) -> Result<Selection, String> {
    let answer = serde_json::from_slice::<Answer>(output).map_err(|error| {
        format!(
            "alerter output: {error}: {:?}",
            String::from_utf8_lossy(output)
        )
    })?;
    match answer.kind.as_str() {
        "closed" => return Ok(Selection::Declined),
        "timeout" => return Ok(Selection::TimedOut),
        "actionClicked" => {}
        _ => return Ok(Selection::Dismissed),
    }
    let value = answer.value.unwrap_or_default();
    if let Some(index) = answer.index {
        return index
            .parse::<usize>()
            .ok()
            .filter(|chosen| actions.get(*chosen) == Some(&value))
            .map(Selection::Chosen)
            .ok_or_else(|| {
                format!("alerter output names no offered action: index {index:?}, value {value:?}")
            });
    }
    actions
        .iter()
        .position(|action| *action == value)
        .map(Selection::Chosen)
        .ok_or_else(|| format!("alerter output names no offered action: {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_is_grouped_by_request_and_offers_the_actions() {
        let notice = notice(&["コードレビュー", "AIレビュー"]);

        let arguments = arguments(&notice, 60)
            .into_iter()
            .map(|argument| argument.into_string().expect("text"))
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "--title",
                "GitHub レビュー依頼",
                "--subtitle",
                "Fix",
                "--message",
                "依頼者: @tanaka\nacme/app #7",
                "--actions",
                "コードレビュー,AIレビュー",
                "--close-label",
                "見送る",
                "--group",
                "pr-wake:acme/app#7@1",
                "--timeout",
                "60",
                "--json",
                "--dropdown-label",
                "レビュー方法を選択",
            ]
        );
    }

    #[test]
    fn answer_is_read_into_what_the_owner_did() {
        let actions = ["ADRレビュー".to_owned(), "コードレビュー".to_owned()];
        for (name, output, expected) in [
            (
                "dropdown",
                r#"{"activationType":"actionClicked","activationValue":"コードレビュー","activationValueIndex":"1"}"#,
                Ok(Selection::Chosen(1)),
            ),
            (
                "single_action",
                r#"{"activationType":"actionClicked","activationValue":"ADRレビュー"}"#,
                Ok(Selection::Chosen(0)),
            ),
            (
                "decline",
                r#"{"activationType":"closed","activationValue":"見送る"}"#,
                Ok(Selection::Declined),
            ),
            (
                "timeout",
                r#"{"activationType":"timeout"}"#,
                Ok(Selection::TimedOut),
            ),
            (
                "body_click",
                r#"{"activationType":"contentsClicked"}"#,
                Ok(Selection::Dismissed),
            ),
            (
                "unknown_action",
                r#"{"activationType":"actionClicked","activationValue":"x"}"#,
                Err(()),
            ),
            (
                "index_of_another_label",
                r#"{"activationType":"actionClicked","activationValue":"ADRレビュー","activationValueIndex":"1"}"#,
                Err(()),
            ),
            ("not_json", "", Err(())),
        ] {
            let selection = parse_selection(output.as_bytes(), &actions);

            assert_eq!(selection.map_err(|_| ()), expected, "case: {name}");
        }
    }

    fn notice(actions: &[&str]) -> Notice {
        Notice {
            key: "acme/app#7@1".to_owned(),
            title: "GitHub レビュー依頼",
            subtitle: "Fix".to_owned(),
            message: "依頼者: @tanaka\nacme/app #7".to_owned(),
            actions: actions.iter().map(|action| (*action).to_owned()).collect(),
            decline: "見送る",
            dropdown: Some("レビュー方法を選択"),
        }
    }
}
