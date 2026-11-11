import { describe, expect, test } from "bun:test";

import type { Workers } from "../prwatch/worker.ts";
import { appWorkers } from "./app-workers.ts";
import { t3Binary, type T3Ran, type T3Run, t3Workers } from "./t3-worker.ts";

describe("t3Workers.send", () => {
  test("queues the prompt on the thread through stdin", async () => {
    const { run, runs } = runner({
      ran: true,
      status: 0,
      stdout: '{"delivery":"queued"}\n',
      stderr: "",
    });

    const sent = await t3Workers(run).send(
      "t3",
      "t3:w1",
      "-- looks like an option",
    );

    expect(sent).toEqual({ outcome: "done", detail: { delivery: "queued" } });
    expect(runs).toEqual([
      {
        args: ["thread", "send", "w1", "-", "--json"],
        input: "-- looks like an option",
      },
    ]);
  });

  test.each<{ name: string; ran: T3Ran; outcome: string }>([
    {
      name: "a t3 that cannot start",
      ran: { ran: false, started: false, message: "cannot run t3" },
      outcome: "not_sent",
    },
    {
      name: "a server that is not running",
      ran: {
        ran: true,
        status: 1,
        stdout: "",
        stderr:
          "No running T3 Code server uses /x/.t3. Start T3 Code, then retry.",
      },
      outcome: "not_sent",
    },
    {
      name: "a sandbox that blocks the CLI's port",
      ran: {
        ran: true,
        status: 1,
        stdout: "",
        stderr: "Failed to reserve loopback port\nlisten EPERM",
      },
      outcome: "not_sent",
    },
    {
      name: "a failure after the server was reached",
      ran: {
        ran: true,
        status: 1,
        stdout: "",
        stderr: "Could not reach the T3 Code server at http://127.0.0.1:3773.",
      },
      outcome: "unknown",
    },
    {
      name: "a t3 stopped while it ran",
      ran: { ran: false, started: true, message: "t3 was stopped by SIGTERM" },
      outcome: "unknown",
    },
  ])("answers $name as $outcome", async ({ ran, outcome }) => {
    const { run } = runner(ran);

    const sent = await t3Workers(run).send("t3", "t3:w1", "p");

    expect(sent.outcome).toBe(outcome as never);
  });
});

describe("t3Workers.look", () => {
  test("finds the reply to the latest message with our text, after the worker finished it", async () => {
    const { run, runs } = runner(
      shown(thread({ latestRunId: "r2" }), [
        message("user", "r1", PROMPT),
        message("assistant", "r1", "old answer"),
        message("user", "r2", `${PROMPT}\n`),
        message("assistant", "r2", "working on it"),
        message("assistant", "r2", "fixed"),
      ]),
    );

    const view = await t3Workers(run).look("t3", "t3:w1", sent(), NOW);

    expect(view).toEqual({
      gone: false,
      busy: null,
      answer: { status: "completed", text: "fixed" },
    });
    expect(runs[0]?.args).toEqual([
      "thread",
      "show",
      "w1",
      "--messages",
      "50",
      "--json",
    ]);
  });

  test.each([
    {
      name: "a run that is still going",
      thread: thread({
        status: "running",
        activeRunId: "r1",
        latestRunId: "r1",
      }),
      messages: [message("user", "r1", PROMPT)],
      expected: {
        gone: false,
        busy: "worker turn in progress",
        answer: { status: "inProgress", text: "" },
      },
    },
    {
      name: "a run the owner stopped",
      thread: thread({ status: "interrupted", latestRunId: "r1" }),
      messages: [message("user", "r1", PROMPT)],
      expected: {
        gone: false,
        busy: null,
        answer: { status: "other", text: "" },
      },
    },
    {
      name: "a worker waiting for the owner's answer",
      thread: thread({ pendingRequestCount: 1, latestRunId: "r1" }),
      messages: [
        message("user", "r1", PROMPT),
        message("assistant", "r1", "done"),
      ],
      expected: {
        gone: false,
        busy: "worker is waiting for an answer",
        answer: { status: "completed", text: "done" },
      },
    },
    {
      name: "a message not yet shown",
      thread: thread({}),
      messages: [message("user", "r0", "something else")],
      expected: { gone: false, busy: null, answer: null },
    },
  ])("reads $name", async ({ thread, messages, expected }) => {
    const { run } = runner(shown(thread, [...messages]));

    expect(await t3Workers(run).look("t3", "t3:w1", sent(), NOW)).toEqual(
      expected as never,
    );
  });

  test.each<{ name: string; ran: T3Ran; expected: unknown }>([
    {
      name: "a thread T3 Code no longer has as gone",
      ran: {
        ran: true,
        status: 1,
        stdout: "",
        stderr: "Thread w1 is no longer available.",
      },
      expected: { gone: true },
    },
    {
      name: "a server that is not running as unknown for now",
      ran: {
        ran: true,
        status: 1,
        stdout: "",
        stderr: "No running T3 Code server uses /x/.t3.",
      },
      expected: null,
    },
    {
      name: "a t3 that cannot start as unknown for now",
      ran: { ran: false, started: false, message: "cannot run t3" },
      expected: null,
    },
    {
      name: "output that is not a thread as unknown for now",
      ran: { ran: true, status: 0, stdout: "Thread w1\n", stderr: "" },
      expected: null,
    },
  ])("reads $name", async ({ ran, expected }) => {
    const { run } = runner(ran);

    expect(await t3Workers(run).look("t3", "t3:w1", null, NOW)).toEqual(
      expected as never,
    );
  });
});

