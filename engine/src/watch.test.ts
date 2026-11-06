import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Result } from "better-result";

import type { Fields } from "./lib/object.ts";
import { GithubUnreachable } from "./prwatch/failure.ts";
import type { GitHub, PrComment, PrDetail } from "./prwatch/github.ts";
import { botKey, type PrSummary, type WatchState } from "./prwatch/state.ts";
import {
  freshState,
  keyOf,
  loadWatchState,
  recordLink,
  saveWatchState,
} from "./prwatch/store.ts";
import { runCycle } from "./prwatch/watcher.ts";
import { watchPaths } from "./paths.ts";
import { codexWorkers } from "./app/watch-worker.ts";

const PR = "https://github.com/org/pay/pull/7";
const REF = { repo: "org/pay", number: 7 };
const HEAD = "a".repeat(40);
const LINKED_AT = "2026-10-20T00:00:00.000Z";
// Local noon, so the default quiet hours never apply unless a test asks for them.
const NOON = new Date(2026, 9, 23, 12, 0);

const cleanups: (() => Promise<unknown>)[] = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

const summary = (fields: Partial<PrSummary> = {}): PrSummary => ({
  state: "OPEN",
  isDraft: false,
  head: HEAD,
  updatedAt: "2026-10-23T01:00:00Z",
  mergeable: "MERGEABLE",
  bots: {
    coderabbit: {
      state: "SUCCESS",
      description: "Review completed",
      createdAt: "2026-10-23T01:00:00Z",
    },
  },
  rollup: "SUCCESS",
  ...fields,
});

const comment = (fields: Partial<PrComment>): PrComment => ({
  id: "inline:1",
  kind: "inline",
  author: "coderabbitai[bot]",
  url: `${PR}#discussion_r1`,
  createdAt: "2026-10-23T00:30:00Z",
  body: "**🟠 Major**",
  path: "src/pay.kt",
  line: 42,
  reply: false,
  resolved: false,
  approved: false,
  ...fields,
});

const detailOf = (
  polled: PrSummary,
  comments: PrComment[],
  extra: Partial<PrDetail> = {},
): PrDetail => ({
  ...polled,
  url: PR,
  title: "Pay",
  failedRequired: [],
  comments,
  noActionable: [],
  ...extra,
});

type Turn = Fields;

const turn = (fields: Fields = {}): Turn => ({
  status: "completed",
  startedAt: Math.floor(NOON.getTime() / 1000) - 3600,
  completedAt: Math.floor(NOON.getTime() / 1000) - 3500,
  items: [{ type: "userMessage", content: [{ type: "text", text: "go" }] }],
  ...fields,
});

// A turn another thread started, as a Claude worker shows it.
const delegatedTurn = (
  startedAt: Date,
  input: unknown,
  answer: string,
  source = "reviewer",
) =>
  turn({
    startedAt: Math.floor(startedAt.getTime() / 1000),
    completedAt: Math.floor(startedAt.getTime() / 1000) + 60,
    items: [
      {
        type: "userMessage",
        codexDelegation: { sourceThreadId: source, input: String(input) },
        content: [{ type: "text", text: "<codex_delegation>" }],
      },
      { type: "agentMessage", phase: "final_answer", text: answer },
    ],
  });

