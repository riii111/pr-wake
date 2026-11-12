mod pull;
mod query;

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde_json::{Map, Value};

use self::{
    pull::{Lists, Object, Thread},
    query::Page,
};
use crate::{
    app::watch::{
        pr::{GitHub, PrDetail, PrRef, ReadError, ReadErrorKind},
        reviewers::ReviewBot,
        state::PrSummary,
    },
    infra::command::{CommandError, CommandOutput},
};

// One run of gh: its arguments, the token to run it as, and its standard input.
pub(crate) struct GhCall<'call> {
    pub(crate) arguments: &'call [&'call str],
    pub(crate) token: Option<&'call str>,
    pub(crate) input: Option<&'call [u8]>,
    pub(crate) timeout: Duration,
}

pub(crate) type RunGh<'run> =
    Box<dyn FnMut(&GhCall<'_>) -> Result<CommandOutput, CommandError> + 'run>;

pub(crate) type Clock<'clock> = Box<dyn Fn() -> Instant + 'clock>;

// GitHub read through `gh api graphql`, as the account each repository is configured with.
pub(crate) struct GhApi<'run> {
    run: RunGh<'run>,
    now: Clock<'run>,
    tokens: HashMap<String, KeptToken>,
}

struct KeptToken {
    value: String,
    at: Instant,
}

