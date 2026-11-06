import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Result } from "better-result";

import { callApp, callSocketPath } from "./app/call.ts";
import { checkV1, ProtocolUnsupported } from "./app/protocol.ts";
import { type EngineDeps, runEngine } from "./cli.ts";
import type { Fields } from "./lib/object.ts";
import type { GitHub } from "./prwatch/github.ts";

const cleanups: (() => Promise<unknown>)[] = [];

// No test here reaches GitHub.
const unreachable = async () => {
  throw new Error("no test reaches GitHub");
};
const GITHUB: GitHub = {
  summaries: unreachable,
  detail: unreachable,
  openPulls: unreachable,
};

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

const fixture = async () => {
  const root = await mkdtemp(join(tmpdir(), "pr-wake-engine-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "config/harnexus"), { recursive: true });
  await writeFile(
    join(root, "config/harnexus/pr-watch.json"),
    JSON.stringify({ repositories: { "org/pay": { account: "owner" } } }),
  );
  const env: Record<string, string> = {
    HOME: root,
    XDG_CONFIG_HOME: join(root, "config"),
    XDG_STATE_HOME: join(root, "state"),
    // Pinned, so a Cloud variable in the ambient environment cannot refuse the commands.
    HARNEXUS_EXECUTION_ENV: "local",
    CODEX_THREAD_ID: "caller",
  };
  const calls: Fields[] = [];
  const invoke = async (
    args: string[],
    deps: Partial<Omit<EngineDeps, "env">> = {},
  ) => {
    const run = await runEngine(args, {
      env,
      github: GITHUB,
      callApp: async (caller, tool, sent) => {
        calls.push({ caller, tool, ...sent });
        return { outcome: "done" };
      },
      checkProtocol: async () => Result.ok(),
      print: () => {},
      sleep: async () => {},
      program: { path: "/nonexistent", compiled: false },
      ...deps,
    });
    return {
      code: run.code,
      out:
        run.code === 0 && run.stdout.startsWith("{")
          ? JSON.parse(run.stdout)
          : null,
      err: run.stderr,
      stdout: run.stdout,
    };
  };
  return { root, env, calls, invoke };
};

describe("pr-wake-engine watch", () => {
  test.each([
    {
      name: "--pr without --dry-run",
      args: ["--pr", "https://github.com/org/pay/pull/1"],
      message: "--pr is only for --dry-run",
    },
    {
      name: "--release with --dry-run",
      args: ["--dry-run", "--release", "https://github.com/org/pay/pull/1"],
      message:
        "use only one of --dry-run, --release, --status, --sender and --link",
    },
    {
      name: "--status with --release",
      args: ["--status", "--release", "https://github.com/org/pay/pull/1"],
      message:
        "use only one of --dry-run, --release, --status, --sender and --link",
    },
    {
      name: "--take-over without --link",
      args: ["--take-over"],
      message: "--take-over is only for --link",
    },
    { name: "an unknown option", args: ["--nope"], message: "--nope" },
  ])("refuses $name", async ({ args, message }) => {
    const { invoke } = await fixture();

    const refused = await invoke(["watch", ...args]);

    expect([refused.code, refused.err]).toEqual([
      2,
      expect.stringContaining(message),
    ]);
  });

  test("shows its usage on --help and refuses another subcommand", async () => {
    const { invoke } = await fixture();

    const help = await invoke(["--help"]);
    const other = await invoke(["review"]);

    expect([help.code, help.stdout]).toEqual([
      0,
      expect.stringContaining("pr-wake-engine watch [--dry-run"),
    ]);
    expect(other.code).toBe(2);
  });

  test("records the calling chat as the sender and warns a linked chat while none can send to it", async () => {
    const { env, invoke } = await fixture();
    const pr = "https://github.com/org/pay/pull/9";

    const unsent = await invoke(["watch", "--link", pr]);
    env.CODEX_THREAD_ID = "sender-chat";
    const registered = await invoke(["watch", "--sender"]);
    env.CODEX_THREAD_ID = "caller";
    const linked = await invoke(["watch", "--link", pr]);
    const notPr = await invoke(["watch", "--link", "9"]);
    const shown = await invoke(["watch", "--status"]);

    expect(unsent.out).toEqual({
      threadId: "caller",
      watched: pr,
      warning:
        "no chat is registered to send from: run pr-wake watch sender in a chat kept for it",
    });
    expect(registered.out).toEqual({ sender: "sender-chat" });
    expect(linked.out).toEqual({ threadId: "caller", watched: pr });
    expect([notPr.code, notPr.err]).toEqual([
      1,
      expect.stringContaining("9 is not watched: the PR watch needs the URL"),
    ]);
    expect(shown.out.sender).toBe("sender-chat");
    expect(shown.out.prs.map((each: Fields) => each.worker)).toEqual([
      "caller",
    ]);
  });

  test.each([
    { name: "no chat ID", id: "" },
    { name: "a chat ID with a space", id: "a b" },
    { name: "an unconfirmed chat ID", id: "client-new-thread:1" },
  ])("refuses to link from $name", async ({ id }) => {
    const { env, invoke } = await fixture();
    env.CODEX_THREAD_ID = id;

    const refused = await invoke([
      "watch",
      "--link",
      "https://github.com/org/pay/pull/9",
    ]);

    expect([refused.code, refused.err]).toEqual([
      1,
      expect.stringContaining("CODEX_THREAD_ID"),
    ]);
  });

  test.each([
    { HARNEXUS_EXECUTION_ENV: "cloud" },
    { HARNEXUS_EXECUTION_ENV: "", CLAUDE_CODE_REMOTE: "true" },
    { HARNEXUS_EXECUTION_ENV: "", CODEX_ENVIRONMENT_ID: "env" },
  ])("refuses in Cloud (%o)", async (cloud) => {
    const { env, invoke } = await fixture();
    Object.assign(env, cloud);

    const refused = await invoke(["watch", "--status"]);

    expect([refused.code, refused.err]).toEqual([
      1,
      expect.stringContaining("unavailable in Cloud"),
    ]);
  });

  test("ends before the watch can send when harnexus does not speak the expected protocol", async () => {
    const { env, calls, invoke } = await fixture();
    env.CODEX_THREAD_ID = "sender-chat";
    await invoke(["watch", "--sender"]);
    let watched = false;

    const refused = await invoke(["watch"], {
      checkProtocol: async () =>
        Result.err(new ProtocolUnsupported({ message: "harnexus is too old" })),
      sleep: async () => {
        watched = true;
      },
    });

    expect([refused.code, refused.err, watched, calls]).toEqual([
      1,
      expect.stringContaining("harnexus is too old"),
      false,
      [],
    ]);
  });
});

// A bridge on a socket that answers each request line as the test says, keeping the lines it was sent.
const bridge = async (answer: (request: Fields) => Fields | null) => {
  const root = await mkdtemp(join(tmpdir(), "pr-wake-bridge-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  const path = join(root, "call.sock");
  const lines: Fields[] = [];
  const server = createServer((socket) => {
    let data = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      data += chunk;
      if (!data.endsWith("\n")) return;
      const request = JSON.parse(data) as Fields;
      lines.push(request);
      const answered = answer(request);
      // Null keeps the connection open without an answer, as a bridge stuck mid-call does.
      if (answered !== null) socket.end(`${JSON.stringify(answered)}\n`);
    });
  });
  await new Promise<void>((resolve) => server.listen(path, resolve));
  cleanups.push(() => new Promise((resolve) => server.close(resolve)));
  return { env: { HARNEXUS_CALL_SOCKET: path }, lines };
};

const OFFERED = {
  version: 1,
  outcome: "done",
  operations: ["read_thread", "send_message_to_thread", "name_thread"],
};

// What a bridge from before protocol 1 answers any line naming a version.
const OLD_BRIDGE = { outcome: "invalid", message: "expected a call" };

const v1Bridge = (call: (request: Fields) => Fields | null) =>
  bridge((request) =>
    request.query === "capabilities" ? OFFERED : call(request),
  );

describe("callApp", () => {
  test("asks for protocol 1 and sends the call in its form", async () => {
    const { env, lines } = await v1Bridge(() => ({
      version: 1,
      outcome: "done",
      result: 1,
    }));

    const answer = await callApp(env, "caller", "read_thread", {
      threadId: "w",
    });

    expect(answer).toEqual({ version: 1, outcome: "done", result: 1 });
    expect(lines).toEqual([
      { version: 1, query: "capabilities" },
      {
        version: 1,
        threadId: "caller",
        tool: "read_thread",
        arguments: { threadId: "w" },
      },
    ]);
  });

  test("sends nothing to a bridge without protocol 1, never falling back to the versionless form", async () => {
    const { env, lines } = await bridge(() => OLD_BRIDGE);

    const answer = await callApp(env, "caller", "send_message_to_thread", {
      threadId: "w",
      prompt: "hi",
    });

    expect(answer.outcome).toBe("unsupported");
    expect(lines).toEqual([{ version: 1, query: "capabilities" }]);
  });

  test("sends nothing for a tool the bridge does not offer", async () => {
    const { env, lines } = await v1Bridge(() => ({ outcome: "done" }));

    const answer = await callApp(env, "caller", "create_thread", {});

    expect([answer.outcome, lines.length]).toEqual(["unsupported", 1]);
  });

  test("reports a connected call without an answer in time as unknown", async () => {
    const { env } = await v1Bridge(() => null);

    const answer = await callApp(
      env,
      "caller",
      "send_message_to_thread",
      {},
      50,
    );

    expect(answer.outcome).toBe("unknown");
  });

  test("takes a missing socket as not sent", async () => {
    const { root } = await fixture();

    const answer = await callApp(
      { HARNEXUS_CALL_SOCKET: join(root, "missing.sock") },
      "caller",
      "send_message_to_thread",
      {},
    );

    expect(answer.outcome).toBe("not_sent");
  });
});

describe("checkV1", () => {
  test("accepts a bridge offering protocol 1 with the watch's tools", async () => {
    const { env } = await v1Bridge(() => null);

    expect((await checkV1(env)()).isOk()).toBe(true);
  });

  test.each([
    {
      name: "a bridge from before protocol 1",
      answer: OLD_BRIDGE,
      says: "does not offer call protocol 1",
    },
    {
      name: "a bridge without send_message_to_thread",
      answer: { ...OFFERED, operations: ["read_thread"] },
      says: "does not offer send_message_to_thread",
    },
    {
      name: "a bridge offering another version",
      answer: { ...OFFERED, version: 2 },
      says: "does not offer call protocol 1",
    },
  ])("refuses $name", async ({ answer, says }) => {
    const { env } = await bridge(() => answer);

    const checked = await checkV1(env)();

    expect(checked.isErr() && checked.error.message).toContain(says);
  });

  test("refuses when the bridge cannot be asked", async () => {
    const { root } = await fixture();

    const checked = await checkV1({
      HARNEXUS_CALL_SOCKET: join(root, "missing.sock"),
    })();

    expect(checked.isErr() && checked.error.message).toContain(
      "cannot use the harnexus call socket",
    );
  });
});

describe("callSocketPath", () => {
  test.each([
    { env: { HOME: "/h" }, want: "/h/.local/state/harnexus/call.sock" },
    {
      env: { HOME: "/h", HARNEXUS_STATE_PATH: "/s/threads.json" },
      want: "/s/call.sock",
    },
    { env: { HARNEXUS_CALL_SOCKET: "/x.sock" }, want: "/x.sock" },
    {
      env: { HARNEXUS_CALL_SOCKET: "off" },
      want: { error: "HARNEXUS_CALL_SOCKET is off" },
    },
    {
      env: { HARNEXUS_CALL_SOCKET: "x.sock" },
      want: {
        error: "HARNEXUS_CALL_SOCKET must be on, off or an absolute path",
      },
    },
  ])("finds the socket harnexus serves for %o", ({ env, want }) => {
    expect(callSocketPath(env)).toEqual(want);
  });
});
