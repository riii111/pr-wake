import { afterEach, describe, expect, test } from "bun:test";
import { cp, mkdtemp, readFile, rename, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Result } from "better-result";

import { type EngineDeps, runEngine } from "./cli.ts";
import { holdProcessLock } from "./lib/fs.boundary.ts";
import type { Fields } from "./lib/object.ts";
import { watchPaths } from "./paths.ts";
import type { GitHub, PrComment, PrDetail, PrRef } from "./prwatch/github.ts";
import type { PrSummary } from "./prwatch/state.ts";
import { lockPath } from "./prwatch/store.ts";
import { runCycle } from "./prwatch/watcher.ts";
import { codexWorkers } from "./app/watch-worker.ts";

// Records harnexus-task watch wrote, copied before each test so the fixture itself is never changed.
const LEGACY = join(import.meta.dir, "../test/fixtures/legacy-watch");
const HEAD = "a".repeat(40);
// Local noon, so quiet hours never apply.
const NOON = new Date(2026, 9, 23, 12, 0);

const cleanups: (() => Promise<unknown>)[] = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

const url = (number: number) => `https://github.com/org/pay/pull/${number}`;

const comment = (number: number, id: number): PrComment => ({
  id: `inline:${id}`,
  kind: "inline",
  author: "coderabbitai[bot]",
  url: `${url(number)}#discussion_r${id}`,
  createdAt: "2026-10-21T00:00:00Z",
  body: "**🟠 Major**",
  path: "src/pay.kt",
  line: 42,
  reply: false,
  resolved: false,
  approved: false,
});

const summary: PrSummary = {
  state: "OPEN",
  isDraft: false,
  head: HEAD,
  // Later than every record, so each PR's detail is read again.
  updatedAt: "2026-10-23T01:00:00Z",
  mergeable: "MERGEABLE",
  bots: {
    coderabbit: {
      state: "SUCCESS",
      description: "Review completed",
      createdAt: "2026-10-22T00:00:00Z",
    },
  },
  rollup: "SUCCESS",
};

// Each PR shows only the comments its record already holds.
const COMMENTS: Record<number, PrComment[]> = {
  7: [comment(7, 71)],
  8: [comment(8, 81)],
  9: [comment(9, 91)],
  10: [comment(10, 101)],
};

