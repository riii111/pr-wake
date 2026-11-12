use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::app::watch::{
    pr::{ItemKind, PrComment, PrDetail},
    reviewers::{self, BotProgress, ReviewBot},
    state::{BotStatus, FailedCheck, PrSummary},
};

pub(super) type Object = Map<String, Value>;

// The lists of a PR read to their ends.
pub(super) struct Lists {
    pub(super) threads: Vec<Thread>,
    pub(super) reviews: Vec<Object>,
    pub(super) comments: Vec<Object>,
    pub(super) contexts: Vec<Object>,
}

pub(super) struct Thread {
    pub(super) resolved: bool,
    pub(super) comments: Vec<Object>,
}

// A cancelled run is left out, since a newer push cancels the runs it replaces.
const FAILED_RUNS: [&str; 4] = ["FAILURE", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED"];
const FAILED_STATUSES: [&str; 2] = ["FAILURE", "ERROR"];

pub(super) fn pull_of(repository: Option<&Value>) -> Option<&Object> {
    repository?.get("pullRequest")?.as_object()
}

pub(super) fn summary_of(pull: &Object, bots: &[ReviewBot]) -> PrSummary {
    let commit = last_commit(pull.get("commits"));
    let status = commit.and_then(|commit| commit.get("status")?.as_object());
    let rollup = commit
        .and_then(|commit| commit.get("statusCheckRollup")?.as_object())
        .and_then(|rollup| rollup.get("state")?.as_str())
        .map(str::to_owned);
    PrSummary {
        state: text(pull.get("state")),
        is_draft: pull.get("isDraft") == Some(&Value::Bool(true)),
        head: text(pull.get("headRefOid")),
        updated_at: text(pull.get("updatedAt")),
        mergeable: text(pull.get("mergeable")),
        bots: bots
            .iter()
            .enumerate()
            .filter_map(|(index, bot)| {
                let alias = format!("b{index}");
                let progress = match bot.status.as_ref()? {
                    BotProgress::CommitStatus { .. } => {
                        bot_status_of(status.and_then(|status| status.get(&alias)))
                    }
                    BotProgress::CheckRun(_) => {
                        check_run_status_of(commit.and_then(|commit| commit.get(&alias)))
                    }
                };
                Some((bot.id.clone(), progress))
            })
            .collect(),
        rollup,
    }
}

pub(super) fn detail_of(pull: &Object, lists: &Lists, bots: &[ReviewBot]) -> PrDetail {
    let inline = lists.threads.iter().flat_map(|thread| {
        thread
            .comments
            .iter()
            .map(|node| inline_of(node, thread.resolved))
    });
    let reviews = lists
        .reviews
        .iter()
        .map(|node| plain_of(node, ItemKind::Review, "review"));
    let issue_comments = lists
        .comments
        .iter()
        .map(|node| plain_of(node, ItemKind::IssueComment, "issue"))
        .collect::<Vec<_>>();

    let no_actionable = bots
        .iter()
        .filter(|bot| {
            bot.no_actionable.as_ref().is_some_and(|said| {
                issue_comments.iter().any(|comment| {
                    reviewers::bot_of(bots, &comment.author).is_some_and(|by| by.id == bot.id)
                        && comment.body.contains(said.as_str())
                })
            })
        })
        .map(|bot| bot.id.clone())
        .collect();

    PrDetail {
        summary: summary_of(pull, bots),
        url: text(pull.get("url")),
        title: text(pull.get("title")),
        failed_required: failed_required(&lists.contexts, bots),
        comments: inline.chain(reviews).chain(issue_comments).collect(),
        no_actionable,
    }
}

pub(super) fn nodes_of(connection: Option<&Value>) -> Vec<Object> {
    connection
        .and_then(|connection| connection.get("nodes")?.as_array())
        .map(|nodes| nodes.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default()
}

pub(super) fn next_cursor(connection: Option<&Value>) -> Option<String> {
    let info = connection?.get("pageInfo")?.as_object()?;
    (info.get("hasNextPage") == Some(&Value::Bool(true)))
        .then(|| info.get("endCursor")?.as_str().map(str::to_owned))
        .flatten()
}

pub(super) fn last_commit(commits: Option<&Value>) -> Option<&Object> {
    commits?
        .get("nodes")?
        .as_array()?
        .first()?
        .get("commit")?
        .as_object()
}

pub(super) fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

// A rerun is a later run of the same name, so only the latest run counts.
// Runs are ordered by id, since ids grow with creation while a queued run has no start time yet.
fn check_run_status_of(suites: Option<&Value>) -> Option<BotStatus> {
    let runs = nodes_of(suites)
        .iter()
        .flat_map(|suite| nodes_of(suite.get("checkRuns")))
        .collect::<Vec<_>>();
    let run = runs
        .iter()
        .fold(None, |latest: Option<&Object>, next| match latest {
            Some(latest) if id_of(next) <= id_of(latest) => Some(latest),
            _ => Some(next),
        })?;

    let completed = run
        .get("status")
        .is_some_and(|status| status == "COMPLETED");
    Some(BotStatus {
        state: if completed {
            text(run.get("conclusion"))
        } else {
            "PENDING".to_owned()
        },
        description: String::new(),
        created_at: text(run.get(if completed {
            "completedAt"
        } else {
            "startedAt"
        })),
    })
}

fn bot_status_of(context: Option<&Value>) -> Option<BotStatus> {
    let context = context?.as_object()?;
    Some(BotStatus {
        state: text(context.get("state")),
        description: text(context.get("description")),
        created_at: text(context.get("createdAt")),
    })
}

fn inline_of(node: &Object, resolved: bool) -> PrComment {
    let line = match node.get("line") {
        None | Some(Value::Null) => node.get("originalLine"),
        line => line,
    };
    PrComment {
        id: format!("inline:{}", js_text(node.get("databaseId"))),
        kind: ItemKind::Inline,
        author: login_of(node.get("author")),
        url: text(node.get("url")),
        created_at: text(node.get("createdAt")),
        body: text(node.get("body")),
        path: node.get("path").and_then(Value::as_str).map(str::to_owned),
        line: line.and_then(Value::as_u64),
        reply: node.get("replyTo").is_some_and(Value::is_object),
        resolved,
        approved: false,
    }
}

fn plain_of(node: &Object, kind: ItemKind, prefix: &str) -> PrComment {
    PrComment {
        id: format!("{prefix}:{}", js_text(node.get("databaseId"))),
        kind,
        author: login_of(node.get("author")),
        url: text(node.get("url")),
        created_at: text(node.get("createdAt")),
        body: text(node.get("body")),
        path: None,
        line: None,
        reply: false,
        resolved: false,
        approved: node.get("state").is_some_and(|state| state == "APPROVED"),
    }
}

fn login_of(author: Option<&Value>) -> String {
    let Some(author) = author.and_then(Value::as_object) else {
        return "ghost".to_owned();
    };
    let Some(login) = author.get("login").and_then(Value::as_str) else {
        return "ghost".to_owned();
    };
    // GraphQL drops the [bot] suffix that REST, the web and the ignore list use for apps.
    if author.get("__typename").is_some_and(|kind| kind == "Bot") {
        format!("{login}[bot]")
    } else {
        login.to_owned()
    }
}

// A bot's own status tells its review's progress, not a failure of the PR.
fn failed_required(contexts: &[Object], bots: &[ReviewBot]) -> Vec<FailedCheck> {
    let statuses = bots
        .iter()
        .filter_map(|bot| match &bot.status {
            Some(BotProgress::CommitStatus { context, .. }) => Some(context.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let runs = bots
        .iter()
        .filter_map(|bot| match &bot.status {
            Some(BotProgress::CheckRun(name)) => Some(name.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();

    contexts
        .iter()
        .filter(|context| context.get("isRequired") == Some(&Value::Bool(true)))
        .filter_map(|context| {
            if context
                .get("__typename")
                .is_some_and(|kind| kind == "CheckRun")
            {
                let name = text(context.get("name"));
                let failed = FAILED_RUNS.contains(&text(context.get("conclusion")).as_str());
                (failed && !runs.contains(name.as_str())).then(|| FailedCheck {
                    url: link_of(context.get("detailsUrl")),
                    name,
                })
            } else {
                let name = text(context.get("context"));
                let failed = FAILED_STATUSES.contains(&text(context.get("state")).as_str());
                (failed && !statuses.contains(name.as_str())).then(|| FailedCheck {
                    url: link_of(context.get("targetUrl")),
                    name,
                })
            }
        })
        .collect()
}

fn link_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn id_of(node: &Object) -> i64 {
    node.get("databaseId").and_then(Value::as_i64).unwrap_or(-1)
}

// JavaScript's `String(value)`, which names the comment ids the records already hold.
fn js_text(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}
