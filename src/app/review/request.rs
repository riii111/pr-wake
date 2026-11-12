use std::collections::BTreeMap;

use super::state::{CachedPr, Found};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchHit {
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    // Changes with every new event, so an unchanged one means the PR's requests are as cached.
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewRequest {
    pub(crate) id: u64,
    pub(crate) requester: Option<String>,
    pub(crate) reviewer: Option<String>,
    pub(crate) team_slug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) found: Vec<Found>,
    pub(crate) prs: BTreeMap<String, CachedPr>,
}

// A search across all of GitHub answers incomplete results, so each search is scoped to one owner.
// `user:` matches an organization too.
#[must_use]
pub(crate) fn request_queries(owners: &[String], teams: &[String]) -> Vec<String> {
    owners
        .iter()
        .map(|owner| format!("is:pr is:open archived:false user-review-requested:@me user:{owner}"))
        .chain(
            teams
                .iter()
                .map(|team| format!("is:pr is:open archived:false team-review-requested:{team}")),
        )
        .collect()
}

pub(crate) fn pending_requests<E>(
    hits: Vec<SearchHit>,
    cache: &BTreeMap<String, CachedPr>,
    login: &str,
    teams: &[String],
    mut requests_of: impl FnMut(&SearchHit) -> Result<Vec<ReviewRequest>, E>,
) -> Result<Pending, E> {
    // A later search may answer for the same PR again, so the hits are keyed by PR and the later one is kept.
    let hits = hits
        .into_iter()
        .map(|hit| (pr_key(&hit.repo, hit.number), hit))
        .collect::<BTreeMap<_, _>>();

    let mut pending = Pending {
        found: Vec::with_capacity(hits.len()),
        prs: BTreeMap::new(),
    };
    for (key, hit) in hits {
        let cached = cache
            .get(&key)
            .filter(|cached| cached.updated_at == hit.updated_at)
            .and_then(|cached| cached.found.clone());
        let found = if let Some(found) = cached {
            found
        } else {
            let requests = requests_of(&hit)?;
            latest_request(&requests, &hit, login, teams).unwrap_or_else(|| Found {
                // No event names a request the search still lists, so it is kept under a PR-level key.
                key: format!("{key}@unknown"),
                repo: hit.repo.clone(),
                number: hit.number,
                title: hit.title.clone(),
                requester: None,
                team: None,
            })
        };

        pending.found.push(Found {
            title: hit.title.clone(),
            ..found.clone()
        });
        pending.prs.insert(
            key,
            CachedPr {
                updated_at: hit.updated_at,
                found: Some(found),
            },
        );
    }
    Ok(pending)
}

// Names the event's requester, never the PR author.
#[must_use]
pub(crate) fn latest_request(
    requests: &[ReviewRequest],
    hit: &SearchHit,
    login: &str,
    teams: &[String],
) -> Option<Found> {
    let owner = hit
        .repo
        .split_once('/')
        .map_or(hit.repo.as_str(), |(owner, _)| owner);
    let (latest, team) = requests
        .iter()
        .filter_map(|request| {
            let mine = request
                .reviewer
                .as_deref()
                .is_some_and(|reviewer| reviewer.eq_ignore_ascii_case(login));
            if mine {
                return Some((request, None));
            }
            let team = configured_team(teams, owner, request.team_slug.as_deref()?)?;
            Some((request, Some(team)))
        })
        .max_by_key(|(request, _)| request.id)?;

    Some(Found {
        key: format!("{}#{}@{}", hit.repo, hit.number, latest.id),
        repo: hit.repo.clone(),
        number: hit.number,
        title: hit.title.clone(),
        requester: latest.requester.clone(),
        team: team.map(str::to_owned),
    })
}

#[must_use]
pub(crate) fn pr_key(repo: &str, number: u64) -> String {
    format!("{}#{number}", repo.to_lowercase())
}

