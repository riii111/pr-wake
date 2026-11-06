import { Result } from "better-result";

import {
  type GraphqlAnswer,
  postGraphql,
} from "../github/graphql.boundary.ts";
import { parseJson } from "../lib/json.boundary.ts";
import { isObject } from "../lib/object.ts";
import type {
  CommandFailed,
  CommandOutput,
} from "../lib/process.boundary.ts";
import type { ItemKind } from "./config.ts";
import {
  GithubAuthFailed,
  GithubRejected,
  GithubUnreachable,
  type WatchFailure,
} from "./failure.ts";
import { botOf, type ReviewBot } from "./reviewers.ts";
import type { BotStatus, FailedCheck, PrSummary } from "./state.ts";

// The review bots are passed on every read, since the config is read again each cycle.
export type GitHub = {
  summaries: (
    account: string,
    refs: readonly PrRef[],
    bots: readonly ReviewBot[],
  ) => Promise<Result<(PrSummary | null)[], WatchFailure>>;
  detail: (
    account: string,
    ref: PrRef,
    bots: readonly ReviewBot[],
  ) => Promise<Result<PrDetail, WatchFailure>>;
  openPulls: (
    account: string,
    repo: string,
  ) => Promise<Result<PrRef[], WatchFailure>>;
};

type Env = Record<string, string | undefined>;

export type RunCommand = (
  argv: readonly string[],
  env: Env,
) => Promise<Result<CommandOutput, CommandFailed>>;

export type PrRef = { repo: string; number: number };

export type PrDetail = PrSummary & {
  url: string;
  title: string;
  failedRequired: FailedCheck[];
  comments: PrComment[];
  // The ids of the bots whose PR comment says they found nothing.
  noActionable: string[];
};

export type PrComment = {
  id: string;
  kind: ItemKind;
  author: string;
  url: string;
  createdAt: string;
  body: string;
  path: string | null;
  line: number | null;
  reply: boolean;
  resolved: boolean;
  approved: boolean;
};

type Post = (
  token: string,
  query: string,
  variables: Record<string, unknown>,
) => Promise<Result<GraphqlAnswer, { message: string }>>;

const rejected = (message: string) =>
  Result.err(new GithubRejected({ message }));

