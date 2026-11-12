use std::fmt::{self, Display, Formatter};

use serde_json::{Map, Value};

use crate::app::watch::{
    pr::PrRef,
    reviewers::{BotProgress, ReviewBot},
};

// A list read past its first page, through the node that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    ReviewThreads,
    Reviews,
    Comments,
    ThreadComments,
    Contexts,
}

// A head gets one check suite per app that checks it, far fewer than this.
const MAX_SUITES: u32 = 50;

const COMMENT_NODE: &str = "databaseId url createdAt body path line originalLine
  author { __typename login } replyTo { databaseId }";

const PAGE_INFO: &str = "pageInfo { hasNextPage endCursor }";

const PLAIN_NODE: &str = "databaseId url createdAt body author { __typename login }";

const CONTEXT_NODE: &str = "__typename
  ... on StatusContext { context state targetUrl isRequired(pullRequestNumber: $number) }
  ... on CheckRun { name conclusion detailsUrl isRequired(pullRequestNumber: $number) }";

pub(super) const OPEN_QUERY: &str = "query($search: String!) {
  search(query: $search, type: ISSUE, first: 50) { nodes { ... on PullRequest { number } } }
}";

// GitHub charges one point per query however many PRs it reads, so an account's PRs share one aliased query.
pub(super) fn summary_query(prs: &[PrRef], bots: &[ReviewBot]) -> (String, Map<String, Value>) {
    let fields = summary_fields(bots);
    let mut variables = Map::new();
    let mut declared = Vec::with_capacity(prs.len());
    let pulls = prs
        .iter()
        .enumerate()
        .map(|(index, pr)| {
            let (owner, name) = split_repo(&pr.repo);
            variables.insert(format!("o{index}"), Value::from(owner));
            variables.insert(format!("r{index}"), Value::from(name));
            variables.insert(format!("n{index}"), Value::from(pr.number));
            declared.push(format!(
                "$o{index}: String!, $r{index}: String!, $n{index}: Int!"
            ));
            format!(
                "p{index}: repository(owner: $o{index}, name: $r{index}) {{ pullRequest(number: $n{index}) {{ {fields} }} }}"
            )
        })
        .collect::<Vec<_>>();
    (
        format!("query({}) {{ {} }}", declared.join(", "), pulls.join("\n")),
        variables,
    )
}

pub(super) fn detail_query(bots: &[ReviewBot]) -> String {
    let fields = summary_fields(bots);
    let thread = thread_node();
    let review = review_node();
    format!(
        "query($owner: String!, $name: String!, $number: Int!) {{
  repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{
    id url title {fields}
    checks: commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{
      id contexts(first: 100) {{ nodes {{ {CONTEXT_NODE} }} {PAGE_INFO} }}
    }} }} }} }}
    reviewThreads(first: 100) {{ nodes {{ {thread} }} {PAGE_INFO} }}
    reviews(first: 100) {{ nodes {{ {review} }} {PAGE_INFO} }}
    comments(first: 100) {{ nodes {{ {PLAIN_NODE} }} {PAGE_INFO} }}
  }} }}
}}"
    )
}

pub(super) fn split_repo(repo: &str) -> (&str, &str) {
    repo.split_once('/').unwrap_or((repo, ""))
}

impl Page {
    pub(super) fn query(self) -> String {
        let (on, field, node) = self.list();
        let number = if self == Self::Contexts {
            ", $number: Int!"
        } else {
            ""
        };
        format!(
            "query($id: ID!, $after: String!{number}) {{
  node(id: $id) {{ ... on {on} {{ {field}(first: 100, after: $after) {{ nodes {{ {node} }} {PAGE_INFO} }} }} }}
}}"
        )
    }

    pub(super) const fn field(self) -> &'static str {
        self.list().1
    }

    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::ReviewThreads => "reviewThreads",
            Self::Reviews => "reviews",
            Self::Comments => "comments",
            Self::ThreadComments => "threadComments",
            Self::Contexts => "contexts",
        }
    }

    const fn list(self) -> (&'static str, &'static str, Node) {
        match self {
            Self::ReviewThreads => ("PullRequest", "reviewThreads", Node::Thread),
            Self::Reviews => ("PullRequest", "reviews", Node::Review),
            Self::Comments => ("PullRequest", "comments", Node::Plain),
            Self::ThreadComments => ("PullRequestReviewThread", "comments", Node::Comment),
            Self::Contexts => ("StatusCheckRollup", "contexts", Node::Context),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Node {
    Thread,
    Review,
    Plain,
    Comment,
    Context,
}

impl Display for Node {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread => formatter.write_str(&thread_node()),
            Self::Review => formatter.write_str(&review_node()),
            Self::Plain => formatter.write_str(PLAIN_NODE),
            Self::Comment => formatter.write_str(COMMENT_NODE),
            Self::Context => formatter.write_str(CONTEXT_NODE),
        }
    }
}

fn summary_fields(bots: &[ReviewBot]) -> String {
    // Each bot's progress is read under an alias by its place in the list.
    let contexts = bots
        .iter()
        .enumerate()
        .filter_map(|(index, bot)| match &bot.status {
            Some(BotProgress::CommitStatus { context, .. }) => Some(format!(
                "b{index}: context(name: {}) {{ state description createdAt }}",
                quoted(context)
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    // A workflow's check suite has no stable id, so a check run is looked up in every suite of the head.
    let runs = bots
        .iter()
        .enumerate()
        .filter_map(|(index, bot)| match &bot.status {
            Some(BotProgress::CheckRun(name)) => Some(format!(
                "b{index}: checkSuites(first: {MAX_SUITES}) {{ nodes {{ checkRuns(last: 1, filterBy: {{ checkName: {} }}) {{ nodes {{ databaseId status conclusion startedAt completedAt }} }} }} }}",
                quoted(name)
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    let status = if contexts.is_empty() {
        String::new()
    } else {
        format!("status {{ {} }}", contexts.join(" "))
    };
    format!(
        "state isDraft headRefOid updatedAt mergeable
  commits(last: 1) {{ nodes {{ commit {{
    {status}
    {}
    statusCheckRollup {{ state }}
  }} }} }}",
        runs.join("\n    ")
    )
}

fn thread_node() -> String {
    format!("id isResolved comments(first: 100) {{ nodes {{ {COMMENT_NODE} }} {PAGE_INFO} }}")
}

// An issue comment has no state, so only reviews ask for it.
fn review_node() -> String {
    format!("{PLAIN_NODE} state")
}

// JSON's string escapes are valid GraphQL, so a name is quoted as a JSON string.
fn quoted(text: &str) -> String {
    Value::from(text).to_string()
}