// Starts from a PR the watch already read once, unless a test checks the first read.
const fixture = async (config: Fields = {}, { firstRead = false } = {}) => {
  const root = await mkdtemp(join(tmpdir(), "pr-wake-watch-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  const env = {
    HOME: root,
    XDG_STATE_HOME: join(root, "state"),
    XDG_CONFIG_HOME: join(root, "config"),
  };
  await mkdir(join(root, "config/harnexus"), { recursive: true });
  await writeFile(
    join(root, "config/harnexus/pr-watch.json"),
    JSON.stringify({
      repositories: { "org/pay": { account: "owner" } },
      quietHours: null,
      ...config,
    }),
  );
  const paths = watchPaths(env);
  await recordLink(paths.state, {
    repo: "org/pay",
    number: 7,
    url: PR,
    worker: "worker",
    linkedAt: LINKED_AT,
  });
  if (!firstRead)
    await saveWatchState(paths.state, REF, {
      ...freshState(),
      summary: summary({ updatedAt: "2026-10-22T00:00:00Z" }),
    });

  const world = {
    summary: summary(),
    comments: [] as PrComment[],
    extra: {} as Partial<PrDetail>,
    detailFails: false,
    turns: [turn()] as Turn[],
    active: false,
    gone: false,
    sendOutcome: "done",
    // The chat registered with watch --sender.
    sender: "reviewer" as string | null,
    now: NOON,
  };
  const calls = { summaries: 0, details: 0, sent: [] as Fields[] };

  const github: GitHub = {
    summaries: async (_account, refs) => {
      calls.summaries += 1;
      return Result.ok(refs.map(() => world.summary));
    },
    detail: async () => {
      calls.details += 1;
      if (world.detailFails) {
        return Result.err(
          new GithubUnreachable({ cause: null, message: "GitHub is down" }),
        );
      }

      return Result.ok(detailOf(world.summary, world.comments, world.extra));
    },
    openPulls: async () => Result.ok([]),
  };

  const callApp = async (caller: string, tool: string, args: Fields) => {
    if (tool === "read_thread") {
      if (world.gone)
        return {
          outcome: "tool_error",
          result: {
            content: [
              { type: "text", text: "No Codex thread found for threadId" },
            ],
            isError: true,
          },
        };
      const read = {
        thread: {
          id: args.threadId,
          status: { type: world.active ? "active" : "idle" },
        },
        turns: world.turns,
      };
      return {
        outcome: "done",
        result: { content: [{ type: "text", text: JSON.stringify(read) }] },
      };
    }

    calls.sent.push({ caller, tool, ...args });
    return { outcome: world.sendOutcome };
  };

  const logs: Fields[] = [];
  const cycle = async (over?: Set<string>) => {
    const ran = await runCycle({
      paths,
      github,
      workers: codexWorkers(callApp, async () => Result.ok(world.sender)),
      now: () => world.now,
      log: (entry) => logs.push(entry),
      ...(over === undefined ? {} : { over }),
    });
    expect(ran.isOk()).toBe(true);
  };

  const state = async (): Promise<WatchState> =>
    (
      await loadWatchState(paths.state, { repo: "org/pay", number: 7 })
    ).unwrapOr(null as never);
  const later = (minutes: number) => {
    world.now = new Date(world.now.getTime() + minutes * 60_000);
    world.summary = { ...world.summary, updatedAt: world.now.toISOString() };
  };

  return { env, paths, world, calls, logs, cycle, state, later };
};

describe("runCycle", () => {
  test("sends a completed CodeRabbit review with its new findings in one message from the registered chat", async () => {
    const { world, calls, cycle, state } = await fixture({
      instructions: "$task-worker",
    });
    world.comments = [
      comment({}),
      comment({ id: "inline:2", reply: true, url: `${PR}#discussion_r2` }),
      comment({
        id: "inline:3",
        createdAt: "2026-10-19T00:00:00Z",
        url: `${PR}#discussion_r3`,
      }),
      comment({
        id: "issue:4",
        kind: "issue_comment",
        author: "owner",
        url: `${PR}#issuecomment-4`,
      }),
    ];

    await cycle();

    expect(calls.sent).toHaveLength(1);
    const [sent] = calls.sent;
    expect([sent?.caller, sent?.tool, sent?.threadId]).toEqual([
      "reviewer",
      "send_message_to_thread",
      "worker",
    ]);
    expect(String(sent?.prompt)).toStartWith("$task-worker\n");
    expect(String(sent?.prompt)).toContain(`${PR}#discussion_r1`);
    for (const skipped of ["discussion_r2", "discussion_r3", "issuecomment-4"])
      expect(String(sent?.prompt)).not.toContain(skipped);
    const saved = await state();
    expect([
      saved.status,
      saved.pending,
      saved.delivered?.findings.length,
    ]).toEqual(["watching", [], 1]);
  });

  test("takes everything on a PR it reads for the first time as seen and sends only what comes after", async () => {
    const { world, calls, cycle, state, later } = await fixture(
      {},
      { firstRead: true },
    );
    world.summary = summary({ mergeable: "CONFLICTING", rollup: "FAILURE" });
    world.extra = {
      failedRequired: [{ name: "build", url: "https://ci.example/1" }],
    };
    world.comments = [
      comment({}),
      comment({
        id: "review:2",
        kind: "review",
        author: "tanaka",
        url: `${PR}#pullrequestreview-2`,
        body: "looks off",
        path: null,
        line: null,
      }),
      comment({
        id: "issue:3",
        kind: "issue_comment",
        author: "tanaka",
        url: `${PR}#issuecomment-3`,
      }),
    ];
    await cycle();
    const first = await state();
    expect(calls.sent).toEqual([]);
    expect([first.pending, first.conflictHead]).toEqual([[], HEAD]);

    later(1);
    world.comments = [
      ...world.comments,
      comment({
        id: "inline:4",
        author: "tanaka",
        url: `${PR}#discussion_r4`,
        createdAt: world.now.toISOString(),
      }),
    ];
    await cycle();

    expect(calls.sent).toHaveLength(1);
    const prompt = String(calls.sent[0]?.prompt);
    expect(prompt).toContain("理由：レビュー・コメント\n");
    expect(prompt).toContain(`1. [tanaka] src/pay.kt:42 ${PR}#discussion_r4`);
    expect(prompt).not.toContain("2. ");
  });

  test("takes CodeRabbit's earlier findings as seen on the first read while its review runs", async () => {
    const { world, calls, cycle, later } = await fixture(
      {},
      { firstRead: true },
    );
    world.summary = summary({
      bots: {
        coderabbit: {
          state: "PENDING",
          description: "Review in progress",
          createdAt: "2026-10-23T01:00:00Z",
        },
      },
    });
    world.comments = [comment({})];
    await cycle();
    expect(calls.sent).toEqual([]);

    world.summary = summary({ updatedAt: "2026-10-23T02:00:00Z" });
    world.extra = { noActionable: ["coderabbit"] };
    later(1);
    await cycle();

    expect(calls.sent).toHaveLength(1);
    expect(String(calls.sent[0]?.prompt)).toContain(
      "「指摘なし」とだけ報告してください",
    );
    expect(String(calls.sent[0]?.prompt)).not.toContain("一致しない");
  });

  test("reads a draft on its first read and sends only what came after once it is ready", async () => {
    const { world, calls, cycle, state, later } = await fixture(
      {},
      { firstRead: true },
    );
    world.summary = summary({ isDraft: true });
    world.comments = [comment({})];
    await cycle();
    expect([calls.details, calls.sent, (await state()).status]).toEqual([
      1,
      [],
      "draft",
    ]);

    world.summary = summary({ isDraft: false });
    later(1);
    world.comments = [
      comment({}),
      comment({
        id: "inline:5",
        author: "tanaka",
        url: `${PR}#discussion_r5`,
        createdAt: world.now.toISOString(),
      }),
    ];
    await cycle();

    expect(calls.sent).toHaveLength(1);
    expect(String(calls.sent[0]?.prompt)).toContain("discussion_r5");
    expect(String(calls.sent[0]?.prompt)).not.toContain("discussion_r1");
  });

  test("reads the PR again on the next cycle when the first read of its detail failed", async () => {
    const { world, calls, cycle, state, later } = await fixture(
      {},
      { firstRead: true },
    );
    world.comments = [comment({})];
    world.detailFails = true;
    await cycle();
    expect((await state()).summary).toBeNull();

    world.detailFails = false;
    later(1);
    await cycle();

    expect(calls.details).toBe(2);
    expect(calls.sent).toEqual([]);
    expect((await state()).seen).toContain("inline:1");
  });

  test("logs a failed read of a PR's detail by its tag, without the message that names the cause", async () => {
    const { world, logs, cycle } = await fixture({}, { firstRead: true });
    world.detailFails = true;

    await cycle();

    expect(logs.filter((log) => log.event === "detail_failed")).toMatchObject([
      { error: "GithubUnreachable" },
    ]);
    expect(JSON.stringify(logs)).not.toContain("GitHub is down");
  });

  test("never sends an event again and checks the answer before the next message", async () => {
    const { world, calls, cycle, state, later } = await fixture();
    world.comments = [comment({})];
    await cycle();

    later(1);
    world.comments = [
      comment({}),
      comment({
        id: "inline:5",
        author: "tanaka",
        url: `${PR}#discussion_r5`,
        createdAt: world.now.toISOString(),
      }),
    ];
    await cycle();
    expect(calls.sent).toHaveLength(1);

    world.turns = [
      delegatedTurn(NOON, calls.sent[0]?.prompt, `1. ${PR}#discussion_r1 直す`),
    ];
    later(1);
    await cycle();

    expect(calls.sent).toHaveLength(2);
    expect(String(calls.sent[1]?.prompt)).toContain(`${PR}#discussion_r5`);
    expect(String(calls.sent[1]?.prompt)).not.toContain("discussion_r1");
    expect((await state()).followUp).toEqual([]);
  });

  test("sends one follow-up for the findings the answer left out", async () => {
    const { world, calls, cycle, later } = await fixture();
    world.comments = [
      comment({}),
      comment({ id: "inline:2", url: `${PR}#discussion_r2` }),
    ];
    await cycle();

    world.turns = [
      delegatedTurn(
        NOON,
        calls.sent[0]?.prompt,
        `${PR}#discussion_r1 のみ判定`,
      ),
    ];
    later(1);
    await cycle();
    world.turns = [
      delegatedTurn(world.now, calls.sent[1]?.prompt, "判定できません"),
      ...world.turns,
    ];
    later(1);
    await cycle();

    expect(calls.sent).toHaveLength(2);
    expect(String(calls.sent[1]?.prompt)).toContain("discussion_r2");
    expect(String(calls.sent[1]?.prompt)).not.toContain("discussion_r1");
  });

  test("sends a person's approvals and follows up only on the one with a body", async () => {
    const { world, calls, cycle, later } = await fixture();
    const approval = (id: number, body: string) =>
      comment({
        id: `review:${id}`,
        kind: "review",
        author: "tanaka",
        url: `${PR}#pullrequestreview-${id}`,
        body,
        path: null,
        line: null,
        approved: true,
      });
    world.comments = [approval(1, "LGTM, one nit"), approval(2, "")];
    await cycle();

    expect(calls.sent).toHaveLength(1);
    const prompt = String(calls.sent[0]?.prompt);
    expect(prompt).toContain(`${PR}#pullrequestreview-1`);
    expect(prompt).toContain(`${PR}#pullrequestreview-2`);

    world.turns = [delegatedTurn(NOON, calls.sent[0]?.prompt, "確認しました")];
    later(1);
    await cycle();

    expect(calls.sent).toHaveLength(2);
    expect(String(calls.sent[1]?.prompt)).toContain("pullrequestreview-1");
    expect(String(calls.sent[1]?.prompt)).not.toContain("pullrequestreview-2");
  });

  test("takes only the turn our message started as its answer", async () => {
    const { world, calls, cycle, later } = await fixture();
    world.comments = [comment({})];
    await cycle();
    const sent = calls.sent[0]?.prompt;
    later(1);
    world.comments = [
      comment({}),
      comment({
        id: "inline:5",
        author: "tanaka",
        url: `${PR}#discussion_r5`,
        createdAt: world.now.toISOString(),
      }),
    ];

    world.turns = [
      delegatedTurn(NOON, sent, "unrelated", "coordinator"),
      delegatedTurn(NOON, "another message", "unrelated"),
      delegatedTurn(new Date(NOON.getTime() - 60_000), sent, "earlier"),
    ];
    await cycle();
    expect(calls.sent).toHaveLength(1);

    world.turns = [
      delegatedTurn(NOON, sent, `${PR}#discussion_r1`),
      ...world.turns,
    ];
    later(1);
    await cycle();

    expect(calls.sent).toHaveLength(2);
    expect(String(calls.sent[1]?.prompt)).toContain("discussion_r5");
  });

  test.each([
    {
      name: "CodeRabbit's",
      config: {},
      bot: "coderabbit",
      running: "Review in progress",
      finished: "Review completed",
      author: "tanaka",
      held: { author: "coderabbitai[bot]", body: "**🟠 Major**" },
      firstLabel: "[tanaka]",
      review: "CodeRabbitのレビュー完了",
      label: "[CodeRabbit・Major]",
    },
    {
      name: "Claude's",
      config: { reviewBots: ["claude"] },
      bot: "claude",
      running: "",
      finished: "",
      author: "coderabbitai[bot]",
      held: { author: "claude[bot]", body: "[warning] 重複します。" },
      firstLabel: "[coderabbitai[bot]]",
      review: "Claudeのレビュー完了",
      label: "[Claude・warning]",
    },
    {
      name: "a configured bot's",
      config: {
        reviewBots: [
          {
            id: "other",
            name: "OtherBot",
            login: "other-reviewer[bot]",
            status: { context: "Other Review", completed: "done" },
          },
        ],
      },
      bot: "other",
      running: "running",
      finished: "done",
      author: "coderabbitai[bot]",
      held: { author: "other-reviewer[bot]", body: "x" },
      firstLabel: "[coderabbitai[bot]]",
      review: "OtherBotのレビュー完了",
      label: "[OtherBot]",
    },
  ])(
    "holds $name findings while its review runs and sends the rest at once",
    async ({
      config,
      bot,
      running,
      finished,
      author,
      held,
      firstLabel,
      review,
      label,
    }) => {
      const { world, calls, cycle, later } = await fixture(config);
      const botRun = (
        state: string,
        description: string,
        createdAt: string,
      ) => ({
        [bot]: { state, description, createdAt },
      });
      world.summary = summary({
        bots: botRun("PENDING", running, "2026-10-23T01:00:00Z"),
      });
      world.comments = [
        comment(held),
        comment({ id: "inline:9", author, url: `${PR}#discussion_r9` }),
      ];
      await cycle();

      expect(calls.sent).toHaveLength(1);
      expect(String(calls.sent[0]?.prompt)).toContain(firstLabel);
      expect(String(calls.sent[0]?.prompt)).not.toContain("discussion_r1");

      world.turns = [
        delegatedTurn(NOON, calls.sent[0]?.prompt, `${PR}#discussion_r9`),
      ];
      world.summary = summary({
        updatedAt: "2026-10-23T02:00:00Z",
        bots: botRun("SUCCESS", finished, "2026-10-23T01:00:00Z"),
      });
      later(1);
      await cycle();

      expect(calls.sent).toHaveLength(2);
      expect(String(calls.sent[1]?.prompt)).toContain(review);
      expect(String(calls.sent[1]?.prompt)).toContain(
        `${label} src/pay.kt:42 ${PR}#discussion_r1`,
      );
    },
  );

  test.each([
    {
      name: "the worker's turn is running",
      set: (world: Fields) => {
        world.active = true;
      },
    },
    {
      name: "the owner talked to the worker a minute ago",
      set: (world: Fields) => {
        world.turns = [
          turn({ completedAt: Math.floor(NOON.getTime() / 1000) - 60 }),
        ];
      },
    },
    {
      name: "it is within the quiet hours",
      config: { quietHours: { start: "22:00", end: "05:00" } },
      set: (world: Fields) => {
        world.now = new Date(2026, 9, 23, 23, 30);
      },
    },
  ])("queues without sending while $name", async ({ config, set }) => {
    const { world, calls, cycle, state } = await fixture(config);
    world.comments = [comment({})];
    set(world);

    await cycle();

    expect(calls.sent).toEqual([]);
    expect((await state()).pending.map((event) => event.type)).toEqual([
      "review_done",
      "finding",
    ]);
  });

  test("halts on an unconfirmed send and never sends for the PR again", async () => {
    const { world, calls, cycle, state, later } = await fixture();
    world.comments = [comment({})];
    world.sendOutcome = "unknown";
    await cycle();

    world.sendOutcome = "done";
    later(5);
    await cycle();

    const saved = await state();
    expect(calls.sent).toHaveLength(1);
    expect([saved.status, typeof saved.halted?.prompt]).toEqual([
      "halted",
      "string",
    ]);
  });

  test("keeps the message queued when the app is not reachable", async () => {
    const { world, calls, cycle, state, later } = await fixture();
    world.comments = [comment({})];
    world.sendOutcome = "not_sent";
    await cycle();

    world.sendOutcome = "done";
    later(1);
    await cycle();

    expect(calls.sent).toHaveLength(2);
    expect((await state()).pending).toEqual([]);
  });

  test("pauses a draft without reading its detail and resumes when it is ready", async () => {
    const { world, calls, cycle, state, later } = await fixture();
    world.summary = summary({ isDraft: true });
    world.comments = [comment({})];
    await cycle();
    expect([calls.details, (await state()).status]).toEqual([0, "draft"]);

    world.summary = summary({ isDraft: false });
    later(1);
    await cycle();

    expect(calls.details).toBe(1);
    expect(calls.sent).toHaveLength(1);
  });

  test.each([
    {
      name: "a merged PR",
      set: (world: Fields) => {
        world.summary = summary({ state: "MERGED" });
      },
      expected: ["ended", "merged"],
    },
    {
      name: "a PR whose worker thread is gone",
      set: (world: Fields) => {
        world.gone = true;
        world.comments = [comment({})];
      },
      expected: ["stopped", "worker thread is gone"],
    },
  ])("stops watching $name", async ({ set, expected }) => {
    const { world, calls, cycle, state, later } = await fixture();
    set(world);
    await cycle();
    const polls = calls.summaries;

    later(5);
    await cycle();

    const saved = await state();
    expect([saved.status, saved.reason]).toEqual([...expected]);
    expect(calls.summaries).toBe(polls);
    expect(calls.sent).toEqual([]);
  });

  test("reads the detail only when the light poll changed", async () => {
    const { world, calls, cycle } = await fixture();
    await cycle();

    world.now = new Date(NOON.getTime() + 60_000);
    await cycle();

    expect([calls.summaries, calls.details]).toEqual([2, 1]);
  });

  test("polls a PR idle for longer than idleAfterHours at the idle interval", async () => {
    const { world, calls, cycle } = await fixture({
      idleAfterHours: 1,
      idleIntervalSeconds: 600,
    });
    await cycle();

    world.now = new Date(NOON.getTime() + 2 * 3_600_000);
    await cycle();
    world.now = new Date(world.now.getTime() + 60_000);
    await cycle();
    world.now = new Date(world.now.getTime() + 600_000);
    await cycle();

    expect(calls.summaries).toBe(3);
  });

  test("tells a conflict once per head and a failed required check once per head", async () => {
    const { world, calls, cycle, later } = await fixture();
    world.summary = summary({ mergeable: "CONFLICTING", rollup: "FAILURE" });
    world.extra = {
      failedRequired: [{ name: "build", url: "https://ci.example/1" }],
    };
    await cycle();

    world.turns = [delegatedTurn(NOON, calls.sent[0]?.prompt, "ok")];
    later(1);
    await cycle();

    expect(calls.sent).toHaveLength(1);
    expect(String(calls.sent[0]?.prompt)).toContain("[コンフリクト]");
    expect(String(calls.sent[0]?.prompt)).toContain("[必須CIの失敗] build");
  });

  test("leaves a PR of a repository the config does not list unpolled", async () => {
    const { calls, cycle, state } = await fixture({ repositories: {} });

    await cycle();

    expect([calls.summaries, calls.sent]).toEqual([0, []]);
    expect((await state()).reason).toBe("repository is not configured");
  });

  test("sends what a state written before review bots were configurable still holds", async () => {
    const { world, calls, cycle, state, paths } = await fixture();
    world.turns = [turn()];
    await saveWatchState(paths.state, REF, {
      ...freshState(),
      summary: summary(),
      pending: [
        {
          type: "coderabbit_done",
          head: HEAD,
          findings: 1,
          summaryAgrees: true,
        },
        {
          type: "finding",
          finding: {
            id: "inline:1",
            kind: "inline",
            coderabbit: true,
            label: "CodeRabbit・Major",
            path: "src/pay.kt",
            line: 42,
            url: `${PR}#discussion_r1`,
            createdAt: "2026-10-23T00:30:00Z",
          },
        },
      ] as unknown as WatchState["pending"],
    });

    await cycle();

    expect(String(calls.sent[0]?.prompt)).toContain("CodeRabbitのレビュー完了");
    const delivered = (await state()).delivered?.findings[0];
    expect([delivered?.bot, delivered?.approval]).toEqual(["coderabbit", null]);
  });

  test("stops reading a PR it found over for as long as the process runs", async () => {
    const { world, logs, cycle, paths } = await fixture();
    const over = new Set<string>();
    world.summary = summary({ state: "MERGED" });
    await cycle(over);
    await writeFile(join(paths.state, keyOf(REF), "state.json"), "{");

    await cycle(over);
    const skipped = logs.filter((log) => log.event === "state_unreadable");
    await cycle();
    const read = logs.filter((log) => log.event === "state_unreadable");

    expect([over.size, skipped.length, read.length]).toEqual([1, 0, 1]);
  });

  test("leaves a record it cannot read as it is, polling and sending nothing from it", async () => {
    const { calls, logs, cycle, paths } = await fixture();
    const path = join(paths.state, keyOf(REF), "state.json");
    await saveWatchState(paths.state, REF, {
      ...freshState(),
      summary: summary(),
      seen: [botKey("coderabbit"), "inline:1"],
      pending: [{ type: "conflict", head: HEAD }],
    });
    const damaged = (await readFile(path, "utf8")).replace(
      '"type": "conflict"',
      '"type": "merged"',
    );
    await writeFile(path, damaged);

    await cycle();

    expect(await readFile(path, "utf8")).toBe(damaged);
    expect([calls.summaries, calls.sent]).toEqual([0, []]);
    expect(logs.filter((log) => log.event === "state_unreadable")).toHaveLength(
      1,
    );
  });

  test("starts a bot added to the config later from the reviews it already finished", async () => {
    const { world, calls, cycle, later } = await fixture({
      reviewBots: [
        "coderabbit",
        {
          id: "other",
          name: "OtherBot",
          login: "other-reviewer[bot]",
          status: { context: "Other Review", completed: "done" },
        },
      ],
    });
    const done = (createdAt: string) => ({
      state: "SUCCESS",
      description: "done",
      createdAt,
    });
    world.summary = summary({
      bots: {
        coderabbit: summary().bots.coderabbit ?? null,
        other: done("2026-10-23T01:00:00Z"),
      },
    });
    world.extra = { noActionable: ["coderabbit"] };
    await cycle();
    const first = String(calls.sent[0]?.prompt);

    world.turns = [delegatedTurn(NOON, calls.sent[0]?.prompt, "指摘なし")];
    world.summary = summary({
      head: "b".repeat(40),
      bots: { coderabbit: null, other: done("2026-10-23T03:00:00Z") },
    });
    later(1);
    await cycle();

    expect(first).not.toContain("OtherBot");
    expect(String(calls.sent[1]?.prompt)).toContain(
      "OtherBotのレビューが完了し",
    );
  });

  test("sends nothing without a registered chat, or when the registered chat is the worker itself", async () => {
    const { world, calls, cycle, later, state } = await fixture();
    world.comments = [comment({})];
    world.sender = null;
    await cycle();
    const reason = (await state()).reason;
    world.sender = "worker";
    later(1);
    await cycle();
    const fromItself = calls.sent.length;
    const itselfReason = (await state()).reason;

    world.sender = "sender";
    later(1);
    await cycle();

    expect(reason).toContain("no chat is registered");
    expect(itselfReason).toContain("is this chat itself");
    expect(fromItself).toBe(0);
    expect(calls.sent.map((sent) => [sent.caller, sent.threadId])).toEqual([
      ["sender", "worker"],
    ]);
  });

  test("hands a PR named from another chat over to it, dropping the answer awaited from the last chat", async () => {
    const { world, calls, cycle, state, later, paths, logs } = await fixture();
    world.comments = [comment({})];
    world.turns = [turn()];
    await cycle();
    expect((await state()).delivered?.sender).toBe("reviewer");

    await recordLink(paths.state, {
      repo: "org/pay",
      number: 7,
      url: PR,
      worker: "next",
      linkedAt: LINKED_AT,
    });
    world.sender = "sender";
    world.comments = [
      comment({}),
      comment({ id: "inline:9", author: "tanaka", url: `${PR}#discussion_r9` }),
    ];
    later(1);
    await cycle();

    expect(logs.some((log) => log.event === "handed_over")).toBe(true);
    expect(calls.sent.map((sent) => [sent.caller, sent.threadId])).toEqual([
      ["reviewer", "worker"],
      ["sender", "next"],
    ]);
    // The finding only the last chat was told goes to the new one too.
    expect(String(calls.sent[1]?.prompt)).toContain("discussion_r1");
    expect(String(calls.sent[1]?.prompt)).toContain("discussion_r9");
  });

  test.each([
    {
      name: "stopped for a gone chat",
      stop: (world: Fields) => {
        world.gone = true;
      },
      resume: (world: Fields) => {
        world.gone = false;
      },
      stopped: "stopped",
    },
    {
      name: "halted on an unconfirmed send",
      stop: (world: Fields) => {
        world.sendOutcome = "unknown";
      },
      resume: (world: Fields) => {
        world.sendOutcome = "done";
      },
      stopped: "halted",
    },
  ])(
    "hands a PR $name over to the chat that takes it and sends what it still had to send",
    async ({ stop, resume, stopped }) => {
      const { world, calls, cycle, state, later, paths } = await fixture();
      world.comments = [comment({})];
      stop(world);
      await cycle();
      expect((await state()).status).toBe(stopped);

      await recordLink(paths.state, {
        repo: "org/pay",
        number: 7,
        url: PR,
        worker: "next",
        linkedAt: LINKED_AT,
      });
      resume(world);
      world.sender = "sender";
      later(1);
      await cycle();

      const saved = await state();
      expect([saved.status, saved.halted, saved.worker]).toEqual([
        "watching",
        null,
        "next",
      ]);
      expect(calls.sent.at(-1)?.threadId).toBe("next");
    },
  );
});