// Reading the token starts gh, which on macOS starts the keychain tool too, so it is not read for
// every request; a token the owner rotates is still picked up within this long.
const TOKEN_TTL: Duration = Duration::from_mins(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

// A bound on the reads for one PR, so a list that never ends cannot keep the watch reading.
const MAX_PAGES: usize = 50;

const BRIEF_CHARS: usize = 500;

impl<'run> GhApi<'run> {
    #[must_use]
    pub(crate) fn new(run: RunGh<'run>, now: Clock<'run>) -> Self {
        Self {
            run,
            now,
            tokens: HashMap::new(),
        }
    }

    fn token(&mut self, account: &str) -> Result<String, ReadError> {
        let now = (self.now)();
        if let Some(kept) = self.tokens.get(account)
            && now.duration_since(kept.at) < TOKEN_TTL
        {
            return Ok(kept.value.clone());
        }
        let ran = (self.run)(&GhCall {
            arguments: &[
                "auth",
                "token",
                "--hostname",
                "github.com",
                "--user",
                account,
            ],
            token: None,
            input: None,
            timeout: AUTH_TIMEOUT,
        })
        .map_err(|error| ReadError {
            kind: ReadErrorKind::AuthFailed,
            message: format!("gh auth token: {error}"),
        })?;
        let value = String::from_utf8_lossy(&ran.stdout).trim().to_owned();
        if ran.code != 0 || value.is_empty() {
            return Err(ReadError {
                kind: ReadErrorKind::AuthFailed,
                message: format!("gh has no token for account {account}"),
            });
        }
        // Kept in memory only, so it never reaches the state or the log.
        self.tokens.insert(
            account.to_owned(),
            KeptToken {
                value: value.clone(),
                at: now,
            },
        );
        Ok(value)
    }

    fn query(
        &mut self,
        account: &str,
        text: &str,
        variables: Map<String, Value>,
    ) -> Result<Object, ReadError> {
        let token = self.token(account)?;
        let body = Value::Object(Map::from_iter([
            ("query".to_owned(), Value::from(text)),
            ("variables".to_owned(), Value::Object(variables)),
        ]))
        .to_string();
        let ran = (self.run)(&GhCall {
            arguments: &["api", "graphql", "--hostname", "github.com", "--input", "-"],
            token: Some(&token),
            input: Some(body.as_bytes()),
            timeout: QUERY_TIMEOUT,
        });
        let data = data_of(ran);
        // Any failure drops the token, so a rejected one is read again without telling a 401 by
        // what gh prints, at the cost of one more start of gh after a failure.
        if data.is_err() {
            self.tokens.remove(account);
        }
        data
    }

    // Each later page is read through the node that owns the list.
    fn all_nodes(
        &mut self,
        account: &str,
        pr: &PrRef,
        page: Page,
        owner: Option<&Value>,
        first: Option<&Value>,
    ) -> Result<Vec<Object>, ReadError> {
        let mut nodes = pull::nodes_of(first);
        let mut next = pull::next_cursor(first);
        let mut read = 1;
        while let Some(after) = next {
            if read >= MAX_PAGES {
                return Err(rejected(format!(
                    "{}#{} has more than {MAX_PAGES} pages of {}",
                    pr.repo,
                    pr.number,
                    page.name()
                )));
            }
            let mut variables = Map::from_iter([
                ("id".to_owned(), owner.cloned().unwrap_or(Value::Null)),
                ("after".to_owned(), Value::from(after)),
            ]);
            if page == Page::Contexts {
                variables.insert("number".to_owned(), Value::from(pr.number));
            }
            let data = self.query(account, &page.query(), variables)?;
            let connection = data
                .get("node")
                .and_then(Value::as_object)
                .and_then(|node| node.get(page.field()));
            nodes.extend(pull::nodes_of(connection));
            next = pull::next_cursor(connection);
            read += 1;
        }
        Ok(nodes)
    }

    fn read_lists(&mut self, account: &str, pr: &PrRef, pull: &Object) -> Result<Lists, ReadError> {
        let id = pull.get("id");
        let threads = self
            .all_nodes(
                account,
                pr,
                Page::ReviewThreads,
                id,
                pull.get("reviewThreads"),
            )?
            .into_iter()
            .map(|thread| {
                let comments = self.all_nodes(
                    account,
                    pr,
                    Page::ThreadComments,
                    thread.get("id"),
                    thread.get("comments"),
                )?;
                Ok(Thread {
                    resolved: thread.get("isResolved") == Some(&Value::Bool(true)),
                    comments,
                })
            })
            .collect::<Result<Vec<_>, ReadError>>()?;
        let rollup = pull::last_commit(pull.get("checks"))
            .and_then(|commit| commit.get("statusCheckRollup")?.as_object());
        let contexts = match rollup {
            Some(rollup) => self.all_nodes(
                account,
                pr,
                Page::Contexts,
                rollup.get("id"),
                rollup.get("contexts"),
            )?,
            None => Vec::new(),
        };
        let reviews = self.all_nodes(account, pr, Page::Reviews, id, pull.get("reviews"))?;
        let comments = self.all_nodes(account, pr, Page::Comments, id, pull.get("comments"))?;
        Ok(Lists {
            threads,
            reviews,
            comments,
            contexts,
        })
    }
}

impl GitHub for GhApi<'_> {
    fn summaries(
        &mut self,
        account: &str,
        prs: &[PrRef],
        bots: &[ReviewBot],
    ) -> Result<Vec<Option<PrSummary>>, ReadError> {
        if prs.is_empty() {
            return Ok(Vec::new());
        }
        let (text, variables) = query::summary_query(prs, bots);
        let data = self.query(account, &text, variables)?;
        Ok((0..prs.len())
            .map(|index| {
                pull::pull_of(data.get(&format!("p{index}")))
                    .map(|pull| pull::summary_of(pull, bots))
            })
            .collect())
    }

    fn detail(
        &mut self,
        account: &str,
        pr: &PrRef,
        bots: &[ReviewBot],
    ) -> Result<PrDetail, ReadError> {
        let (owner, name) = query::split_repo(&pr.repo);
        let variables = Map::from_iter([
            ("owner".to_owned(), Value::from(owner)),
            ("name".to_owned(), Value::from(name)),
            ("number".to_owned(), Value::from(pr.number)),
        ]);
        let data = self.query(account, &query::detail_query(bots), variables)?;
        let Some(pull) = pull::pull_of(data.get("repository")) else {
            return Err(rejected(format!(
                "{}#{} is not visible to {account}",
                pr.repo, pr.number
            )));
        };
        let lists = self.read_lists(account, pr, pull)?;
        Ok(pull::detail_of(pull, &lists, bots))
    }

    fn open_pulls(&mut self, account: &str, repo: &str) -> Result<Vec<PrRef>, ReadError> {
        let search = format!("repo:{repo} is:pr is:open draft:false author:{account}");
        let variables = Map::from_iter([("search".to_owned(), Value::from(search))]);
        let data = self.query(account, query::OPEN_QUERY, variables)?;
        Ok(pull::nodes_of(data.get("search"))
            .iter()
            .filter_map(|node| node.get("number")?.as_u64())
            .map(|number| PrRef {
                repo: repo.to_owned(),
                number,
            })
            .collect())
    }
}

