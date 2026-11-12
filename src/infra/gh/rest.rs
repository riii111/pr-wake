use std::{ffi::OsString, iter, path::Path, sync::LazyLock, time::Duration};

use regex::Regex;
use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    app::review::{
        PullRequest, is_repo_name,
        request::{ReviewRequest, SearchHit},
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

    // Search answers at most 1000 results; past that the least recently updated are not seen.
    pub(crate) fn search_pull_requests(&self, query: &str) -> Result<Vec<SearchHit>, String> {
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
        output: Result<String, String>,
        asked: Mutex<Vec<Vec<String>>>,
    }

    #[test]
    fn search_reads_every_page_with_the_query_scoped_by_gh() {
        let page = |number: u64| {
            format!(
                r#"{{"incomplete_results":false,"items":[{{"number":{number},"title":"PR {number}","updated_at":"2026-10-20T10:00:00Z","repository_url":"https://api.github.com/repos/acme/app"}}]}}"#
            )
        };
        let gh = FakeGh::answering(Ok(format!("{}\n{}", page(3), page(4))));

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
            (
                "number_zero",
                r#"{"incomplete_results":false,"items":[{"number":0,"title":"t","updated_at":"u","repository_url":"https://api.github.com/repos/acme/app"}]}"#,
            ),
            ("not_json", "rate limited"),
        ] {
            let gh = FakeGh::answering(Ok(output.to_owned()));

            let hits = gh.rest(|rest| rest.search_pull_requests("q"));

            assert!(hits.is_err(), "case: {name}: {hits:?}");
        }
    }

    #[test]
    fn review_requests_are_read_from_every_page_of_events() {
        let first = r#"[{"id":7,"event":"review_requested","review_requester":{"login":"tanaka"},"requested_reviewer":{"login":"me"}},{"id":8,"event":"labeled","label":{"name":"x"}}]"#;
        let second = r#"[{"id":9,"event":"review_requested","review_requester":null,"requested_team":{"slug":"backend"}}]"#;
        let gh = FakeGh::answering(Ok(format!("{first}{second}")));

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
        let gh = FakeGh::answering(Ok(pull_json("Acme/App", 12, HEAD)));

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
        assert_eq!(gh.asked(), [["api", "repos/acme/app/pulls/12"]]);
    }

    #[test]
    fn pull_request_answer_for_another_pr_or_head_is_refused() {
        for (name, output) in [
            ("other_number", pull_json("acme/app", 13, HEAD)),
            ("other_repository", pull_json("acme/other", 12, HEAD)),
            ("short_sha", pull_json("acme/app", 12, "0123456")),
        ] {
            let gh = FakeGh::answering(Ok(output));

            let pr = gh.rest(|rest| rest.pull_request("acme/app", 12));

            assert!(pr.is_err(), "case: {name}: {pr:?}");
        }
    }

    #[test]
    fn login_is_the_active_account() {
        for (name, output, expected) in [
            ("login", Ok(r#"{"login":"me"}"#), Ok("me")),
            ("no_login", Ok(r#"{"login":""}"#), Err(())),
            ("gh_failed", Err("gh: exit status 1"), Err(())),
        ] {
            let gh = FakeGh::answering(output.map(str::to_owned).map_err(str::to_owned));

            let login = gh.rest(|rest| rest.current_login());

            assert_eq!(login.as_deref().map_err(|_| ()), expected, "case: {name}");
        }
    }

    impl FakeGh {
        fn answering(output: Result<String, String>) -> Self {
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
                self.output.clone().map(String::into_bytes)
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
