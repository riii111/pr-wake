import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Result } from "better-result";

import { callApp, callSocketPath } from "./app/call.ts";
import { ProtocolUnsupported } from "./app/protocol.ts";
import { type EngineDeps, runEngine } from "./cli.ts";
import type { Fields } from "./lib/object.ts";

const cleanups: (() => Promise<unknown>)[] = [];

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
      runCommand: async () => {
        throw new Error("no command runs in these tests");
      },
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

describe("callApp", () => {
  test("reports a connected call without an answer in time as unknown", async () => {
    const { root } = await fixture();
    const silent = join(root, "silent.sock");
    const server = createServer(() => {});
    await new Promise<void>((resolve) => server.listen(silent, resolve));
    cleanups.push(async () => {
      server.close();
    });

    const answer = await callApp(
      { HARNEXUS_CALL_SOCKET: silent },
      "caller",
      "send_message_to_thread",
      {},
      50,
    );

    expect(answer.outcome).toBe("unknown");
  });

  test("sends one request line and reads the answer line", async () => {
    const { root } = await fixture();
    const path = join(root, "call.sock");
    const lines: string[] = [];
    const server = createServer((socket) => {
      socket.setEncoding("utf8");
      socket.on("data", (chunk: string) => {
        lines.push(chunk);
        socket.end(`${JSON.stringify({ outcome: "done", result: 1 })}\n`);
      });
    });
    await new Promise<void>((resolve) => server.listen(path, resolve));
    cleanups.push(async () => {
      server.close();
    });

    const answer = await callApp(
      { HARNEXUS_CALL_SOCKET: path },
      "caller",
      "read_thread",
      { threadId: "w" },
    );

    expect(answer).toEqual({ outcome: "done", result: 1 });
    expect(lines.map((line) => JSON.parse(line))).toEqual([
      { threadId: "caller", tool: "read_thread", arguments: { threadId: "w" } },
    ]);
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