export const createGitHub = (
  runCommand: RunCommand,
  env: Env,
  post: Post = postGraphql,
  now: () => number = Date.now,
): GitHub => {
  const tokens = new Map<string, { value: string; at: number }>();
  const token = async (account: string) => {
    const kept = tokens.get(account);
    // Reading the token starts gh, which on macOS starts the keychain tool too, so it is not read for every request.
    if (kept !== undefined && now() - kept.at < TOKEN_TTL_MS)
      return Result.ok(kept.value);

    const ran = await runCommand(
      ["gh", "auth", "token", "--hostname", "github.com", "--user", account],
      env,
    );
    if (ran.isErr()) {
      return Result.err(
        new GithubAuthFailed({
          cause: ran.error,
          message: `gh auth token: ${ran.error.message}`,
        }),
      );
    }

    const value = ran.value.stdout.trim();
    if (ran.value.status !== 0 || value === "")
      return Result.err(
        new GithubAuthFailed({
          message: `gh has no token for account ${account}`,
        }),
      );
    // Kept in memory for a few minutes only, so a token the owner rotates is picked up and it never reaches the state or the log.
    tokens.set(account, { value, at: now() });
    return Result.ok(value);
  };

  const query = (
    account: string,
    text: string,
    variables: Record<string, unknown>,
  ) =>
    Result.gen(async function* () {
      const secret = yield* Result.await(token(account));
      const answer = await post(secret, text, variables);
      if (answer.isErr()) {
        return Result.err(
          new GithubUnreachable({
            cause: answer.error,
            message: answer.error.message,
          }),
        );
      }

      const { status } = answer.value;
      // A rejected token is read again on the next request.
      if (status === 401) tokens.delete(account);
      const parsed = parseJson(answer.value.text);
      const body = parsed.isOk() ? parsed.value : null;
      if (!isObject(body))
        return rejected(`GitHub answered ${status} without JSON`);
      if (Array.isArray(body.errors) && body.errors.length > 0)
        return rejected(`GitHub answered ${status}: ${errorsOf(body.errors)}`);
      if (status !== 200 || !isObject(body.data))
        return rejected(`GitHub answered ${status}`);
      return Result.ok(body.data);
    });

  const readAllPages = (
    account: string,
    ref: PrRef,
    pull: Record<string, unknown>,
  ) =>
    Result.gen(async function* () {
      const nodes = (page: Page, owner: unknown, connection: unknown) =>
        allNodes(account, ref, page, owner, connection);

      const threads = yield* Result.await(
        nodes("reviewThreads", pull.id, pull.reviewThreads),
      );
      for (const thread of threads)
        thread.comments = {
          nodes: yield* Result.await(
            nodes("threadComments", thread.id, thread.comments),
          ),
        };

      const rollup = lastCommit(pull.checks).statusCheckRollup;
      const contexts = isObject(rollup)
        ? yield* Result.await(nodes("contexts", rollup.id, rollup.contexts))
        : [];

      return Result.ok({
        ...pull,
        reviewThreads: { nodes: threads },
        reviews: {
          nodes: yield* Result.await(nodes("reviews", pull.id, pull.reviews)),
        },
        comments: {
          nodes: yield* Result.await(nodes("comments", pull.id, pull.comments)),
        },
        checks: {
          nodes: [
            {
              commit: { statusCheckRollup: { contexts: { nodes: contexts } } },
            },
          ],
        },
      });
    });

  const allNodes = (
    account: string,
    ref: PrRef,
    page: Page,
    owner: unknown,
    first: unknown,
  ) =>
    Result.gen(async function* () {
      const nodes = nodesOf(first);
      let info = pageInfoOf(first);
      for (let read = 1; info.next !== null; read += 1) {
        if (read >= MAX_PAGES)
          return rejected(
            `${ref.repo}#${ref.number} has more than ${MAX_PAGES} pages of ${page}`,
          );

        const data = yield* Result.await(
          query(account, PAGE_QUERIES[page], {
            id: owner,
            after: info.next,
            ...(page === "contexts" && { number: ref.number }),
          }),
        );
        const connection = isObject(data.node)
          ? data.node[PAGES[page].field]
          : null;
        nodes.push(...nodesOf(connection));
        info = pageInfoOf(connection);
      }

      return Result.ok(nodes);
    });

  return {
    summaries: (account, refs, bots) =>
      Result.gen(async function* () {
        if (refs.length === 0) return Result.ok([]);

        const { text, variables } = summaryQuery(refs, bots);
        const data = yield* Result.await(query(account, text, variables));
        return Result.ok(
          refs.map((_, index) => {
            const pull = pullOf(data[`p${index}`]);
            return pull === null ? null : summaryOf(pull, bots);
          }),
        );
      }),

    detail: (account, ref, bots) =>
      Result.gen(async function* () {
        const [owner, name] = ref.repo.split("/");
        const data = yield* Result.await(
          query(account, detailQuery(bots), {
            owner,
            name,
            number: ref.number,
          }),
        );

        const pull = pullOf(data.repository);
        if (pull === null)
          return rejected(
            `${ref.repo}#${ref.number} is not visible to ${account}`,
          );
        return Result.ok(
          detailOf(yield* Result.await(readAllPages(account, ref, pull)), bots),
        );
      }),

    openPulls: (account, repo) =>
      Result.gen(async function* () {
        const data = yield* Result.await(
          query(account, OPEN_QUERY, {
            search: `repo:${repo} is:pr is:open draft:false author:${account}`,
          }),
        );

        const search = isObject(data.search) ? data.search : {};
        return Result.ok(
          nodesOf(search).flatMap((node) =>
            typeof node.number === "number"
              ? [{ repo, number: node.number }]
              : [],
          ),
        );
      }),
  };
};

