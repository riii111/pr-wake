use std::path::Path;

use serde_json::{Map, Value, json};

use super::thread::{NOT_SENT, result_text};

pub(crate) struct NewChat<'chat> {
    pub(crate) checkout: &'chat Path,
    pub(crate) head_sha: &'chat str,
    pub(crate) title: &'chat str,
    pub(crate) prompt: &'chat str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Created {
    Started(String),
    NotSent(String),
    // The chat may exist, so no other chat is opened for the same request.
    Unknown(String),
}

type CallTool<'call> = dyn FnMut(&str, Map<String, Value>) -> Map<String, Value> + 'call;

pub(crate) fn create_chat(call: &mut CallTool<'_>, chat: &NewChat<'_>) -> Created {
    let projects = call("list_projects", Map::new());
    let project = match project_of(&projects, chat.checkout) {
        Ok(project) => project,
        Err(reason) => return Created::NotSent(reason),
    };
    created(&call("create_thread", arguments(&project, chat)))
}

fn arguments(project: &str, chat: &NewChat<'_>) -> Map<String, Value> {
    let arguments = json!({
        "title": chat.title,
        "prompt": chat.prompt,
        // The app resolves the branch name as any ref, so the worktree starts at the fetched head.
        "target": {
            "type": "project",
            "projectId": project,
            "environment": {
                "type": "worktree",
                "startingState": { "type": "branch", "branchName": chat.head_sha },
            },
        },
    });
    match arguments {
        Value::Object(arguments) => arguments,
        _ => unreachable!("the arguments are an object"),
    }
}

// Only a local project at the checkout counts, so the chat's worktree is added from the configured clone.
fn project_of(answer: &Map<String, Value>, checkout: &Path) -> Result<String, String> {
    if answer.get("outcome").and_then(Value::as_str) != Some("done") {
        return Err(format!(
            "cannot list the Codex projects: {}",
            message_of(answer)
        ));
    }
    let listed = serde_json::from_str::<Value>(&result_text(answer)).unwrap_or_default();
    listed["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|project| {
            project["hostId"] == "local"
                && project["path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path) == checkout)
        })
        .and_then(|project| project["projectId"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "Codex App has no project at {}; add the checkout as a project",
                checkout.display()
            )
        })
}

fn created(answer: &Map<String, Value>) -> Created {
    let outcome = answer
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if NOT_SENT.contains(&outcome) {
        return Created::NotSent(format!("{outcome}: {}", message_of(answer)));
    }
    match answer.get("threadId").and_then(Value::as_str) {
        Some(thread) if outcome == "done" => Created::Started(thread.to_owned()),
        _ => Created::Unknown(format!("{outcome}: {}", message_of(answer))),
    }
}

fn message_of(answer: &Map<String, Value>) -> &str {
    answer
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKOUT: &str = "/repos/app";

    #[test]
    fn chat_starts_in_a_worktree_of_the_checkouts_project_at_the_head() {
        let mut calls = Vec::new();
        let created = create_chat(
            &mut |tool, arguments| {
                calls.push((tool.to_owned(), Value::Object(arguments)));
                match tool {
                    "list_projects" => listed(&[("other", "/repos/other"), ("p1", CHECKOUT)]),
                    _ => answer(json!({ "outcome": "done", "threadId": "review-chat" })),
                }
            },
            &chat(),
        );

        assert_eq!(created, Created::Started("review-chat".to_owned()));
        assert_eq!(
            calls,
            [
                ("list_projects".to_owned(), json!({})),
                (
                    "create_thread".to_owned(),
                    json!({
                        "title": "Review acme/app#12",
                        "prompt": "review it",
                        "target": {
                            "type": "project",
                            "projectId": "p1",
                            "environment": {
                                "type": "worktree",
                                "startingState": { "type": "branch", "branchName": "abc" },
                            },
                        },
                    })
                ),
            ]
        );
    }

    #[test]
    fn chat_is_not_created_without_a_project_at_the_checkout() {
        for (name, projects) in [
            (
                "unlisted",
                answer(json!({ "outcome": "not_sent", "message": "socket is off" })),
            ),
            ("elsewhere", listed(&[("other", "/repos/other")])),
        ] {
            let mut tools = Vec::new();
            let created = create_chat(
                &mut |tool, _| {
                    tools.push(tool.to_owned());
                    projects.clone()
                },
                &chat(),
            );

            assert!(
                matches!(created, Created::NotSent(_)),
                "case: {name}: {created:?}"
            );
            assert_eq!(tools, ["list_projects"], "case: {name}");
        }
    }

    #[test]
    fn create_outcome_tells_a_chat_that_may_exist_from_one_never_sent() {
        for (name, created_answer, expected) in [
            (
                "rejected",
                json!({ "outcome": "rejected", "message": "bad target" }),
                Created::NotSent("rejected: bad target".to_owned()),
            ),
            (
                "unknown",
                json!({ "outcome": "unknown", "message": "no answer" }),
                Created::Unknown("unknown: no answer".to_owned()),
            ),
            (
                "done_without_thread",
                json!({ "outcome": "done" }),
                Created::Unknown("done: ".to_owned()),
            ),
        ] {
            let created = create_chat(
                &mut |tool, _| match tool {
                    "list_projects" => listed(&[("p1", CHECKOUT)]),
                    _ => answer(created_answer.clone()),
                },
                &chat(),
            );

            assert_eq!(created, expected, "case: {name}");
        }
    }

    fn chat() -> NewChat<'static> {
        NewChat {
            checkout: Path::new(CHECKOUT),
            head_sha: "abc",
            title: "Review acme/app#12",
            prompt: "review it",
        }
    }

    fn listed(projects: &[(&str, &str)]) -> Map<String, Value> {
        let projects = projects
            .iter()
            .map(|(id, path)| json!({ "projectId": id, "path": path, "hostId": "local" }))
            .collect::<Vec<_>>();
        let text = json!({ "projects": projects }).to_string();
        answer(json!({
            "outcome": "done",
            "result": { "content": [{ "type": "text", "text": text }] },
        }))
    }

    fn answer(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(answer) => answer,
            _ => unreachable!("an answer is an object"),
        }
    }
}
