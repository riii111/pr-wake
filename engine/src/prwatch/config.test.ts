import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { loadConfig } from "./config.ts";
import { PRESETS } from "./reviewers.ts";

const cleanups: (() => Promise<unknown>)[] = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

const configWith = async (fields: Record<string, unknown>) => {
  const root = await mkdtemp(join(tmpdir(), "pr-wake-watch-config-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  const path = join(root, "pr-watch.json");
  await writeFile(
    path,
    JSON.stringify({
      repositories: { "org/pay": { account: "owner" } },
      ...fields,
    }),
  );
  return loadConfig(path);
};

describe("loadConfig", () => {
  test("watches CodeRabbit when the config names no review bot", async () => {
    const config = await configWith({});

    expect(config.unwrap().reviewBots).toEqual([PRESETS.coderabbit]);
  });

  test("adds no instructions unless the config gives them, trimmed", async () => {
    const none = await configWith({});
    const given = await configWith({ instructions: " $task-worker\n" });
    const blank = await configWith({ instructions: "  " });

    expect(none.unwrap().instructions).toBeNull();
    expect(given.unwrap().instructions).toBe("$task-worker");
    expect(blank.isErr()).toBe(true);
  });

  test("takes a preset by name and fills what a described bot leaves out", async () => {
    const config = await configWith({
      reviewBots: [
        "coderabbit",
        { id: "other", name: "OtherBot", login: "other-reviewer[bot]" },
      ],
    });

    expect(config.unwrap().reviewBots).toEqual([
      PRESETS.coderabbit,
      {
        id: "other",
        name: "OtherBot",
        login: "other-reviewer[bot]",
        status: null,
        actionableReview: null,
        noActionable: null,
        severity: null,
      },
    ]);
  });

  test("takes a described bot that waits for a check run", async () => {
    const config = await configWith({
      reviewBots: [
        {
          id: "other",
          name: "OtherBot",
          login: "other-reviewer[bot]",
          status: { checkRun: "Other Review" },
        },
      ],
    });

    expect(config.unwrap().reviewBots.map((bot) => bot.status)).toEqual([
      { checkRun: "Other Review" },
    ]);
  });

  test.each([
    {
      name: "a status that names both a commit status and a check run",
      reviewBots: [
        {
          id: "x",
          name: "X",
          login: "x[bot]",
          status: { context: "X", completed: "done", checkRun: "X" },
        },
      ],
      message: "reviewBots",
    },
    {
      name: "a severity that is not a regular expression",
      reviewBots: [
        { id: "x", name: "X", login: "x[bot]", severity: "(unclosed" },
      ],
      message: "is not a regular expression",
    },
    {
      name: "an id used twice",
      reviewBots: ["coderabbit", { id: "coderabbit", name: "X", login: "x" }],
      message: "reviewBots repeats an id",
    },
    {
      name: "a login used twice",
      reviewBots: [
        "coderabbit",
        { id: "other", name: "X", login: "CodeRabbitAI[bot]" },
      ],
      message: "reviewBots repeats a login",
    },
    {
      name: "an unknown preset",
      reviewBots: ["copilot"],
      message: "reviewBots",
    },
  ])("refuses $name", async ({ reviewBots, message }) => {
    const config = await configWith({ reviewBots });

    expect(config.isErr() && config.error.message).toContain(message);
  });
});