const errorsOf = (errors: unknown[]) =>
  errors
    .map((error) =>
      isObject(error) && typeof error.message === "string"
        ? error.message
        : JSON.stringify(error),
    )
    .join("; ");

const TOKEN_TTL_MS = 10 * 60_000;

const summaryFields = (bots: readonly ReviewBot[]) => {
  // Each bot's progress is read under an alias by its place in the list; JSON's string escapes are valid GraphQL.
  const contexts = bots.flatMap((bot, index) =>
    bot.status === null || !("context" in bot.status)
      ? []
      : [
          `b${index}: context(name: ${JSON.stringify(bot.status.context)}) { state description createdAt }`,
        ],
  );
  // A check run is looked up in every check suite of the head, since the suite of the workflow that runs it has no stable id.
  const runs = bots.flatMap((bot, index) =>
    bot.status === null || !("checkRun" in bot.status)
      ? []
      : [
          `b${index}: checkSuites(first: ${MAX_SUITES}) { nodes { checkRuns(last: 1, filterBy: { checkName: ${JSON.stringify(bot.status.checkRun)} }) { nodes { databaseId status conclusion startedAt completedAt } } } }`,
        ],
  );
  const status =
    contexts.length === 0 ? "" : `status { ${contexts.join(" ")} }`;
  return `state isDraft headRefOid updatedAt mergeable
  commits(last: 1) { nodes { commit {
    ${status}
    ${runs.join("\n    ")}
    statusCheckRollup { state }
  } } }`;
};

// A head gets one check suite per app that checks it, far fewer than this.
const MAX_SUITES = 50;

// One aliased query for every PR of an account.
const summaryQuery = (refs: readonly PrRef[], bots: readonly ReviewBot[]) => {
  const fields = summaryFields(bots);
  const variables: Record<string, unknown> = {};
  const declared: string[] = [];
  // GitHub counts one aliased query as a single point, however many PRs it reads.
  const pulls = refs.map((ref, index) => {
    const [owner, name] = ref.repo.split("/");
    Object.assign(variables, {
      [`o${index}`]: owner,
      [`r${index}`]: name,
      [`n${index}`]: ref.number,
    });
    declared.push(`$o${index}: String!, $r${index}: String!, $n${index}: Int!`);
    return `p${index}: repository(owner: $o${index}, name: $r${index}) { pullRequest(number: $n${index}) { ${fields} } }`;
  });

  return {
    text: `query(${declared.join(", ")}) { ${pulls.join("\n")} }`,
    variables,
  };
};

const COMMENT_NODE = `databaseId url createdAt body path line originalLine
  author { __typename login } replyTo { databaseId }`;

const PAGE_INFO = "pageInfo { hasNextPage endCursor }";

const THREAD_NODE = `id isResolved comments(first: 100) { nodes { ${COMMENT_NODE} } ${PAGE_INFO} }`;

const PLAIN_NODE = "databaseId url createdAt body author { __typename login }";

// An issue comment has no state, so only reviews ask for it.
const REVIEW_NODE = `${PLAIN_NODE} state`;

const CONTEXT_NODE = `__typename
  ... on StatusContext { context state targetUrl isRequired(pullRequestNumber: $number) }
  ... on CheckRun { name conclusion detailsUrl isRequired(pullRequestNumber: $number) }`;

const detailQuery = (
  bots: readonly ReviewBot[],
) => `query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) {
    id url title ${summaryFields(bots)}
    checks: commits(last: 1) { nodes { commit { statusCheckRollup {
      id contexts(first: 100) { nodes { ${CONTEXT_NODE} } ${PAGE_INFO} }
    } } } }
    reviewThreads(first: 100) { nodes { ${THREAD_NODE} } ${PAGE_INFO} }
    reviews(first: 100) { nodes { ${REVIEW_NODE} } ${PAGE_INFO} }
    comments(first: 100) { nodes { ${PLAIN_NODE} } ${PAGE_INFO} }
  } }
}`;