const legacyCopy = async () => {
  const root = await mkdtemp(join(tmpdir(), "pr-wake-legacy-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  await cp(LEGACY, root, { recursive: true });
  await rename(join(root, "state/taskctl"), join(root, "state/harnexus-crew"));
  const env = {
    HOME: root,
    XDG_CONFIG_HOME: join(root, "config"),
    XDG_STATE_HOME: join(root, "state"),
    HARNEXUS_EXECUTION_ENV: "local",
  };
  const paths = watchPaths(env);
  const sent: Fields[] = [];

  const github: GitHub = {
    summaries: async (_account, refs) => Result.ok(refs.map(() => summary)),
    detail: async (_account, ref: PrRef): Promise<Result<PrDetail, never>> =>
      Result.ok({
        ...summary,
        url: url(ref.number),
        title: `PR ${ref.number}`,
        failedRequired: [],
        comments: COMMENTS[ref.number] ?? [],
        noActionable: [],
      }),
    openPulls: async () => Result.ok([]),
  };

  // Every worker is idle and last talked to an hour ago, so nothing holds a message back.
  const callApp = async (caller: string, tool: string, args: Fields) => {
    if (tool === "read_thread") {
      const read = {
        thread: { id: args.threadId, status: { type: "idle" } },
        turns: [
          {
            status: "completed",
            startedAt: Math.floor(NOON.getTime() / 1000) - 3600,
            completedAt: Math.floor(NOON.getTime() / 1000) - 3500,
            items: [{ type: "userMessage", content: [] }],
          },
        ],
      };
      return {
        outcome: "done",
        result: { content: [{ type: "text", text: JSON.stringify(read) }] },
      };
    }

    if (bridge.send.outcome === "unsupported") return bridge.send;
    sent.push({ caller, tool, ...args });
    return bridge.send;
  };

  const logs: Fields[] = [];
  // A fresh set each time, as each start of the watch process has.
  const cycle = async (now = NOON) => {
    const ran = await runCycle({
      paths,
      github,
      workers: codexWorkers(callApp, () =>
        Promise.resolve(Result.ok("sender-chat")),
      ),
      now: () => now,
      log: (entry) => logs.push(entry),
      over: new Set(),
    });
    expect(ran.isOk()).toBe(true);
  };

  // What the bridge answers a send; a test sets it to answer as a bridge without protocol 1.
  const bridge = { send: { outcome: "done" } as Fields };
  const engine = async (args: string[], extra: Record<string, string> = {}) => {
    const deps: EngineDeps = {
      env: { ...env, ...extra },
      github,
      callApp,
      checkProtocol: async () => Result.ok(),
      print: () => {},
      sleep: async () => {},
      program: { path: "/nonexistent", compiled: false },
    };
    const run = await runEngine(["watch", ...args], deps);
    return {
      code: run.code,
      out: run.code === 0 ? JSON.parse(run.stdout) : null,
      err: run.stderr,
    };
  };

  const record = async (key: string, file: "link" | "state") =>
    JSON.parse(await readFile(join(paths.state, key, `${file}.json`), "utf8"));

  return { paths, sent, logs, cycle, engine, record, bridge };
};

const KEY = {
  7: "02bff83354bf169d",
  8: "3d0a4555db96a9b0",
  9: "6db070b27f9194fb",
  10: "e4fa9c7ae05e70a7",
};

describe("records harnexus-task watch left", () => {
  test("show the sender, queued messages, awaited answers and a pending hand-over", async () => {
    const { engine } = await legacyCopy();

    const shown = await engine(["--status"]);

    expect(shown.code).toBe(0);
    expect(shown.out.sender).toBe("sender-chat");
    expect(
      shown.out.prs.map((pr: Fields) => [
        pr.pr,
        pr.worker,
        pr.status,
        pr.pending,
        pr.awaitingAnswer,
        pr.handingOver,
      ]),
    ).toEqual([
      [url(7), "next-chat", "watching", 0, true, true],
      [url(8), "w8", "watching", 0, false, false],
      [url(9), "w9", "watching", 1, false, false],
      [url(10), "w10", "halted", 0, false, false],
    ]);
  });

  test("go on without resending what was read, deliver the queued finding and hand a PR over", async () => {
    const { sent, logs, cycle, record } = await legacyCopy();

    await cycle();

    expect(sent.map((each) => [each.caller, each.threadId])).toEqual([
      ["sender-chat", "next-chat"],
      ["sender-chat", "w9"],
    ]);
    // The chat that took the PR over is told what only the last chat was told.
    expect(String(sent[0]?.prompt)).toContain("discussion_r71");
    expect(String(sent[1]?.prompt)).toContain("discussion_r91");
    expect(logs.some((log) => log.event === "handed_over")).toBe(true);
    const handed = await record(KEY[7], "state");
    expect([handed.worker, handed.delivered?.sender]).toEqual([
      "next-chat",
      "sender-chat",
    ]);
    const queued = await record(KEY[9], "state");
    expect(queued.pending).toEqual([]);
    // The record from before review bots were configurable is read and saved in the current form.
    const older = await record(KEY[8], "state");
    expect(older.seen).toEqual([
      "inline:81",
      `coderabbit:${HEAD}`,
      "bot:coderabbit",
    ]);
    expect(older.summary.bots.coderabbit.state).toBe("SUCCESS");
  });

  test("send nothing twice after the watch restarts", async () => {
    const { sent, cycle } = await legacyCopy();
    await cycle();
    const first = sent.length;

    // A new process, minutes later, reading the records the last one saved.
    await cycle(new Date(NOON.getTime() + 2 * 60_000));
    await cycle(new Date(NOON.getTime() + 4 * 60_000));

    expect(sent).toHaveLength(first);
  });

  test("keep a halted PR halted until it is released, and send nothing for it", async () => {
    const { sent, cycle, engine, record } = await legacyCopy();
    await cycle();
    expect(sent.some((each) => each.threadId === "w10")).toBe(false);

    const released = await engine(["--release", url(10)]);
    const state = await record(KEY[10], "state");

    expect(released.out).toEqual({ released: url(10), worker: "w10" });
    expect([state.status, state.halted, state.pending]).toEqual([
      "watching",
      null,
      [],
    ]);
  });

  test("keep the first link time for the same chat and refuse another chat unless it takes over", async () => {
    const { engine, record } = await legacyCopy();

    const again = await engine(["--link", url(9)], { CODEX_THREAD_ID: "w9" });
    const refused = await engine(["--link", url(9)], {
      CODEX_THREAD_ID: "other",
    });
    const kept = await record(KEY[9], "link");
    const taken = await engine(["--link", url(9), "--take-over"], {
      CODEX_THREAD_ID: "other",
    });
    const moved = await record(KEY[9], "link");

    expect(again.out).toEqual({ threadId: "w9", watched: url(9) });
    expect([kept.worker, kept.linkedAt]).toEqual([
      "w9",
      "2026-10-20T00:00:00.000Z",
    ]);
    expect([refused.code, refused.err]).toEqual([
      1,
      expect.stringContaining("already watched for chat w9"),
    ]);
    expect(taken.out).toEqual({ threadId: "other", watched: url(9) });
    expect(moved.worker).toBe("other");
    expect(moved.linkedAt).not.toBe("2026-10-20T00:00:00.000Z");
  });

  test("end the watch before writing once the bridge stops offering protocol 1, keeping what was queued", async () => {
    const { engine, record, sent, bridge } = await legacyCopy();
    bridge.send = { outcome: "unsupported", message: "old bridge" };

    const ended = await engine([]);

    expect([ended.code, ended.err]).toEqual([
      1,
      expect.stringContaining("cannot use the harnexus call socket: old bridge"),
    ]);
    expect(sent).toEqual([]);
    const queued = await record(KEY[9], "state");
    expect([queued.status, queued.pending.length]).toEqual(["watching", 1]);
    const handed = await record(KEY[7], "state");
    expect(handed.status).toBe("watching");
  });

  // The lock is taken with O_EXLOCK, which only macOS honors.
  test.skipIf(process.platform !== "darwin")(
    "let only one watch write them while watch.lock is held",
    async () => {
      const { paths, engine, sent } = await legacyCopy();
      expect(holdProcessLock(lockPath(paths.state)).unwrap()).toBe(true);

      const second = await engine([]);

      expect([second.code, second.err]).toEqual([
        1,
        expect.stringContaining("another PR watch is running"),
      ]);
      expect(sent).toEqual([]);
    },
  );
});
