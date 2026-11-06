import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Result } from "better-result";

import { holdProcessLock } from "../lib/fs.boundary.ts";
import { dryRun, release, status, watchForever } from "./commands.ts";
import type { GitHub, PrDetail } from "./github.ts";
import { botKey, type PrSummary } from "./state.ts";
import {
  freshState,
  keyOf,
  loadWatchState,
  lockPath,
  recordLink,
  saveWatchState,
} from "./store.ts";

const PR = "https://github.com/org/pay/pull/7";
const REF = { repo: "org/pay", number: 7 };
const LINK = {
  ...REF,
  url: PR,
  worker: "worker",
  linkedAt: "2026-09-01T00:00:00Z",
};

const cleanups: (() => Promise<unknown>)[] = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

const fixture = async () => {
  const root = await mkdtemp(join(tmpdir(), "pr-wake-watch-commands-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  const paths = {
    config: join(root, "pr-watch.json"),
    state: join(root, "watch"),
  };
  await writeFile(
    paths.config,
    JSON.stringify({
      repositories: { "Org/Pay": { account: "owner" } },
      ignore: [{ author: "coderabbitai[bot]", kind: "issue_comment" }],
    }),
  );
  return { paths, root: paths.state };
};

const summary: PrSummary = {
  state: "OPEN",
  isDraft: false,
  head: "a".repeat(40),
  updatedAt: "2026-10-23T00:00:00Z",
  mergeable: "MERGEABLE",
  bots: {
    coderabbit: {
      state: "SUCCESS",
      description: "Review completed",
      createdAt: "2026-10-01T00:00:00Z",
    },
  },
  rollup: "SUCCESS",
};

const detail: PrDetail = {
  ...summary,
  url: PR,
  title: "Pay",
  failedRequired: [],
  noActionable: [],
  comments: [
    {
      id: "inline:1",
      kind: "inline",
      author: "coderabbitai[bot]",
      url: `${PR}#discussion_r1`,
      createdAt: "2026-10-01T00:00:00Z",
      body: "**🟡 Minor**",
      path: "a.kt",
      line: 3,
      reply: false,
      resolved: false,
      approved: false,
    },
    {
      id: "issue:2",
      kind: "issue_comment",
      author: "coderabbitai[bot]",
      url: `${PR}#issuecomment-2`,
      createdAt: "2026-10-01T00:00:00Z",
      body: "walkthrough",
      path: null,
      line: null,
      reply: false,
      resolved: false,
      approved: false,
    },
  ],
};

const github = (asked: string[]): GitHub => ({
  summaries: async () => Result.ok([]),
  detail: async (account, ref) => {
    asked.push(`${account} ${ref.repo}#${ref.number}`);
    return Result.ok(detail);
  },
  openPulls: async (account, repo) => {
    asked.push(`open ${account} ${repo}`);
    return Result.ok([{ repo, number: 7 }]);
  },
});

describe("dryRun", () => {
  test("reports the verdicts and the message for every open PR the watch already read without writing its state", async () => {
    const { paths, root } = await fixture();
    await recordLink(root, LINK);
    const read = {
      ...freshState(),
      summary,
      seen: [botKey("coderabbit")],
    };
    await saveWatchState(root, REF, read);
    const asked: string[] = [];

    const ran = await dryRun({ paths, github: github(asked) }, []);

    const report = ran.unwrap().prs as Record<string, unknown>[];
    expect(asked).toEqual(["open owner org/pay", "owner org/pay#7"]);
    expect((await loadWatchState(root, REF)).unwrap()).toEqual(read);
    expect(report[0]?.firstRead).toBe(false);
    expect(report[0]?.ignored).toEqual([
      {
        id: "issue:2",
        author: "coderabbitai[bot]",
        kind: "issue_comment",
        reason: "ignore rule",
        url: `${PR}#issuecomment-2`,
      },
    ]);
    expect(String(report[0]?.prompt)).toContain(
      `1. [CodeRabbit・Minor] a.kt:3 ${PR}#discussion_r1`,
    );
  });

  test.each([
    { name: "a linked PR without watch state", linked: true },
    { name: "a PR no worker linked", linked: false },
  ])(
    "shows $name as a first read with no message and writes no state",
    async ({ linked }) => {
      const { paths, root } = await fixture();
      if (linked) await recordLink(root, LINK);

      const ran = await dryRun({ paths, github: github([]) }, [PR]);

      const [report] = ran.unwrap().prs as Record<string, unknown>[];
      expect([report?.firstRead, report?.prompt]).toEqual([true, null]);
      expect(report?.notified).toHaveLength(1);
      expect((await loadWatchState(root, REF)).unwrap()).toEqual(freshState());
    },
  );

  test("reads only the given PRs and refuses a repository outside the config", async () => {
    const { paths } = await fixture();
    const asked: string[] = [];

    const given = await dryRun({ paths, github: github(asked) }, [
      `${PR}/files`,
    ]);
    const outside = await dryRun({ paths, github: github(asked) }, [
      "https://github.com/org/other/pull/1",
    ]);

    expect(given.isOk()).toBe(true);
    expect(asked).toEqual(["owner org/pay#7"]);
    expect(outside.isErr() && outside.error.message).toContain(
      "org/other is not in the repositories",
    );
  });
});

describe("release", () => {
  test("resumes a halted PR and takes its message as delivered", async () => {
    const { root } = await fixture();
    const ref = { repo: "org/pay", number: 7 };
    await recordLink(root, {
      ...ref,
      url: PR,
      worker: "worker",
      linkedAt: "2026-10-01T00:00:00Z",
    });
    await saveWatchState(root, ref, {
      ...freshState(),
      status: "halted",
      pending: [{ type: "conflict", head: "a" }],
      halted: { prompt: "x" },
    });

    const released = await release(root, PR);
    const again = await release(root, PR);

    const state = (await loadWatchState(root, ref)).unwrap();
    expect(released.unwrap()).toEqual({ released: PR, worker: "worker" });
    expect([state.status, state.pending, state.halted]).toEqual([
      "watching",
      [],
      null,
    ]);
    expect(again.isErr() && again.error.message).toContain("is not halted");
  });
});

describe("status", () => {
  test("lists each linked PR with what its watch is doing", async () => {
    const { root } = await fixture();
    await recordLink(root, LINK);
    await saveWatchState(root, REF, {
      ...freshState(),
      status: "halted",
      reason: "x",
      pending: [{ type: "conflict", head: "a" }],
    });

    const shown = await status(root);

    expect(shown.unwrap()).toEqual({
      sender: null,
      prs: [
        {
          pr: PR,
          worker: "worker",
          status: "halted",
          reason: "x",
          pending: 1,
          followUp: 0,
          awaitingAnswer: false,
          handingOver: false,
          checkedAt: null,
        },
      ],
    });
  });
});

describe("status with an unreadable record", () => {
  test("shows the record as unreadable and still lists the others", async () => {
    const { root } = await fixture();
    await recordLink(root, LINK);
    await recordLink(root, { ...LINK, number: 8, url: `${PR.slice(0, -1)}8` });
    await writeFile(join(root, keyOf(REF), "state.json"), "{");

    const shown = (await status(root)).unwrap().prs as Record<
      string,
      unknown
    >[];

    expect(shown.map((pr) => pr.status).sort()).toEqual([
      "unreadable",
      "watching",
    ]);
  });
});

describe("watchForever once its program is replaced", () => {
  test("ends before the next cycle once the check after a sleep finds it", async () => {
    const { paths } = await fixture();
    const logs: Record<string, unknown>[] = [];
    let slept = 0;
    let checks = 0;

    const ran = await watchForever(
      {
        paths,
        github: github([]),
        workers: {
          senderOf: async () => ({ missing: "none" }),
          look: async () => null,
          send: async () => ({ outcome: "done", detail: {} }),
        },
        now: () => new Date(),
        log: (entry) => logs.push(entry),
      },
      async () => {
        slept += 1;
      },
      async () => {
        checks += 1;
        return checks > 1;
      },
    );

    expect(ran.isErr() && ran.error.message).toContain("was replaced");
    // One cycle ran before the sleep, and none after the check that found the program replaced.
    expect([checks, slept]).toEqual([2, 1]);
    expect(logs.map((entry) => entry.event)).toEqual(["started", "replaced"]);
  });
});

// The lock comes from macOS's O_EXLOCK, which other systems do not have.
const onMac = process.platform === "darwin";

describe("watchForever", () => {
  test.skipIf(!onMac)(
    "refuses to start while another watch holds the lock",
    async () => {
      const { paths, root } = await fixture();
      await mkdir(root, { recursive: true });
      expect(holdProcessLock(lockPath(root)).unwrap()).toBe(true);

      const ran = await watchForever(
        {
          paths,
          github: github([]),
          workers: {
            senderOf: async () => ({ missing: "none" }),
            look: async () => null,
            send: async () => ({ outcome: "done", detail: {} }),
          },
          now: () => new Date(),
          log: () => {},
        },
        async () => {},
      );

      expect(ran.isErr() && ran.error.message).toBe(
        "another PR watch is running",
      );
    },
  );
});
