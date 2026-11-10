import { join } from "node:path";

import { parseJson } from "../lib/json.boundary.ts";
import { type Fields, isObject } from "../lib/object.ts";
import type { Answer, Workers } from "../prwatch/worker.ts";

// A linked worker in T3 Code is recorded with this before its thread, so a link tells which app to reach it through.
export const T3_WORKER = "t3:";

export const isT3Worker = (worker: string) => worker.startsWith(T3_WORKER);

// The t3 CLI sends as the person using T3 Code, so every message to a T3 Code worker is recorded as from it.
export const T3_SENDER = "t3";

export type T3Ran =
  | { ran: true; status: number; stdout: string; stderr: string }
  // started tells a command that may have acted from one that never ran.
  | { ran: false; started: boolean; message: string };

export type T3Run = (
  args: string[],
  input: string,
  timeoutMs: number,
) => Promise<T3Ran>;

// The PR watch's workers in T3 Code, reached through T3 Code's t3 CLI.
export const t3Workers = (run: T3Run): Workers => ({
  senderOf: () => Promise.resolve(T3_SENDER),

  look: async (_sender, worker, awaited) => {
    const shown = await run(
      [
        "thread",
        "show",
        threadOf(worker),
        "--messages",
        String(MESSAGES),
        "--json",
      ],
      "",
      SHOW_TIMEOUT_MS,
    );
    if (!shown.ran) return null;
    if (shown.status !== 0)
      return GONE.test(shown.stderr) ? { gone: true } : null;

    const view = viewOf(printed(shown.stdout));
    if (view === null) return null;
    return {
      gone: false,
      busy: busyReason(view.thread),
      answer: awaited === null ? null : answerTo(view, awaited.prompt),
    };
  },

  send: async (_sender, worker, prompt) => {
    const sent = await run(
      ["thread", "send", threadOf(worker), "-", "--json"],
      prompt,
      SEND_TIMEOUT_MS,
    );
    if (!sent.ran)
      return {
        outcome: sent.started ? "unknown" : "not_sent",
        detail: { error: sent.message },
      };
    if (sent.status === 0)
      return { outcome: "done", detail: printed(sent.stdout) ?? {} };

    const error = brief(sent.stderr) || `t3 exited with ${sent.status}`;
    return {
      outcome: BEFORE_SENDING.test(sent.stderr) ? "not_sent" : "unknown",
      detail: { error },
    };
  },
});

// The t3 CLI that the watch runs: HARNEXUS_T3_BIN when set, then t3 on PATH, then the launcher the desktop app writes.
export const t3Binary = (
  env: Record<string, string | undefined>,
  home: string,
  which: (name: string) => string | null,
) => {
  const set = env.HARNEXUS_T3_BIN;
  if (set !== undefined && set !== "") return set.startsWith("/") ? set : null;
  const t3Home = env.T3CODE_HOME?.startsWith("/")
    ? env.T3CODE_HOME
    : join(home, ".t3");
  return which("t3") ?? join(t3Home, "bin/t3");
};

const threadOf = (worker: string) => worker.slice(T3_WORKER.length);

// Enough of the latest messages to still hold the watch's own after the worker's answer to it.
const MESSAGES = 50;
const SHOW_TIMEOUT_MS = 30_000;
const SEND_TIMEOUT_MS = 60_000;

// What the t3 CLI answers for a thread that was deleted or never existed.
const GONE = /is no longer available/;

// Failures the t3 CLI reports before it reaches the server; any other may come after the message was queued.
const BEFORE_SENDING =
  /No running T3 Code server|is not a thread id|Failed to reserve loopback port|listen EPERM/;

type Thread = {
  status: string;
  activeRunId: string | null;
  latestRunId: string | null;
  pendingRequestCount: number;
};

type Message = { role: string; runId: string | null; text: string };

type View = { thread: Thread; messages: Message[] };

const busyReason = (thread: Thread) => {
  if (thread.activeRunId !== null || thread.status === "running")
    return "worker turn in progress";
  if (thread.pendingRequestCount > 0) return "worker is waiting for an answer";
  return null;
};

// The run our message started and its last reply; the t3 CLI shows no times, so the latest message with our text is ours.
const answerTo = (view: View, prompt: string): Answer | null => {
  const asked = view.messages
    .map((message, index) =>
      message.role === "user" && message.text.trimEnd() === prompt.trimEnd()
        ? index
        : -1,
    )
    .reduce((last, index) => Math.max(last, index), -1);
  const run = view.messages[asked]?.runId ?? null;
  if (run === null) return null;

  const replies = view.messages
    .slice(asked + 1)
    .filter((message) => message.role === "assistant" && message.runId === run);
  const text = replies.at(-1)?.text ?? "";
  return { status: runStatus(view.thread, run, text), text };
};

const runStatus = (
  thread: Thread,
  run: string,
  text: string,
): Answer["status"] => {
  if (run === thread.activeRunId) return "inProgress";
  if (run === thread.latestRunId)
    return thread.status === "completed"
      ? "completed"
      : thread.status === "running"
        ? "inProgress"
        : "other";
  // A run a later one followed has ended, and only its reply tells whether it was answered.
  return text === "" ? "other" : "completed";
};

const viewOf = (shown: Fields | null): View | null => {
  const thread = shown?.thread;
  if (!isObject(thread) || !Array.isArray(shown?.messages)) return null;

  return {
    thread: {
      status: String(thread.status ?? ""),
      activeRunId: textOrNull(thread.activeRunId),
      latestRunId: textOrNull(thread.latestRunId),
      pendingRequestCount:
        typeof thread.pendingRequestCount === "number"
          ? thread.pendingRequestCount
          : 0,
    },
    messages: shown.messages.flatMap((message) =>
      isObject(message) && typeof message.text === "string"
        ? [
            {
              role: String(message.role ?? ""),
              runId: textOrNull(message.runId),
              text: message.text,
            },
          ]
        : [],
    ),
  };
};

// The one JSON object a --json command prints, on its last line.
const printed = (stdout: string) => {
  const last = stdout.trim().split("\n").at(-1) ?? "";
  const parsed = parseJson(last).unwrapOr(null);
  return isObject(parsed) ? parsed : null;
};

const textOrNull = (value: unknown) =>
  typeof value === "string" ? value : null;

const brief = (text: string) => text.trim().slice(0, 500);
