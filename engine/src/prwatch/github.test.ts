import { describe, expect, test } from "bun:test";

import { Result } from "better-result";

import { createGitHub } from "./github.ts";
import { PRESETS } from "./reviewers.ts";

const PR = "https://github.com/org/pay/pull/7";

const BOTS = [PRESETS.coderabbit];

const fake = (data: unknown, status = 200) => {
  const posts: {
    token: string;
    query: string;
    variables: Record<string, unknown>;
  }[] = [];
  const commands: string[][] = [];
  const clock = { now: 0 };
  const github = createGitHub(
    async (argv) => {
      commands.push([...argv]);
      return Result.ok({ status: 0, stdout: "secret-token\n", stderr: "" });
    },
    {},
    async (token, query, variables) => {
      posts.push({ token, query, variables });
      return Result.ok({ status, text: JSON.stringify(data) });
    },
    () => clock.now,
  );
  return { github, posts, commands, clock };
};

const commit = {
  oid: "a".repeat(40),
  status: {
    b0: {
      state: "SUCCESS",
      description: "Review completed",
      createdAt: "2026-10-23T00:00:00Z",
    },
  },
  statusCheckRollup: { state: "FAILURE" },
};

const pull = {
  url: PR,
  title: "Pay",
  state: "OPEN",
  isDraft: false,
  headRefOid: "a".repeat(40),
  updatedAt: "2026-10-23T00:00:00Z",
  mergeable: "MERGEABLE",
  commits: { nodes: [{ commit }] },
  checks: {
    nodes: [
      {
        commit: {
          statusCheckRollup: {
            contexts: {
              nodes: [
                {
                  __typename: "CheckRun",
                  name: "build",
                  conclusion: "FAILURE",
                  detailsUrl: "https://ci.example/1",
                  isRequired: true,
                },
                {
                  __typename: "CheckRun",
                  name: "label",
                  conclusion: "FAILURE",
                  isRequired: false,
                },
                {
                  __typename: "CheckRun",
                  name: "superseded",
                  conclusion: "CANCELLED",
                  isRequired: true,
                },
                {
                  __typename: "StatusContext",
                  context: "CodeRabbit",
                  state: "FAILURE",
                  targetUrl: null,
                  isRequired: true,
                },
                {
                  __typename: "StatusContext",
                  context: "deploy",
                  state: "ERROR",
                  targetUrl: null,
                  isRequired: true,
                },
              ],
            },
          },
        },
      },
    ],
  },
  reviewThreads: {
    nodes: [
      {
        isResolved: true,
        comments: {
          nodes: [
            {
              databaseId: 11,
              url: `${PR}#discussion_r11`,
              createdAt: "2026-10-23T00:00:00Z",
              body: "**🟠 Major**",
              path: "a.kt",
              line: null,
              originalLine: 4,
              author: { __typename: "Bot", login: "coderabbitai" },
              replyTo: null,
            },
            {
              databaseId: 12,
              url: `${PR}#discussion_r12`,
              createdAt: "2026-10-23T00:01:00Z",
              body: "thanks",
              path: "a.kt",
              line: 4,
              author: { __typename: "User", login: "owner" },
              replyTo: { databaseId: 11 },
            },
          ],
        },
      },
    ],
  },
  reviews: {
    nodes: [
      {
        databaseId: 21,
        url: `${PR}#pullrequestreview-21`,
        createdAt: "2026-10-23T00:00:00Z",
        body: "",
        author: null,
        state: "APPROVED",
      },
    ],
  },
  comments: {
    nodes: [
      {
        databaseId: 31,
        url: `${PR}#issuecomment-31`,
        createdAt: "2026-10-23T00:00:00Z",
        body: "No actionable comments were generated in the recent review. 🎉",
        author: { __typename: "Bot", login: "coderabbitai" },
      },
    ],
  },
};