fn configured_team<'teams>(
    teams: &'teams [String],
    owner: &str,
    slug: &str,
) -> Option<&'teams str> {
    teams
        .iter()
        .find(|team| {
            team.split_once('/')
                .is_some_and(|(organization, team_slug)| {
                    organization.eq_ignore_ascii_case(owner) && team_slug.eq_ignore_ascii_case(slug)
                })
        })
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct LatestCase {
        name: &'static str,
        requests: Vec<ReviewRequest>,
        teams: &'static [&'static str],
        expected: Option<(&'static str, Option<&'static str>)>,
    }

    #[test]
    fn latest_request_to_the_login_or_a_configured_team_names_its_requester() {
        let team_requests = vec![
            team_request(1, "a", "backend"),
            team_request(2, "b", "frontend"),
        ];
        for case in [
            LatestCase {
                name: "latest_direct_request_wins",
                requests: vec![
                    direct(10, "tanaka", "me"),
                    direct(11, "sato", "someone-else"),
                    direct(12, "suzuki", "ME"),
                ],
                teams: &[],
                expected: Some(("acme/app#3@12", Some("suzuki"))),
            },
            LatestCase {
                name: "unknown_requester",
                requests: vec![ReviewRequest {
                    id: 20,
                    requester: None,
                    reviewer: Some("me".to_owned()),
                    team_slug: None,
                }],
                teams: &[],
                expected: Some(("acme/app#3@20", None)),
            },
            LatestCase {
                name: "configured_team",
                requests: team_requests.clone(),
                teams: &["acme/backend"],
                expected: Some(("acme/app#3@1", Some("a"))),
            },
            LatestCase {
                name: "team_of_another_organization",
                requests: team_requests,
                teams: &["other/backend"],
                expected: None,
            },
        ] {
            let teams = case
                .teams
                .iter()
                .map(|team| (*team).to_owned())
                .collect::<Vec<_>>();

            let found = latest_request(&case.requests, &hit("acme/app", 3, "u"), "me", &teams);

            let found = found.map(|found| (found.key, found.requester));
            let expected = case
                .expected
                .map(|(key, requester)| (key.to_owned(), requester.map(str::to_owned)));
            assert_eq!(found, expected, "case: {}", case.name);
        }
    }

    #[test]
    fn team_request_is_shown_with_the_configured_team() {
        let teams = vec!["Acme/Backend".to_owned()];

        let found = latest_request(
            &[team_request(1, "a", "backend")],
            &hit("acme/app", 3, "u"),
            "me",
            &teams,
        );

        assert_eq!(
            found.and_then(|found| found.team).as_deref(),
            Some("Acme/Backend")
        );
    }

    #[test]
    fn queries_are_scoped_to_the_configured_owners_then_the_teams() {
        let owners = vec!["acme".to_owned(), "zeta".to_owned()];
        let teams = vec!["acme/backend".to_owned()];

        let queries = request_queries(&owners, &teams);

        assert_eq!(
            queries,
            [
                "is:pr is:open archived:false user-review-requested:@me user:acme",
                "is:pr is:open archived:false user-review-requested:@me user:zeta",
                "is:pr is:open archived:false team-review-requested:acme/backend",
            ]
        );
    }

    // A re-request bumps the PR's updated_at, so the cache must not hide it.
    #[test]
    fn events_are_read_again_only_for_updated_prs() {
        let events = RefCell::new(vec![direct(7, "tanaka", "me")]);
        let reads = RefCell::new(0);
        let read = |_: &SearchHit| -> Result<Vec<ReviewRequest>, String> {
            *reads.borrow_mut() += 1;
            Ok(events.borrow().clone())
        };

        let first = pending_requests(
            vec![hit("acme/app", 3, "10:00")],
            &BTreeMap::new(),
            "me",
            &[],
            read,
        )
        .expect("first poll");
        let unchanged = pending_requests(
            vec![hit("acme/app", 3, "10:00")],
            &first.prs,
            "me",
            &[],
            read,
        )
        .expect("unchanged poll");
        let reads_before_update = *reads.borrow();
        events.borrow_mut().push(direct(8, "sato", "me"));

        let updated = pending_requests(
            vec![hit("acme/app", 3, "11:00")],
            &unchanged.prs,
            "me",
            &[],
            read,
        )
        .expect("updated poll");

        assert_eq!(keys(&first), ["acme/app#3@7"]);
        assert_eq!(keys(&unchanged), ["acme/app#3@7"]);
        assert_eq!(reads_before_update, 1);
        assert_eq!(keys(&updated), ["acme/app#3@8"]);
        assert_eq!(updated.found[0].requester.as_deref(), Some("sato"));
    }

    #[test]
    fn pending_pr_no_event_names_is_kept_under_an_unknown_request() {
        let pending = pending_requests(
            vec![hit("Acme/App", 3, "u")],
            &BTreeMap::new(),
            "me",
            &[],
            |_| Ok::<_, String>(Vec::new()),
        )
        .expect("poll");

        assert_eq!(keys(&pending), ["acme/app#3@unknown"]);
        assert_eq!(pending.found[0].repo, "Acme/App");
    }

    #[test]
    fn one_pr_answered_by_two_searches_is_one_request_with_the_latest_title() {
        let mut later = hit("acme/app", 3, "u");
        later.title = "Renamed".to_owned();

        let pending = pending_requests(
            vec![hit("acme/app", 3, "u"), later],
            &BTreeMap::new(),
            "me",
            &[],
            |_| Ok::<_, String>(vec![direct(7, "tanaka", "me")]),
        )
        .expect("poll");

        assert_eq!(keys(&pending), ["acme/app#3@7"]);
        assert_eq!(pending.found[0].title, "Renamed");
    }

    #[test]
    fn failed_event_read_fails_the_poll() {
        let pending = pending_requests(
            vec![hit("acme/app", 3, "u")],
            &BTreeMap::new(),
            "me",
            &[],
            |_| Err::<Vec<ReviewRequest>, _>("HTTP 502".to_owned()),
        );

        assert_eq!(pending, Err("HTTP 502".to_owned()));
    }

    fn direct(id: u64, requester: &str, reviewer: &str) -> ReviewRequest {
        ReviewRequest {
            id,
            requester: Some(requester.to_owned()),
            reviewer: Some(reviewer.to_owned()),
            team_slug: None,
        }
    }

    fn team_request(id: u64, requester: &str, slug: &str) -> ReviewRequest {
        ReviewRequest {
            id,
            requester: Some(requester.to_owned()),
            reviewer: None,
            team_slug: Some(slug.to_owned()),
        }
    }

    fn hit(repo: &str, number: u64, updated_at: &str) -> SearchHit {
        SearchHit {
            repo: repo.to_owned(),
            number,
            title: format!("PR {number}"),
            updated_at: updated_at.to_owned(),
        }
    }

    fn keys(pending: &Pending) -> Vec<&str> {
        pending
            .found
            .iter()
            .map(|found| found.key.as_str())
            .collect()
    }
}