// Each later page is read through the node that owns the list.
const PAGES = {
  reviewThreads: {
    on: "PullRequest",
    field: "reviewThreads",
    node: THREAD_NODE,
  },
  reviews: { on: "PullRequest", field: "reviews", node: REVIEW_NODE },
  comments: { on: "PullRequest", field: "comments", node: PLAIN_NODE },
  threadComments: {
    on: "PullRequestReviewThread",
    field: "comments",
    node: COMMENT_NODE,
  },
  contexts: { on: "StatusCheckRollup", field: "contexts", node: CONTEXT_NODE },
} as const;

type Page = keyof typeof PAGES;

const PAGE_QUERIES = Object.fromEntries(
  Object.entries(PAGES).map(([page, { on, field, node }]) => [
    page,
    `query($id: ID!, $after: String!${page === "contexts" ? ", $number: Int!" : ""}) {
  node(id: $id) { ... on ${on} { ${field}(first: 100, after: $after) { nodes { ${node} } ${PAGE_INFO} } } }
}`,
  ]),
) as Record<Page, string>;

// A bound on the reads for one PR, so a list that never ends cannot keep the watch reading.
const MAX_PAGES = 50;

const pageInfoOf = (connection: unknown) => {
  const info = isObject(connection) ? connection.pageInfo : null;
  return {
    next:
      isObject(info) &&
      info.hasNextPage === true &&
      typeof info.endCursor === "string"
        ? info.endCursor
        : null,
  };
};

const OPEN_QUERY = `query($search: String!) {
  search(query: $search, type: ISSUE, first: 50) { nodes { ... on PullRequest { number } } }
}`;

const pullOf = (repository: unknown) =>
  isObject(repository) && isObject(repository.pullRequest)
    ? repository.pullRequest
    : null;

const summaryOf = (
  pull: Record<string, unknown>,
  bots: readonly ReviewBot[],
): PrSummary => {
  const commit = lastCommit(pull.commits);
  const status = isObject(commit.status) ? commit.status : {};
  const rollup = isObject(commit.statusCheckRollup)
    ? commit.statusCheckRollup.state
    : null;

  return {
    state: text(pull.state),
    isDraft: pull.isDraft === true,
    head: text(pull.headRefOid),
    updatedAt: text(pull.updatedAt),
    mergeable: text(pull.mergeable),
    bots: Object.fromEntries(
      bots.flatMap((bot, index) => {
        if (bot.status === null) return [];
        const alias = `b${index}`;
        return [
          [
            bot.id,
            "context" in bot.status
              ? botStatusOf(status[alias])
              : checkRunStatusOf(commit[alias]),
          ],
        ];
      }),
    ),
    rollup: typeof rollup === "string" ? rollup : null,
  };
};

const checkRunStatusOf = (suites: unknown): BotStatus | null => {
  const runs = nodesOf(suites).flatMap((suite) => nodesOf(suite.checkRuns));
  // A run started again is a later run of the same name, so the latest one counts; ids grow with creation, while a queued run has no start time yet.
  const run = runs.reduce<Record<string, unknown> | null>(
    (latest, next) =>
      latest === null || idOf(next) > idOf(latest) ? next : latest,
    null,
  );
  if (run === null) return null;

  const completed = run.status === "COMPLETED";
  return {
    state: completed ? text(run.conclusion) : "PENDING",
    description: "",
    createdAt: text(completed ? run.completedAt : run.startedAt),
  };
};

const botStatusOf = (context: unknown): BotStatus | null =>
  isObject(context)
    ? {
        state: text(context.state),
        description: text(context.description),
        createdAt: text(context.createdAt),
      }
    : null;

