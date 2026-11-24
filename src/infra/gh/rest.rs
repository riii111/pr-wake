use std::{ffi::OsString, iter, path::Path, sync::LazyLock, time::Duration};

use regex::Regex;
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    app::review::{
        PullRequest,
        batch::{Reviewers, SubmittedReview},
        inbox::InboxThread,
        is_repo_name,
        notice::Size,
        request::{LiveRequest, ReviewRequest, SearchHit},
    },
    infra::{
        command::{RunTool, ToolCall},
        stop::StopRequest,
    },
};

// Runs as whichever account gh has active.
pub(crate) struct GhRest<'rest> {
    pub(crate) run: &'rest RunTool<'rest>,
    pub(crate) gh: &'rest Path,
    pub(crate) stop: &'rest StopRequest,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct SearchPage {
    incomplete_results: bool,
    items: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    number: u64,
    title: String,
    updated_at: String,
    repository_url: String,
}

#[derive(Deserialize)]
struct IssueEvent {
    id: u64,
    event: String,
    #[serde(default)]
    review_requester: Option<Account>,
    #[serde(default)]
    requested_reviewer: Option<Account>,
    #[serde(default)]
    requested_team: Option<Team>,
}

#[derive(Deserialize)]
struct Account {
    login: String,
}

#[derive(Deserialize)]
struct Team {
    slug: String,
}

#[derive(Deserialize)]
struct Pull {
    number: u64,
    title: String,
    html_url: String,
    head: Head,
    base: Base,
}

#[derive(Deserialize)]
struct PullRequested {
    state: String,
    #[serde(default)]
    requested_reviewers: Vec<Account>,
    #[serde(default)]
    requested_teams: Vec<Team>,
}

#[derive(Deserialize)]
struct PullSize {
    additions: u64,
    deletions: u64,
    changed_files: u64,
}

#[derive(Deserialize)]
struct PullAuthor {
    #[serde(default)]
    user: Option<Account>,
}

#[derive(Deserialize)]
struct RequestedReviewers {
    #[serde(default)]
    users: Vec<Actor>,
}

#[derive(Deserialize)]
struct Review {
    #[serde(default)]
    user: Option<Actor>,
    state: String,
}

#[derive(Deserialize)]
struct Actor {
    login: String,
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Notification {
    id: String,
    reason: String,
    subject: Subject,
}

#[derive(Deserialize)]
struct Subject {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct Head {
    sha: String,
}

#[derive(Deserialize)]
struct Base {
    #[serde(rename = "ref")]
    name: String,
    repo: BaseRepo,
}

#[derive(Deserialize)]
struct BaseRepo {
    full_name: String,
}

// A search with `--paginate` reads at most ten pages, so a slow GitHub still answers in time.
const TIMEOUT: Duration = Duration::from_mins(2);
const REPOSITORY_API: &str = "https://api.github.com/repos/";

static COMMIT_SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[0-9a-f]{40}$").expect("the commit pattern is valid"));

impl GhRest<'_> {
    pub(crate) fn current_login(&self) -> Result<String, String> {
        let output = self.api(["user"])?;
        match serde_json::from_slice::<User>(&output) {
            Ok(user) if !user.login.is_empty() => Ok(user.login),
            _ => Err(format!(
                "gh api user: no login in {:?}",
                String::from_utf8_lossy(&output)
            )),
        }
    }

    pub(crate) fn search_pull_requests(&self, query: &str) -> Result<Vec<SearchHit>, String> {
        // Search answers at most 1000 results; past that the least recently updated are not seen.
        let output = self.api([
            "--paginate",
            "-X",
            "GET",
            "search/issues",
            "-f",
            &format!("q={query}"),
            "-f",
            "per_page=100",
            "-f",
            "sort=updated",
        ])?;
        let pages = decode_pages::<SearchPage>(&output)
            .map_err(|error| format!("search {query:?}: {error}"))?;

        let mut hits = Vec::new();
        for page in pages {
            // A partial answer would make the missing requests look withdrawn.
            if page.incomplete_results {
                return Err(format!("search {query:?}: incomplete results"));
            }
            for item in page.items {
                let repo = item
                    .repository_url
                    .strip_prefix(REPOSITORY_API)
                    .filter(|repo| is_repo_name(repo));
                let Some(repo) = repo.filter(|_| item.number > 0) else {
                    return Err(format!(
                        "search {query:?}: unexpected item {}#{}",
                        item.repository_url, item.number
                    ));
                };
                hits.push(SearchHit {
                    repo: repo.to_owned(),
                    number: item.number,
                    title: item.title,
                    updated_at: item.updated_at,
                });
            }
        }
        Ok(hits)
    }