describe("appWorkers", () => {
  test("reaches a T3 Code worker through T3 Code and any other through the Codex App", async () => {
    const asked: string[] = [];
    const named = (app: string): Workers => ({
      senderOf: async (worker) => {
        asked.push(`${app} ${worker}`);
        return app;
      },
      look: async () => null,
      send: async () => ({ outcome: "done", detail: {} }),
    });
    const workers = appWorkers(named("codex"), named("t3"));

    await workers.senderOf("t3:w1");
    await workers.senderOf("019a-codex");

    expect(asked).toEqual(["t3 t3:w1", "codex 019a-codex"]);
  });
});

describe("t3Binary", () => {
  test.each([
    {
      name: "HARNEXUS_T3_BIN",
      env: { HARNEXUS_T3_BIN: "/opt/t3" },
      onPath: "/bin/t3",
      expected: "/opt/t3",
    },
    {
      name: "a relative HARNEXUS_T3_BIN as refused",
      env: { HARNEXUS_T3_BIN: "t3" },
      onPath: null,
      expected: null,
    },
    { name: "t3 on PATH", env: {}, onPath: "/bin/t3", expected: "/bin/t3" },
    {
      name: "the desktop app's launcher",
      env: {},
      onPath: null,
      expected: "/u/.t3/bin/t3",
    },
    {
      name: "the launcher under T3CODE_HOME",
      env: { T3CODE_HOME: "/data/t3" },
      onPath: null,
      expected: "/data/t3/bin/t3",
    },
  ])("takes $name", ({ env, onPath, expected }) => {
    expect(t3Binary(env, "/u", () => onPath)).toBe(expected);
  });
});

const PROMPT =
  "PR https://github.com/org/pay/pull/7 で対応が必要なことを知らせます。";
const NOW = new Date(2026, 9, 23, 12, 0);

const runner = (ran: T3Ran) => {
  const runs: { args: string[]; input: string }[] = [];
  const run: T3Run = async (args, input) => {
    runs.push({ args, input });
    return ran;
  };
  return { run, runs };
};

const thread = (fields: Record<string, unknown>) => ({
  threadId: "w1",
  status: "completed",
  activeRunId: null,
  latestRunId: null,
  pendingRequestCount: 0,
  ...fields,
});

const message = (role: string, runId: string, text: string) => ({
  role,
  runId,
  text,
});

const shown = (shownThread: unknown, messages: unknown[]): T3Ran => ({
  ran: true,
  status: 0,
  stdout: `${JSON.stringify({ thread: shownThread, messages })}\n`,
  stderr: "",
});

const sent = () => ({ at: NOW.toISOString(), sender: "t3", prompt: PROMPT });