// gh exits 1 for an answer with errors and prints the answer still, so an answer without JSON is
// one GitHub never gave.
fn data_of(ran: Result<CommandOutput, CommandError>) -> Result<Object, ReadError> {
    let output = ran.map_err(|error| ReadError {
        kind: ReadErrorKind::Unreachable,
        message: format!("gh api graphql: {error}"),
    })?;
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Err(if output.code == 0 {
            rejected("GitHub answered without JSON".to_owned())
        } else {
            ReadError {
                kind: ReadErrorKind::Unreachable,
                message: format!(
                    "gh api graphql exited with {}: {}",
                    output.code,
                    brief(&output.stderr)
                ),
            }
        });
    };
    if let Some(errors) = body
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        return Err(rejected(format!("GitHub answered: {}", errors_of(errors))));
    }
    if output.code != 0 {
        let said = body
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| brief(&output.stderr), str::to_owned);
        return Err(rejected(format!("GitHub refused the query: {said}")));
    }
    match body.get("data") {
        Some(Value::Object(data)) => Ok(data.clone()),
        _ => Err(rejected("GitHub answered without data".to_owned())),
    }
}

fn errors_of(errors: &[Value]) -> String {
    errors
        .iter()
        .map(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

const fn rejected(message: String) -> ReadError {
    ReadError {
        kind: ReadErrorKind::Rejected,
        message,
    }
}

// Cut at a character boundary, since the message needs only the start of what gh said.
fn brief(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .trim()
        .chars()
        .take(BRIEF_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
    };

    use serde_json::json;

    use super::*;
    use crate::{
        app::watch::{
            reviewers,
            state::{BotStatus, FailedCheck},
        },
        infra::{command::ExternalCommand, stop::StopRequest},
    };

    const PR: &str = "https://github.com/org/pay/pull/7";
    const TOKEN: &str = "secret-token";

    // What the fake gh was asked: every command line, and every query with the token it ran as.
    struct Recorder {
        commands: RefCell<Vec<Vec<String>>>,
        posts: RefCell<Vec<Post>>,
        elapsed: Cell<Duration>,
        started: Instant,
    }

    #[derive(Debug, Clone)]
    struct Post {
        token: Option<String>,
        query: String,
        variables: Value,
    }

    #[test]
    fn detail_is_read_with_the_account_token_and_names_apps_as_github_shows_them() {
        let recorder = Recorder::new();
        let mut api = recorder
            .api(|_| answer(&json!({ "data": { "repository": { "pullRequest": pull() } } })));

        let detail = api
            .detail("owner", &pay(), &[reviewers::coderabbit()])
            .expect("read the detail");

        assert_eq!(
            *recorder.commands.borrow(),
            [
                strings(&[
                    "auth",
                    "token",
                    "--hostname",
                    "github.com",
                    "--user",
                    "owner"
                ]),
                strings(&["api", "graphql", "--hostname", "github.com", "--input", "-"]),
            ]
        );
        let posts = recorder.posts.borrow();
        assert_eq!(
            posts
                .iter()
                .map(|post| (post.token.clone(), post.variables.clone()))
                .collect::<Vec<_>>(),
            [(
                Some(TOKEN.to_owned()),
                json!({ "owner": "org", "name": "pay", "number": 7 })
            )]
        );
        let comments = detail
            .comments
            .iter()
            .map(|comment| {
                (
                    comment.id.as_str(),
                    comment.author.as_str(),
                    comment.line,
                    comment.reply,
                    comment.resolved,
                    comment.approved,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            comments,
            [
                (
                    "inline:11",
                    "coderabbitai[bot]",
                    Some(4),
                    false,
                    true,
                    false
                ),
                ("inline:12", "owner", Some(4), true, true, false),
                ("review:21", "ghost", None, false, false, true),
                ("issue:31", "coderabbitai[bot]", None, false, false, false),
            ]
        );
        // An issue comment has no state, so asking for one would fail the whole query.
        assert!(posts[0].query.contains(
            "reviews(first: 100) { nodes { databaseId url createdAt body author { __typename login } state }"
        ));
        assert!(posts[0].query.contains(
            "comments(first: 100) { nodes { databaseId url createdAt body author { __typename login } }"
        ));
        assert_eq!(
            detail.failed_required,
            [
                FailedCheck {
                    name: "build".to_owned(),
                    url: Some("https://ci.example/1".to_owned()),
                },
                FailedCheck {
                    name: "deploy".to_owned(),
                    url: None,
                },
            ]
        );
        assert_eq!((detail.url.as_str(), detail.title.as_str()), (PR, "Pay"));
        assert_eq!(
            detail.summary.bots["coderabbit"]
                .as_ref()
                .map(|status| status.description.as_str()),
            Some("Review completed")
        );
        assert_eq!(detail.no_actionable, ["coderabbit"]);
    }

    #[test]
    fn every_later_page_of_threads_comments_reviews_and_checks_is_read() {
        let more = json!({ "hasNextPage": true, "endCursor": "next" });
        let last = json!({ "hasNextPage": false, "endCursor": null });
        let inline = |id: u64, reply_to: Option<u64>| {
            json!({
                "databaseId": id,
                "url": format!("{PR}#discussion_r{id}"),
                "createdAt": "2026-10-23T00:00:00Z",
                "body": "",
                "path": "a.kt",
                "line": 1,
                "author": { "__typename": "User", "login": "tanaka" },
                "replyTo": reply_to.map(|id| json!({ "databaseId": id })),
            })
        };
        let plain = |id: u64| {
            json!({
                "databaseId": id,
                "url": format!("{PR}#c{id}"),
                "createdAt": "2026-10-23T00:00:00Z",
                "body": "x",
                "author": { "__typename": "User", "login": "tanaka" },
            })
        };
        let run = |name: &str| json!({ "__typename": "CheckRun", "name": name, "conclusion": "FAILURE", "isRequired": true });
        let pages = json!({
            "pr": {
                "reviewThreads": {
                    "nodes": [{
                        "id": "t2",
                        "isResolved": false,
                        "comments": { "nodes": [inline(2, None)], "pageInfo": last },
                    }],
                    "pageInfo": last,
                },
                "reviews": { "nodes": [plain(21)], "pageInfo": last },
                "comments": { "nodes": [plain(31)], "pageInfo": last },
            },
            "t1": { "comments": { "nodes": [inline(12, Some(11))], "pageInfo": last } },
            "rollup": { "contexts": { "nodes": [run("e2e")], "pageInfo": last } },
        });
        let mut first = pull();
        first["id"] = json!("pr");
        first["checks"] = json!({ "nodes": [{ "commit": { "statusCheckRollup": {
            "id": "rollup",
            "contexts": { "nodes": [run("build")], "pageInfo": more },
        } } }] });
        first["reviewThreads"] = json!({
            "nodes": [{
                "id": "t1",
                "isResolved": false,
                "comments": { "nodes": [inline(11, None)], "pageInfo": more },
            }],
            "pageInfo": more,
        });
        first["reviews"] = json!({ "nodes": [plain(20)], "pageInfo": more });
        first["comments"] = json!({ "nodes": [plain(30)], "pageInfo": more });
        let asked = RefCell::new(Vec::new());
        let recorder = Recorder::new();
        let mut api = recorder.api(|request| {
            let id = request["variables"]["id"].as_str().map(str::to_owned);
            asked
                .borrow_mut()
                .push(id.clone().unwrap_or_else(|| "detail".to_owned()));
            let data = id.map_or_else(
                || json!({ "repository": { "pullRequest": first } }),
                |id| json!({ "node": pages[id.as_str()] }),
            );
            answer(&json!({ "data": data }))
        });

        let detail = api
            .detail("owner", &pay(), &[reviewers::coderabbit()])
            .expect("read the detail");

        let ids = detail
            .comments
            .iter()
            .map(|comment| comment.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "inline:11",
                "inline:12",
                "inline:2",
                "review:20",
                "review:21",
                "issue:30",
                "issue:31"
            ]
        );
        let failed = detail
            .failed_required
            .iter()
            .map(|check| check.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(failed, ["build", "e2e"]);
        assert_eq!(
            *asked.borrow(),
            ["detail", "pr", "t1", "rollup", "pr", "pr"]
        );
    }

    #[test]
    fn every_pr_of_an_account_is_read_in_one_aliased_query() {
        let recorder = Recorder::new();
        let mut api = recorder.api(|_| {
            answer(&json!({ "data": {
                "p0": { "pullRequest": pull() },
                "p1": { "pullRequest": null },
            } }))
        });
        let gone = PrRef {
            repo: "org/gone".to_owned(),
            number: 8,
        };

        let summaries = api
            .summaries("owner", &[pay(), gone], &[reviewers::coderabbit()])
            .expect("read the summaries");

        let posts = recorder.posts.borrow();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            posts[0].variables,
            json!({ "o0": "org", "r0": "pay", "n0": 7, "o1": "org", "r1": "gone", "n1": 8 })
        );
        assert!(
            posts[0]
                .query
                .contains(r#"b0: context(name: "CodeRabbit")"#)
        );
        let first = summaries[0].as_ref().expect("the first PR is visible");
        assert_eq!(first.rollup.as_deref(), Some("FAILURE"));
        assert_eq!(
            first.bots["coderabbit"]
                .as_ref()
                .map(|status| status.state.as_str()),
            Some("SUCCESS")
        );
        assert_eq!(summaries[1], None);
    }

    #[test]
    fn check_run_bot_progress_is_read_from_its_latest_run() {
        let status = |state: &str, created_at: &str| BotStatus {
            state: state.to_owned(),
            description: String::new(),
            created_at: created_at.to_owned(),
        };
        for (name, runs, expected) in [
            (
                "run_in_progress",
                vec![run(
                    1,
                    "IN_PROGRESS",
                    None,
                    Some("2026-10-23T00:00:00Z"),
                    None,
                )],
                Some(status("PENDING", "2026-10-23T00:00:00Z")),
            ),
            (
                "run_again_after_a_failure",
                vec![
                    run(
                        1,
                        "COMPLETED",
                        Some("FAILURE"),
                        Some("2026-10-23T00:00:00Z"),
                        Some("2026-10-23T00:01:00Z"),
                    ),
                    run(
                        2,
                        "COMPLETED",
                        Some("SUCCESS"),
                        Some("2026-10-23T00:05:00Z"),
                        Some("2026-10-23T00:06:00Z"),
                    ),
                ],
                Some(status("SUCCESS", "2026-10-23T00:06:00Z")),
            ),
            (
                "queued_after_a_success",
                vec![
                    run(
                        1,
                        "COMPLETED",
                        Some("SUCCESS"),
                        Some("2026-10-23T00:00:00Z"),
                        Some("2026-10-23T00:01:00Z"),
                    ),
                    run(2, "QUEUED", None, None, None),
                ],
                Some(status("PENDING", "")),
            ),
            (
                "skipped_after_a_success",
                vec![
                    run(
                        1,
                        "COMPLETED",
                        Some("SUCCESS"),
                        Some("2026-10-23T00:00:00Z"),
                        Some("2026-10-23T00:01:00Z"),
                    ),
                    run(
                        2,
                        "COMPLETED",
                        Some("SKIPPED"),
                        None,
                        Some("2026-10-23T00:05:00Z"),
                    ),
                ],
                Some(status("SKIPPED", "2026-10-23T00:05:00Z")),
            ),
            ("no_run_of_that_name", Vec::new(), None),
        ] {
            let suites = json!({ "nodes": [
                { "checkRuns": { "nodes": runs.iter().take(1).collect::<Vec<_>>() } },
                { "checkRuns": { "nodes": [] } },
                { "checkRuns": { "nodes": runs.iter().skip(1).collect::<Vec<_>>() } },
            ] });
            let mut with_runs = pull();
            with_runs["commits"]["nodes"][0]["commit"]["b0"] = suites;
            let recorder = Recorder::new();
            let mut api = recorder
                .api(|_| answer(&json!({ "data": { "p0": { "pullRequest": with_runs } } })));

            let summaries = api
                .summaries("owner", &[pay()], &[claude()])
                .expect("read the summaries");

            let query = recorder.posts.borrow()[0].query.clone();
            assert!(
                query.contains(r#"b0: checkSuites(first: 50) { nodes { checkRuns(last: 1, filterBy: { checkName: "Claude AI Review" }) { nodes { databaseId"#),
                "case: {name}: {query}"
            );
            assert!(!query.contains("status {"), "case: {name}");
            let bots = summaries[0].as_ref().map(|summary| summary.bots.clone());
            assert_eq!(
                bots,
                Some(BTreeMap::from([("claude".to_owned(), expected)])),
                "case: {name}"
            );
        }
    }

    #[test]
    fn check_run_bot_own_failed_run_is_not_a_failed_required_check() {
        let mut with_review = pull();
        with_review["checks"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"]
            .as_array_mut()
            .expect("the contexts")
            .push(json!({
                "__typename": "CheckRun",
                "name": "Claude AI Review",
                "conclusion": "FAILURE",
                "isRequired": true,
            }));
        let recorder = Recorder::new();
        let mut api = recorder
            .api(|_| answer(&json!({ "data": { "repository": { "pullRequest": with_review } } })));

        let detail = api
            .detail("owner", &pay(), &[claude()])
            .expect("read the detail");

        let failed = detail
            .failed_required
            .iter()
            .map(|check| check.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(failed, ["build", "CodeRabbit", "deploy"]);
    }

    #[test]
    fn account_token_is_kept_for_ten_minutes() {
        let recorder = Recorder::new();
        let mut api = recorder.api(|_| answer(&json!({ "data": { "p0": null } })));

        for minutes in [0, 9, 10] {
            recorder.elapsed.set(Duration::from_secs(minutes * 60));
            api.summaries("owner", &[pay()], &[reviewers::coderabbit()])
                .expect("read the summaries");
        }

        assert_eq!(recorder.token_reads(), 2);
    }

    #[test]
    fn token_is_dropped_after_a_failed_query_and_read_again() {
        let recorder = Recorder::new();
        let mut api = recorder.api(|_| CommandOutput {
            code: 1,
            stdout: br#"{"message":"Bad credentials"}"#.to_vec(),
            stderr: b"gh: Bad credentials (HTTP 401)".to_vec(),
        });

        let first = api.summaries("owner", &[pay()], &[reviewers::coderabbit()]);
        let second = api.summaries("owner", &[pay()], &[reviewers::coderabbit()]);

        assert_eq!(recorder.token_reads(), 2);
        let error = first.expect_err("refused");
        assert_eq!(error.kind, ReadErrorKind::Rejected);
        assert_eq!(error.message, "GitHub refused the query: Bad credentials");
        assert!(second.is_err());
    }

    #[test]
    fn github_errors_are_reported_without_the_token() {
        let recorder = Recorder::new();
        let mut api = recorder.api(|_| CommandOutput {
            code: 1,
            stdout: br#"{"errors":[{"message":"Could not resolve to a Repository"}],"data":null}"#
                .to_vec(),
            stderr: Vec::new(),
        });

        let failed = api.detail("owner", &pay(), &[reviewers::coderabbit()]);

        let error = failed.expect_err("rejected");
        assert_eq!(error.kind, ReadErrorKind::Rejected);
        assert_eq!(
            error.message,
            "GitHub answered: Could not resolve to a Repository"
        );
        assert!(!format!("{error:?}").contains(TOKEN));
    }

    #[test]
    fn failure_kind_tells_auth_unreachable_and_rejected_apart() {
        for (name, token, query, expected) in [
            (
                "no_token",
                (1, "", "no oauth token"),
                (0, "{}", ""),
                (
                    ReadErrorKind::AuthFailed,
                    "gh has no token for account owner",
                ),
            ),
            (
                "gh_without_an_answer",
                (0, TOKEN, ""),
                (1, "", "error connecting to api.github.com"),
                (
                    ReadErrorKind::Unreachable,
                    "gh api graphql exited with 1: error connecting to api.github.com",
                ),
            ),
            (
                "answer_without_data",
                (0, TOKEN, ""),
                (0, "{}", ""),
                (ReadErrorKind::Rejected, "GitHub answered without data"),
            ),
            (
                "answer_not_json",
                (0, TOKEN, ""),
                (0, "<html>", ""),
                (ReadErrorKind::Rejected, "GitHub answered without JSON"),
            ),
        ] {
            let mut api = GhApi::new(
                Box::new(|call| {
                    let (code, stdout, stderr) = if call.arguments[0] == "auth" {
                        token
                    } else {
                        query
                    };
                    Ok(output(code, stdout, stderr))
                }),
                Box::new(Instant::now),
            );

            let failed = api.open_pulls("owner", "org/pay");

            let error = failed.expect_err("failed");
            assert_eq!(
                (error.kind, error.message.as_str()),
                expected,
                "case: {name}"
            );
        }
    }

    #[test]
    fn gh_that_cannot_start_is_unreachable_or_has_no_token() {
        let mut api = GhApi::new(
            Box::new(|call| {
                ExternalCommand::new("/nonexistent/gh", call.timeout).run(&StopRequest::default())
            }),
            Box::new(Instant::now),
        );

        let failed = api.open_pulls("owner", "org/pay");

        let error = failed.expect_err("no gh");
        assert_eq!(error.kind, ReadErrorKind::AuthFailed);
        assert!(
            error.message.starts_with("gh auth token: cannot run"),
            "{error:?}"
        );
    }

    #[test]
    fn open_prs_are_searched_by_the_account_in_the_repository() {
        let recorder = Recorder::new();
        let mut api = recorder.api(|_| {
            answer(&json!({ "data": { "search": { "nodes": [{ "number": 7 }, {}, { "number": 9 }] } } }))
        });

        let open = api.open_pulls("owner", "org/pay").expect("search the PRs");

        assert_eq!(open.iter().map(|pr| pr.number).collect::<Vec<_>>(), [7, 9]);
        assert_eq!(
            recorder.posts.borrow()[0].variables,
            json!({ "search": "repo:org/pay is:pr is:open draft:false author:owner" })
        );
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                commands: RefCell::new(Vec::new()),
                posts: RefCell::new(Vec::new()),
                elapsed: Cell::new(Duration::ZERO),
                started: Instant::now(),
            }
        }

        // gh as the test says: a token for every account, and each query answered by `answer`.
        fn api<'test>(
            &'test self,
            mut answer: impl FnMut(&Value) -> CommandOutput + 'test,
        ) -> GhApi<'test> {
            GhApi::new(
                Box::new(move |call| {
                    self.commands.borrow_mut().push(strings(call.arguments));
                    if call.arguments[0] == "auth" {
                        return Ok(output(0, &format!("{TOKEN}\n"), ""));
                    }
                    let request = serde_json::from_slice::<Value>(call.input.unwrap_or_default())
                        .expect("a JSON request");
                    self.posts.borrow_mut().push(Post {
                        token: call.token.map(str::to_owned),
                        query: request["query"].as_str().unwrap_or_default().to_owned(),
                        variables: request["variables"].clone(),
                    });
                    Ok(answer(&request))
                }),
                Box::new(|| self.started + self.elapsed.get()),
            )
        }

        fn token_reads(&self) -> usize {
            self.commands
                .borrow()
                .iter()
                .filter(|command| command[0] == "auth")
                .count()
        }
    }

    fn answer(body: &Value) -> CommandOutput {
        output(0, &body.to_string(), "")
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn pay() -> PrRef {
        PrRef {
            repo: "org/pay".to_owned(),
            number: 7,
        }
    }

    fn claude() -> ReviewBot {
        reviewers::preset("claude").expect("the claude preset")
    }

    fn run(
        id: u64,
        status: &str,
        conclusion: Option<&str>,
        started_at: Option<&str>,
        completed_at: Option<&str>,
    ) -> Value {
        json!({
            "databaseId": id,
            "status": status,
            "conclusion": conclusion,
            "startedAt": started_at,
            "completedAt": completed_at,
        })
    }

    fn strings(arguments: &[&str]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect()
    }

    fn pull() -> Value {
        json!({
            "url": PR,
            "title": "Pay",
            "state": "OPEN",
            "isDraft": false,
            "headRefOid": "a".repeat(40),
            "updatedAt": "2026-10-23T00:00:00Z",
            "mergeable": "MERGEABLE",
            "commits": { "nodes": [{ "commit": {
                "oid": "a".repeat(40),
                "status": { "b0": {
                    "state": "SUCCESS",
                    "description": "Review completed",
                    "createdAt": "2026-10-23T00:00:00Z",
                } },
                "statusCheckRollup": { "state": "FAILURE" },
            } }] },
            "checks": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "build", "conclusion": "FAILURE", "detailsUrl": "https://ci.example/1", "isRequired": true },
                { "__typename": "CheckRun", "name": "label", "conclusion": "FAILURE", "isRequired": false },
                { "__typename": "CheckRun", "name": "superseded", "conclusion": "CANCELLED", "isRequired": true },
                { "__typename": "StatusContext", "context": "CodeRabbit", "state": "FAILURE", "targetUrl": null, "isRequired": true },
                { "__typename": "StatusContext", "context": "deploy", "state": "ERROR", "targetUrl": null, "isRequired": true },
            ] } } } }] },
            "reviewThreads": { "nodes": [{
                "isResolved": true,
                "comments": { "nodes": [
                    {
                        "databaseId": 11,
                        "url": format!("{PR}#discussion_r11"),
                        "createdAt": "2026-10-23T00:00:00Z",
                        "body": "**🟠 Major**",
                        "path": "a.kt",
                        "line": null,
                        "originalLine": 4,
                        "author": { "__typename": "Bot", "login": "coderabbitai" },
                        "replyTo": null,
                    },
                    {
                        "databaseId": 12,
                        "url": format!("{PR}#discussion_r12"),
                        "createdAt": "2026-10-23T00:01:00Z",
                        "body": "thanks",
                        "path": "a.kt",
                        "line": 4,
                        "author": { "__typename": "User", "login": "owner" },
                        "replyTo": { "databaseId": 11 },
                    },
                ] },
            }] },
            "reviews": { "nodes": [{
                "databaseId": 21,
                "url": format!("{PR}#pullrequestreview-21"),
                "createdAt": "2026-10-23T00:00:00Z",
                "body": "",
                "author": null,
                "state": "APPROVED",
            }] },
            "comments": { "nodes": [{
                "databaseId": 31,
                "url": format!("{PR}#issuecomment-31"),
                "createdAt": "2026-10-23T00:00:00Z",
                "body": "No actionable comments were generated in the recent review. 🎉",
                "author": { "__typename": "Bot", "login": "coderabbitai" },
            }] },
        })
    }
}