    pub(crate) fn review_requests(
        &self,
        repo: &str,
        number: u64,
    ) -> Result<Vec<ReviewRequest>, String> {
        let output = self.api([
            "--paginate",
            &format!("repos/{repo}/issues/{number}/events?per_page=100"),
        ])?;
        let pages = decode_pages::<Vec<IssueEvent>>(&output)
            .map_err(|error| format!("issue events: {error}"))?;
        Ok(pages
            .into_iter()
            .flatten()
            .filter(|event| event.event == "review_requested")
            .map(|event| ReviewRequest {
                id: event.id,
                requester: event.review_requester.map(|account| account.login),
                reviewer: event.requested_reviewer.map(|account| account.login),
                team_slug: event.requested_team.map(|team| team.slug),
            })
            .collect())
    }

    pub(crate) fn pull_request(&self, repo: &str, number: u64) -> Result<PullRequest, String> {
        let output = self.api([format!("repos/{repo}/pulls/{number}").as_str()])?;
        let pull = serde_json::from_slice::<Pull>(&output)
            .map_err(|error| format!("pull request {repo}#{number}: {error}"))?;

        if pull.number != number || !pull.base.repo.full_name.eq_ignore_ascii_case(repo) {
            return Err(format!(
                "pull request {repo}#{number}: answer names {}#{}",
                pull.base.repo.full_name, pull.number
            ));
        }
        if !COMMIT_SHA.is_match(&pull.head.sha) {
            return Err(format!(
                "pull request {repo}#{number}: invalid head sha {:?}",
                pull.head.sha
            ));
        }

        Ok(PullRequest {
            repo: pull.base.repo.full_name,
            number,
            title: pull.title,
            url: pull.html_url,
            head_sha: pull.head.sha,
            base_ref: pull.base.name,
        })
    }

    pub(crate) fn live_request(&self, repo: &str, number: u64) -> Result<LiveRequest, String> {
        let output = self.api([format!("repos/{repo}/pulls/{number}").as_str()])?;
        let pull = serde_json::from_slice::<PullRequested>(&output)
            .map_err(|error| format!("review requests of {repo}#{number}: {error}"))?;
        Ok(LiveRequest {
            open: pull.state == "open",
            users: pull
                .requested_reviewers
                .into_iter()
                .map(|account| account.login)
                .collect(),
            teams: pull
                .requested_teams
                .into_iter()
                .map(|team| team.slug)
                .collect(),
        })
    }

    pub(crate) fn pull_request_size(&self, repo: &str, number: u64) -> Result<Size, String> {
        let output = self.api([format!("repos/{repo}/pulls/{number}").as_str()])?;
        let pull = serde_json::from_slice::<PullSize>(&output)
            .map_err(|error| format!("pull request {repo}#{number}: {error}"))?;
        Ok(Size {
            additions: pull.additions,
            deletions: pull.deletions,
            changed_files: pull.changed_files,
        })
    }

    pub(crate) fn pull_request_author(
        &self,
        repo: &str,
        number: u64,
    ) -> Result<Option<String>, String> {
        let output = self.api([format!("repos/{repo}/pulls/{number}").as_str()])?;
        let pull = serde_json::from_slice::<PullAuthor>(&output)
            .map_err(|error| format!("pull request {repo}#{number}: {error}"))?;
        Ok(pull.user.map(|user| user.login))
    }