const detailOf = (
  pull: Record<string, unknown>,
  bots: readonly ReviewBot[],
): PrDetail => {
  const inline = nodesOf(pull.reviewThreads).flatMap((thread) =>
    nodesOf(thread.comments).map((node) =>
      inlineOf(node, thread.isResolved === true),
    ),
  );
  const reviews = nodesOf(pull.reviews).map(plainOf("review", "review"));
  const issueComments = nodesOf(pull.comments).map(
    plainOf("issue_comment", "issue"),
  );

  return {
    ...summaryOf(pull, bots),
    url: text(pull.url),
    title: text(pull.title),
    failedRequired: failedRequired(pull, bots),
    comments: [...inline, ...reviews, ...issueComments],
    noActionable: bots.flatMap(({ id, noActionable }) =>
      noActionable !== null &&
      issueComments.some(
        (comment) =>
          botOf(bots, comment.author)?.id === id &&
          comment.body.includes(noActionable),
      )
        ? [id]
        : [],
    ),
  };
};

const inlineOf = (
  node: Record<string, unknown>,
  resolved: boolean,
): PrComment => {
  const line = node.line ?? node.originalLine;
  return {
    id: `inline:${String(node.databaseId)}`,
    kind: "inline",
    author: loginOf(node.author),
    url: text(node.url),
    createdAt: text(node.createdAt),
    body: text(node.body),
    path: typeof node.path === "string" ? node.path : null,
    line: typeof line === "number" ? line : null,
    reply: isObject(node.replyTo),
    resolved,
    approved: false,
  };
};

const plainOf =
  (kind: "review" | "issue_comment", prefix: string) =>
  (node: Record<string, unknown>): PrComment => ({
    id: `${prefix}:${String(node.databaseId)}`,
    kind,
    author: loginOf(node.author),
    url: text(node.url),
    createdAt: text(node.createdAt),
    body: text(node.body),
    path: null,
    line: null,
    reply: false,
    resolved: false,
    approved: node.state === "APPROVED",
  });

const loginOf = (author: unknown) => {
  if (!isObject(author) || typeof author.login !== "string") return "ghost";
  // GraphQL drops the [bot] suffix that REST, the web and the ignore list use for apps.
  return author.__typename === "Bot" ? `${author.login}[bot]` : author.login;
};

const failedRequired = (
  pull: Record<string, unknown>,
  bots: readonly ReviewBot[],
): FailedCheck[] => {
  const rollup = lastCommit(pull.checks).statusCheckRollup;
  const contexts = nodesOf(isObject(rollup) ? rollup.contexts : null);
  // A bot's own status tells its review's progress, not a failure of the PR.
  const statuses = new Set(
    bots.flatMap((bot) =>
      bot.status !== null && "context" in bot.status ? bot.status.context : [],
    ),
  );
  const runs = new Set(
    bots.flatMap((bot) =>
      bot.status !== null && "checkRun" in bot.status
        ? bot.status.checkRun
        : [],
    ),
  );

  return contexts.flatMap((context) => {
    if (context.isRequired !== true) return [];
    if (context.__typename === "CheckRun")
      return FAILED_RUNS.has(text(context.conclusion)) &&
        !runs.has(text(context.name))
        ? [{ name: text(context.name), url: linkOf(context.detailsUrl) }]
        : [];
    return FAILED_STATUSES.has(text(context.state)) &&
      !statuses.has(text(context.context))
      ? [{ name: text(context.context), url: linkOf(context.targetUrl) }]
      : [];
  });
};

// A cancelled run is left out, since a newer push cancels the runs it replaces.
const FAILED_RUNS = new Set([
  "FAILURE",
  "TIMED_OUT",
  "STARTUP_FAILURE",
  "ACTION_REQUIRED",
]);
const FAILED_STATUSES = new Set(["FAILURE", "ERROR"]);

const lastCommit = (commits: unknown) => {
  const commit = nodesOf(commits)[0]?.commit;
  return isObject(commit) ? commit : {};
};

const nodesOf = (connection: unknown): Record<string, unknown>[] =>
  isObject(connection) && Array.isArray(connection.nodes)
    ? connection.nodes.filter(isObject)
    : [];

const text = (value: unknown) => (typeof value === "string" ? value : "");

const linkOf = (value: unknown) => (typeof value === "string" ? value : null);

const idOf = (node: Record<string, unknown>) =>
  typeof node.databaseId === "number" ? node.databaseId : -1;
