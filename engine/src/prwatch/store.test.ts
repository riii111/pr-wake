import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { botKey, type WatchState } from "./state.ts";
import { freshState, keyOf, loadWatchState, saveWatchState } from "./store.ts";

const REF = { repo: "org/pay", number: 7 };
const PR = "https://github.com/org/pay/pull/7";
const HEAD = "a".repeat(40);

const cleanups: (() => Promise<unknown>)[] = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

describe("loadWatchState", () => {
  test("starts a fresh state when the PR has no record yet", async () => {
    const root = await newRoot();

    const loaded = await loadWatchState(root, REF);

    expect(loaded.unwrap()).toEqual(freshState());
  });

  test("returns a saved state with its tracking and unknown-outcome stop intact", async () => {
    const root = await newRoot();
    const saved: WatchState = {
      ...freshState(),
      status: "halted",
      reason: "the send could not be confirmed",
      summary: SUMMARY,
      checkedAt: "2026-10-23T02:00:00.000Z",
      changedAt: "2026-10-23T01:00:00.000Z",
      seen: [botKey("coderabbit"), `coderabbit:${HEAD}`, "inline:1"],
      pending: [
        { type: "finding", finding: FINDING },
        {
          type: "review_done",
          bot: "coderabbit",
          name: "CodeRabbit",
          head: HEAD,
          findings: 1,
          summaryAgrees: true,
        },
        { type: "conflict", head: HEAD },
        {
          type: "ci_failed",
          head: HEAD,
          check: { name: "build", url: "https://ci.example/1" },
        },
      ],
      followUp: [FINDING],
      conflictHead: HEAD,
      delivered: {
        at: "2026-10-23T02:00:00.000Z",
        sender: "sender",
        prompt: "go",
        findings: [FINDING],
        followUp: false,
      },
      worker: "worker",
      halted: { at: "2026-10-23T02:00:00.000Z", prompt: "go", answer: null },
    };
    await saveWatchState(root, REF, saved);

    const loaded = await loadWatchState(root, REF);

    expect(loaded.unwrap()).toEqual(saved);
  });

  test("reads a state written before review bots were configurable as the coderabbit preset", async () => {
    const root = await newRoot();
    const { bots: _, ...current } = SUMMARY;
    await writeState(root, {
      ...freshState(),
      summary: { ...current, coderabbit: SUMMARY.bots.coderabbit },
      seen: ["inline:1"],
      pending: [
        {
          type: "coderabbit_done",
          head: HEAD,
          findings: 1,
          summaryAgrees: true,
        },
        { type: "finding", finding: LEGACY_FINDING },
      ],
      followUp: [LEGACY_FINDING],
      delivered: {
        at: "2026-10-23T02:00:00.000Z",
        sender: "sender",
        prompt: "go",
        findings: [LEGACY_FINDING],
        followUp: false,
      },
    });

    const loaded = (await loadWatchState(root, REF)).unwrap();

    expect(loaded).toEqual({
      ...freshState(),
      summary: SUMMARY,
      seen: ["inline:1", botKey("coderabbit")],
      pending: [
        {
          type: "review_done",
          bot: "coderabbit",
          name: "CodeRabbit",
          head: HEAD,
          findings: 1,
          summaryAgrees: true,
        },
        { type: "finding", finding: FINDING },
      ],
      followUp: [FINDING],
      delivered: {
        at: "2026-10-23T02:00:00.000Z",
        sender: "sender",
        prompt: "go",
        findings: [FINDING],
        followUp: false,
      },
    });
  });

  test("reads a state written before a PR followed its chat as one with no worker", async () => {
    const root = await newRoot();
    const { worker: _, ...before } = freshState();
    await writeState(root, before);

    const loaded = await loadWatchState(root, REF);

    expect(loaded.unwrap()).toEqual(freshState());
  });

  test.each<{ name: string; record: () => unknown }>([
    { name: "JSON null", record: () => null },
    { name: "a JSON array", record: () => [freshState()] },
    { name: "an empty object", record: () => ({}) },
    {
      name: "an unknown status",
      record: () => ({ ...TRACKED, status: "sleeping" }),
    },
    {
      name: "seen that is not a list of keys",
      record: () => ({ ...TRACKED, seen: "inline:1" }),
    },
    {
      name: "a pending event of an unknown type",
      record: () => ({ ...TRACKED, pending: [{ type: "merged", head: HEAD }] }),
    },
    {
      name: "a pending finding without an id",
      record: () => ({
        ...TRACKED,
        pending: [{ type: "finding", finding: { ...FINDING, id: undefined } }],
      }),
    },
    {
      name: "a summary whose draft flag is not a boolean",
      record: () => ({ ...TRACKED, summary: { ...SUMMARY, isDraft: "no" } }),
    },
    {
      name: "a delivery without the sent prompt",
      record: () => ({
        ...TRACKED,
        delivered: { at: "2026-10-23T02:00:00.000Z", sender: "sender" },
      }),
    },
  ])(
    "refuses a record with $name and leaves it as it is on disk",
    async ({ record }) => {
      const root = await newRoot();
      await writeState(root, record());
      const before = await readState(root);

      const loaded = await loadWatchState(root, REF);

      expect(loaded.isErr() && loaded.error._tag).toBe("StateUnreadable");
      expect(await readState(root)).toBe(before);
    },
  );

  test("names the damaged field in the error", async () => {
    const root = await newRoot();
    await writeState(root, { ...TRACKED, pending: [{ type: "merged" }] });

    const loaded = await loadWatchState(root, REF);

    expect(loaded.isErr() && loaded.error.message).toContain("pending");
  });
});

const newRoot = async () => {
  const root = await mkdtemp(join(tmpdir(), "pr-wake-watch-store-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  return root;
};

const statePath = (root: string) => join(root, keyOf(REF), "state.json");

const writeState = async (root: string, record: unknown) => {
  await mkdir(join(root, keyOf(REF)), { recursive: true });
  await writeFile(statePath(root), JSON.stringify(record, null, 1));
};

const readState = (root: string) => readFile(statePath(root), "utf8");

const SUMMARY = {
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
};

const FINDING = {
  id: "inline:1",
  kind: "inline" as const,
  bot: "coderabbit",
  label: "CodeRabbit・Major",
  path: "src/pay.kt",
  line: 42,
  url: `${PR}#discussion_r1`,
  createdAt: "2026-10-23T00:30:00Z",
  approval: null,
};

const LEGACY_FINDING = (() => {
  const { bot: _, approval: __, ...rest } = FINDING;
  return { ...rest, coderabbit: true };
})();

// A state that has been tracking the PR, so a refused record is one that would cost something to reset.
const TRACKED = {
  ...freshState(),
  summary: SUMMARY,
  seen: [botKey("coderabbit"), "inline:1"],
  pending: [{ type: "finding", finding: FINDING }],
};