    pub(crate) fn pull_request_reviewers(
        &self,
        repo: &str,
        number: u64,
    ) -> Result<Reviewers, String> {
        let author = self.pull_request_author(repo, number)?;
        let output =
            self.api([format!("repos/{repo}/pulls/{number}/requested_reviewers").as_str()])?;
        let requested = serde_json::from_slice::<RequestedReviewers>(&output)
            .map_err(|error| format!("requested reviewers of {repo}#{number}: {error}"))?;
        let output = self.api([
            "--paginate",
            &format!("repos/{repo}/pulls/{number}/reviews?per_page=100"),
        ])?;
        let reviews = decode_pages::<Vec<Review>>(&output)
            .map_err(|error| format!("reviews of {repo}#{number}: {error}"))?;

        Ok(Reviewers {
            author,
            requested: requested
                .users
                .into_iter()
                .filter(|user| !user.is_bot())
                .map(|user| user.login)
                .collect(),
            reviews: reviews
                .into_iter()
                .flatten()
                .filter_map(|review| {
                    let user = review.user.filter(|user| !user.is_bot())?;
                    Some(SubmittedReview {
                        author: user.login,
                        state: review.state,
                    })
                })
                .collect(),
        })
    }

    pub(crate) fn unread_notifications(&self) -> Result<Vec<InboxThread>, String> {
        let output = self.api([
            "--paginate",
            "-X",
            "GET",
            "notifications",
            "-f",
            "per_page=50",
        ])?;
        let pages = decode_pages::<Vec<Notification>>(&output)
            .map_err(|error| format!("notifications: {error}"))?;
        Ok(pages
            .into_iter()
            .flatten()
            .map(|notification| InboxThread {
                pr: (notification.subject.kind == "PullRequest")
                    .then(|| notification.subject.url.as_deref().and_then(pull_of_url))
                    .flatten(),
                id: notification.id,
                reason: notification.reason,
            })
            .collect())
    }

    pub(crate) fn unsubscribe_thread(&self, id: &str) -> Result<(), String> {
        self.api([
            "-X",
            "DELETE",
            &format!("notifications/threads/{id}/subscription"),
        ])
        .map(drop)
    }

    // Done, unlike read, takes the thread out of the inbox.
    pub(crate) fn mark_thread_done(&self, id: &str) -> Result<(), String> {
        self.api(["-X", "DELETE", &format!("notifications/threads/{id}")])
            .map(drop)
    }

    fn api<'argument>(
        &self,
        arguments: impl IntoIterator<Item = &'argument str>,
    ) -> Result<Vec<u8>, String> {
        (self.run)(&ToolCall {
            program: self.gh,
            arguments: iter::once("api")
                .chain(arguments)
                .map(OsString::from)
                .collect(),
            timeout: TIMEOUT,
            stop: self.stop,
        })
    }
}

impl Actor {
    fn is_bot(&self) -> bool {
        self.kind == "Bot" || self.login.ends_with("[bot]")
    }
}

fn pull_of_url(url: &str) -> Option<(String, u64)> {
    let path = url.strip_prefix(REPOSITORY_API)?;
    let (repo, number) = path.rsplit_once("/pulls/")?;
    let number = number.parse::<u64>().ok().filter(|number| *number > 0)?;
    is_repo_name(repo).then(|| (repo.to_owned(), number))
}