describe("createGitHub", () => {
  test("reads a PR's detail with the account's token and names apps as GitHub shows them", async () => {
    const { github, posts, commands } = fake({
      data: { repository: { pullRequest: pull } },
    });

    const detail = (
      await github.detail("owner", { repo: "org/pay", number: 7 }, BOTS)
    ).unwrap();

    expect(commands).toEqual([
      ["gh", "auth", "token", "--hostname", "github.com", "--user", "owner"],
    ]);
    expect(posts.map((post) => [post.token, post.variables])).toEqual([
      ["secret-token", { owner: "org", name: "pay", number: 7 }],
    ]);
    expect(
      detail.comments.map((comment) => [
        comment.id,
        comment.author,
        comment.line,
        comment.reply,
        comment.resolved,
        comment.approved,
      ]),
    ).toEqual([
      ["inline:11", "coderabbitai[bot]", 4, false, true, false],
      ["inline:12", "owner", 4, true, true, false],
      ["review:21", "ghost", null, false, false, true],
      ["issue:31", "coderabbitai[bot]", null, false, false, false],
    ]);
    // An issue comment has no state, so asking for one would fail the whole query.
    expect(posts[0]?.query).toContain(
      "reviews(first: 100) { nodes { databaseId url createdAt body author { __typename login } state }",
    );
    expect(posts[0]?.query).toContain(
      "comments(first: 100) { nodes { databaseId url createdAt body author { __typename login } }",
    );
    expect(detail.failedRequired).toEqual([
      { name: "build", url: "https://ci.example/1" },
      { name: "deploy", url: null },
    ]);
    expect([detail.bots.coderabbit?.description, detail.noActionable]).toEqual([
      "Review completed",
      ["coderabbit"],
    ]);
  });

  test("reads every later page of threads, their comments, reviews, comments and checks", async () => {
    const more = { hasNextPage: true, endCursor: "next" };
    const last = { hasNextPage: false, endCursor: null };
    const inline = (id: number, replyTo: number | null) => ({
      databaseId: id,
      url: `${PR}#discussion_r${id}`,
      createdAt: "2026-10-23T00:00:00Z",
      body: "",
      path: "a.kt",
      line: 1,
      author: { __typename: "User", login: "tanaka" },
      replyTo: replyTo === null ? null : { databaseId: replyTo },
    });
    const plain = (id: number) => ({
      databaseId: id,
      url: `${PR}#c${id}`,
      createdAt: "2026-10-23T00:00:00Z",
      body: "x",
      author: { __typename: "User", login: "tanaka" },
    });
    const run = (name: string) => ({
      __typename: "CheckRun",
      name,
      conclusion: "FAILURE",
      isRequired: true,
    });
    const pages: Record<string, unknown> = {
      pr: {
        reviewThreads: {
          nodes: [
            {
              id: "t2",
              isResolved: false,
              comments: { nodes: [inline(2, null)], pageInfo: last },
            },
          ],
          pageInfo: last,
        },
        reviews: { nodes: [plain(21)], pageInfo: last },
        comments: { nodes: [plain(31)], pageInfo: last },
      },
      t1: { comments: { nodes: [inline(12, 11)], pageInfo: last } },
      rollup: { contexts: { nodes: [run("e2e")], pageInfo: last } },
    };
    const first = {
      ...pull,
      id: "pr",
      checks: {
        nodes: [
          {
            commit: {
              statusCheckRollup: {
                id: "rollup",
                contexts: { nodes: [run("build")], pageInfo: more },
              },
            },
          },
        ],
      },
      reviewThreads: {
        nodes: [
          {
            id: "t1",
            isResolved: false,
            comments: { nodes: [inline(11, null)], pageInfo: more },
          },
        ],
        pageInfo: more,
      },
      reviews: { nodes: [plain(20)], pageInfo: more },
      comments: { nodes: [plain(30)], pageInfo: more },
    };
    const asked: unknown[] = [];
    const github = createGitHub(
      async () => Result.ok({ status: 0, stdout: "token\n", stderr: "" }),
      {},
      async (_token, _query, variables) => {
        asked.push(variables.id ?? "detail");
        const data =
          variables.id === undefined
            ? { repository: { pullRequest: first } }
            : { node: pages[String(variables.id)] };
        return Result.ok({ status: 200, text: JSON.stringify({ data }) });
      },
    );

    const detail = (
      await github.detail("owner", { repo: "org/pay", number: 7 }, BOTS)
    ).unwrap();

    expect(detail.comments.map((comment) => comment.id)).toEqual([
      "inline:11",
      "inline:12",
      "inline:2",
      "review:20",
      "review:21",
      "issue:30",
      "issue:31",
    ]);
    expect(detail.failedRequired.map((check) => check.name)).toEqual([
      "build",
      "e2e",
    ]);
    expect(asked).toEqual(["detail", "pr", "t1", "rollup", "pr", "pr"]);
  });

  test("reads every PR of an account in one aliased query", async () => {
    const { github, posts } = fake({
      data: {
        p0: { pullRequest: pull },
        p1: { pullRequest: null },
      },
    });

    const summaries = (
      await github.summaries(
        "owner",
        [
          { repo: "org/pay", number: 7 },
          { repo: "org/gone", number: 8 },
        ],
        BOTS,
      )
    ).unwrap();

    expect(posts).toHaveLength(1);
    expect(posts[0]?.variables).toEqual({
      o0: "org",
      r0: "pay",
      n0: 7,
      o1: "org",
      r1: "gone",
      n1: 8,
    });
    expect(posts[0]?.query).toContain('b0: context(name: "CodeRabbit")');
    expect(summaries[0]?.rollup).toBe("FAILURE");
    expect(summaries[0]?.bots.coderabbit?.state).toBe("SUCCESS");
    expect(summaries[1]).toBeNull();
  });

  test.each([
    {
      name: "a run still in progress",
      runs: [run(1, "IN_PROGRESS", null, "2026-10-23T00:00:00Z", null)],
      expected: {
        state: "PENDING",
        description: "",
        createdAt: "2026-10-23T00:00:00Z",
      },
    },
    {
      name: "a run started again after one that failed",
      runs: [
        run(
          1,
          "COMPLETED",
          "FAILURE",
          "2026-10-23T00:00:00Z",
          "2026-10-23T00:01:00Z",
        ),
        run(
          2,
          "COMPLETED",
          "SUCCESS",
          "2026-10-23T00:05:00Z",
          "2026-10-23T00:06:00Z",
        ),
      ],
      expected: {
        state: "SUCCESS",
        description: "",
        createdAt: "2026-10-23T00:06:00Z",
      },
    },
    {
      name: "a queued run with no start time after one that succeeded",
      runs: [
        run(
          1,
          "COMPLETED",
          "SUCCESS",
          "2026-10-23T00:00:00Z",
          "2026-10-23T00:01:00Z",
        ),
        run(2, "QUEUED", null, null, null),
      ],
      expected: { state: "PENDING", description: "", createdAt: "" },
    },
    {
      name: "a skipped run with no start time after one that succeeded",
      runs: [
        run(
          1,
          "COMPLETED",
          "SUCCESS",
          "2026-10-23T00:00:00Z",
          "2026-10-23T00:01:00Z",
        ),
        run(2, "COMPLETED", "SKIPPED", null, "2026-10-23T00:05:00Z"),
      ],
      expected: {
        state: "SKIPPED",
        description: "",
        createdAt: "2026-10-23T00:05:00Z",
      },
    },
    { name: "no run of that name", runs: [], expected: null },
  ])(
    "reads a check run bot's progress from $name",
    async ({ runs, expected }) => {
      const suites = {
        nodes: [
          { checkRuns: { nodes: runs.slice(0, 1) } },
          { checkRuns: { nodes: [] } },
          { checkRuns: { nodes: runs.slice(1) } },
        ],
      };
      const { github, posts } = fake({
        data: {
          p0: {
            pullRequest: {
              ...pull,
              commits: { nodes: [{ commit: { ...commit, b0: suites } }] },
            },
          },
        },
      });

      const summaries = (
        await github.summaries(
          "owner",
          [{ repo: "org/pay", number: 7 }],
          [PRESETS.claude],
        )
      ).unwrap();

      expect(posts[0]?.query).toContain(
        'b0: checkSuites(first: 50) { nodes { checkRuns(last: 1, filterBy: { checkName: "Claude AI Review" }) { nodes { databaseId',
      );
      expect(posts[0]?.query).not.toContain("status {");
      expect(summaries[0]?.bots).toEqual({ claude: expected });
    },
  );

  test("leaves a check run bot's own failed run out of the failed required checks", async () => {
    const reviewRun = {
      __typename: "CheckRun",
      name: "Claude AI Review",
      conclusion: "FAILURE",
      isRequired: true,
    };
    const checks = pull.checks.nodes[0]?.commit.statusCheckRollup.contexts;
    const { github } = fake({
      data: {
        repository: {
          pullRequest: {
            ...pull,
            checks: {
              nodes: [
                {
                  commit: {
                    statusCheckRollup: {
                      contexts: {
                        nodes: [...(checks?.nodes ?? []), reviewRun],
                      },
                    },
                  },
                },
              ],
            },
          },
        },
      },
    });

    const detail = (
      await github.detail("owner", { repo: "org/pay", number: 7 }, [
        PRESETS.claude,
      ])
    ).unwrap();

    expect(detail.failedRequired.map((check) => check.name)).toEqual([
      "build",
      "CodeRabbit",
      "deploy",
    ]);
  });

  test("keeps an account's token for ten minutes", async () => {
    const { github, commands, clock } = fake({ data: { p0: null } });
    const read = () =>
      github.summaries("owner", [{ repo: "org/pay", number: 7 }], BOTS);

    await read();
    clock.now = 9 * 60_000;
    await read();
    clock.now = 10 * 60_000;
    await read();

    expect(commands).toHaveLength(2);
  });

  test("forgets a token GitHub answers 401 for", async () => {
    const { github, commands } = fake({ message: "Bad credentials" }, 401);
    const read = () =>
      github.summaries("owner", [{ repo: "org/pay", number: 7 }], BOTS);

    await read();
    await read();

    expect(commands).toHaveLength(2);
  });

  test("reports GitHub's errors without the token", async () => {
    const { github } = fake(
      { errors: [{ message: "Could not resolve to a Repository" }] },
      200,
    );

    const failed = await github.detail(
      "owner",
      { repo: "org/pay", number: 7 },
      BOTS,
    );

    expect(failed.isErr() && failed.error.message).toBe(
      "GitHub answered 200: Could not resolve to a Repository",
    );
    expect(JSON.stringify(failed)).not.toContain("secret-token");
  });
});

const run = (
  databaseId: number,
  status: string,
  conclusion: string | null,
  startedAt: string | null,
  completedAt: string | null,
) => ({ databaseId, status, conclusion, startedAt, completedAt });