// `gh api --paginate` writes one JSON value per page, back to back.
fn decode_pages<T: DeserializeOwned>(output: &[u8]) -> serde_json::Result<Vec<T>> {
    serde_json::Deserializer::from_slice(output)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    struct FakeGh {
        output: String,
        asked: Mutex<Vec<Vec<String>>>,
    }

    #[test]
    fn search_reads_every_page_with_the_query_scoped_by_gh() {
        let page = |number: u64| {
            format!(
                r#"{{"incomplete_results":false,"items":[{{"number":{number},"title":"PR {number}","updated_at":"2026-10-20T10:00:00Z","repository_url":"https://api.github.com/repos/acme/app"}}]}}"#
            )
        };
        let gh = FakeGh::answering(format!("{}\n{}", page(3), page(4)));

        let hits = gh.rest(|rest| rest.search_pull_requests("is:pr user:acme"));

        let hits = hits.expect("the search answers");
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.repo.as_str(), hit.number))
                .collect::<Vec<_>>(),
            [("acme/app", 3), ("acme/app", 4)]
        );
        assert_eq!(hits[0].updated_at, "2026-10-20T10:00:00Z");
        assert_eq!(
            gh.asked(),
            [[
                "api",
                "--paginate",
                "-X",
                "GET",
                "search/issues",
                "-f",
                "q=is:pr user:acme",
                "-f",
                "per_page=100",
                "-f",
                "sort=updated"
            ]]
        );
    }

    #[test]
    fn search_answer_that_cannot_be_trusted_fails_the_search() {
        for (name, output) in [
            ("incomplete", r#"{"incomplete_results":true,"items":[]}"#),
            (
                "other_host",
                r#"{"incomplete_results":false,"items":[{"number":1,"title":"t","updated_at":"u","repository_url":"https://example.com/repos/acme/app"}]}"#,
            ),
        ] {
            let gh = FakeGh::answering(output.to_owned());

            let hits = gh.rest(|rest| rest.search_pull_requests("q"));

            assert!(hits.is_err(), "case: {name}: {hits:?}");
        }
    }

    #[test]
    fn review_requests_are_read_from_every_page_of_events() {
        let first = r#"[{"id":7,"event":"review_requested","review_requester":{"login":"tanaka"},"requested_reviewer":{"login":"me"}},{"id":8,"event":"labeled","label":{"name":"x"}}]"#;
        let second = r#"[{"id":9,"event":"review_requested","review_requester":null,"requested_team":{"slug":"backend"}}]"#;
        let gh = FakeGh::answering(format!("{first}{second}"));

        let requests = gh.rest(|rest| rest.review_requests("acme/app", 3));

        assert_eq!(
            requests,
            Ok(vec![
                ReviewRequest {
                    id: 7,
                    requester: Some("tanaka".to_owned()),
                    reviewer: Some("me".to_owned()),
                    team_slug: None,
                },
                ReviewRequest {
                    id: 9,
                    requester: None,
                    reviewer: None,
                    team_slug: Some("backend".to_owned()),
                },
            ])
        );
        assert_eq!(
            gh.asked(),
            [[
                "api",
                "--paginate",
                "repos/acme/app/issues/3/events?per_page=100"
            ]]
        );
    }

    #[test]
    fn pull_request_is_read_as_the_base_repository_names_it() {
        let gh = FakeGh::answering(pull_json("Acme/App", 12, HEAD));

        let pr = gh.rest(|rest| rest.pull_request("acme/app", 12));

        assert_eq!(
            pr,
            Ok(PullRequest {
                repo: "Acme/App".to_owned(),
                number: 12,
                title: "T".to_owned(),
                url: "https://github.com/Acme/App/pull/12".to_owned(),
                head_sha: HEAD.to_owned(),
                base_ref: "main".to_owned(),
            })
        );
    }

    #[test]
    fn pull_request_answer_for_another_pr_or_head_is_refused() {
        for (name, output) in [
            ("other_number", pull_json("acme/app", 13, HEAD)),
            ("other_repository", pull_json("acme/other", 12, HEAD)),
            ("short_sha", pull_json("acme/app", 12, "0123456")),
        ] {
            let gh = FakeGh::answering(output);

            let pr = gh.rest(|rest| rest.pull_request("acme/app", 12));

            assert!(pr.is_err(), "case: {name}: {pr:?}");
        }
    }

    #[test]
    fn reviewers_are_the_requested_users_and_the_review_authors_without_bots() {
        let stop = StopRequest::default();
        let asked = Mutex::new(Vec::new());
        let run = |call: &ToolCall<'_>| {
            let arguments = call
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let answer = match arguments.last().map(String::as_str) {
                Some("repos/acme/app/pulls/12") => r#"{"user":{"login":"tanaka"}}"#,
                Some("repos/acme/app/pulls/12/requested_reviewers") => {
                    r#"{"users":[{"login":"sato","type":"User"},{"login":"renovate[bot]","type":"Bot"}],"teams":[{"slug":"backend"}]}"#
                }
                Some("repos/acme/app/pulls/12/reviews?per_page=100") => {
                    r#"[{"user":{"login":"suzuki","type":"User"},"state":"APPROVED"},{"user":{"login":"copilot","type":"Bot"},"state":"COMMENTED"}][{"user":null,"state":"COMMENTED"}]"#
                }
                _ => return Err(format!("unexpected call: {arguments:?}")),
            };
            asked.lock().expect("note the call").push(arguments);
            Ok(answer.as_bytes().to_vec())
        };

        let reviewers = GhRest {
            run: &run,
            gh: Path::new("gh"),
            stop: &stop,
        }
        .pull_request_reviewers("acme/app", 12);

        assert_eq!(
            reviewers,
            Ok(Reviewers {
                author: Some("tanaka".to_owned()),
                requested: vec!["sato".to_owned()],
                reviews: vec![SubmittedReview {
                    author: "suzuki".to_owned(),
                    state: "APPROVED".to_owned(),
                }],
            })
        );
        assert_eq!(
            asked.into_inner().expect("read the calls")[2],
            [
                "api",
                "--paginate",
                "repos/acme/app/pulls/12/reviews?per_page=100"
            ]
        );
    }

    #[test]
    fn unread_notifications_name_the_pull_request_they_are_about() {
        let first = r#"[{"id":"1","reason":"review_requested","subject":{"type":"PullRequest","url":"https://api.github.com/repos/acme/app/pulls/12"}}]"#;
        let second = r#"[{"id":"2","reason":"mention","subject":{"type":"Issue","url":"https://api.github.com/repos/acme/app/issues/3"}},{"id":"3","reason":"ci_activity","subject":{"type":"CheckSuite","url":null}}]"#;
        let gh = FakeGh::answering(format!("{first}{second}"));

        let threads = gh.rest(|rest| rest.unread_notifications());

        let thread = |id: &str, reason: &str, pr: Option<u64>| InboxThread {
            id: id.to_owned(),
            reason: reason.to_owned(),
            pr: pr.map(|number| ("acme/app".to_owned(), number)),
        };
        assert_eq!(
            threads,
            Ok(vec![
                thread("1", "review_requested", Some(12)),
                thread("2", "mention", None),
                thread("3", "ci_activity", None),
            ])
        );
        assert_eq!(
            gh.asked(),
            [[
                "api",
                "--paginate",
                "-X",
                "GET",
                "notifications",
                "-f",
                "per_page=50"
            ]]
        );
    }

    #[test]
    fn thread_is_unsubscribed_then_marked_done() {
        let gh = FakeGh::answering(String::new());

        let cleared = gh.rest(|rest| {
            rest.unsubscribe_thread("42")
                .and_then(|()| rest.mark_thread_done("42"))
        });

        assert_eq!(cleared, Ok(()));
        assert_eq!(
            gh.asked(),
            [
                [
                    "api",
                    "-X",
                    "DELETE",
                    "notifications/threads/42/subscription"
                ]
                .as_slice(),
                ["api", "-X", "DELETE", "notifications/threads/42"].as_slice(),
            ]
        );
    }

    #[test]
    fn empty_login_is_not_an_account() {
        let gh = FakeGh::answering(r#"{"login":""}"#.to_owned());

        let login = gh.rest(|rest| rest.current_login());

        assert!(login.is_err(), "{login:?}");
    }

    impl FakeGh {
        fn answering(output: String) -> Self {
            Self {
                output,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn rest<T>(&self, read: impl FnOnce(GhRest<'_>) -> T) -> T {
            let stop = StopRequest::default();
            let run = |call: &ToolCall<'_>| {
                self.asked.lock().expect("note the call").push(
                    call.arguments
                        .iter()
                        .map(|argument| argument.to_string_lossy().into_owned())
                        .collect(),
                );
                Ok(self.output.clone().into_bytes())
            };
            read(GhRest {
                run: &run,
                gh: Path::new("gh"),
                stop: &stop,
            })
        }

        fn asked(&self) -> Vec<Vec<String>> {
            self.asked.lock().expect("read the calls").clone()
        }
    }

    fn pull_json(repo: &str, number: u64, head: &str) -> String {
        format!(
            r#"{{"number":{number},"title":"T","html_url":"https://github.com/{repo}/pull/{number}","head":{{"sha":"{head}"}},"base":{{"ref":"main","repo":{{"full_name":"{repo}"}}}}}}"#
        )
    }
}
