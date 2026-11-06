import type { Result } from "better-result";

import { parseJson } from "../lib/json.boundary.ts";
import { type Fields, isObject } from "../lib/object.ts";
import { delegationFields, isDelegatingTool } from "./delegation.ts";
import type { Answer, Workers } from "../prwatch/worker.ts";

// The PR watch's workers, Codex chats linked to a PR, reached through the app.
export const codexWorkers = (
  callApp: AppCall,
  // Every message comes from the one chat registered with watch sender, so the watch depends on no other command's records.
  registered: () => Promise<Result<string | null, { message: string }>>,
): Workers => ({
  senderOf: async (worker) => senderFor(await registered(), worker),

  look: async (sender, worker, awaited, now) => {
    const view = await readThread(callApp, sender, worker, awaited?.at ?? null);
    if (view === null || view.gone) return view;

    const turn = awaited === null ? undefined : answerTo(view.turns, awaited);
    return {
      gone: false,
      busy: busyReason(view, now),
      answer: turn === undefined ? null : answerOf(turn),
    };
  },

  send: async (sender, worker, prompt) => {
    const detail = await callApp(sender, "send_message_to_thread", {
      threadId: worker,
      prompt,
    });
    const outcome = String(detail.outcome);
    return {
      outcome:
        outcome === "done"
          ? "done"
          : NOT_SENT.includes(outcome)
            ? "not_sent"
            : "unknown",
      detail,
    };
  },
});

// The outcomes the call socket reports for a request that never reached the app; unsupported is the client's own, for a bridge it never wrote to.
const NOT_SENT = ["not_sent", "rejected", "invalid", "unsupported"];

const answerOf = (turn: Turn): Answer => ({
  status:
    turn.status === "inProgress" || turn.status === "completed"
      ? turn.status
      : "other",
  text: turn.answer,
});

// The chat that sends to the worker, or why nothing can; shared with the warning name --pr and watch --link print.
export const senderFor = (
  registered: Result<string | null, { message: string }>,
  worker: string,
): string | { missing: string } => {
  if (registered.isErr())
    return {
      missing: `cannot read the registered sender: ${registered.error.message}`,
    };
  if (registered.value === null) return { missing: NO_SENDER };
  // A thread cannot message itself, so the registered chat is never sent to from itself.
  if (registered.value === worker)
    return {
      missing:
        "the registered sender is this chat itself: run pr-wake watch sender in another chat",
    };
  return registered.value;
};

const NO_SENDER =
  "no chat is registered to send from: run pr-wake watch sender in a chat kept for it";

type AppCall = (caller: string, tool: string, args: Fields) => Promise<Fields>;

type ThreadView =
  | { gone: true }
  | { gone: false; active: boolean; turns: Turn[] };

type Turn = {
  status: string;
  startedAt: number | null;
  completedAt: number | null;
  origin: "owner" | "delegated" | "other";
  // The thread and message of a delegated turn, which tell our message apart from any other thread's.
  delegation: { source: string; input: string } | null;
  answer: string;
};

// A worker's thread, or null when the app could not be asked, as while it is closed or the Mac sleeps; the next cycle asks again.
const readThread = async (
  callApp: AppCall,
  caller: string,
  thread: string,
  sentAt: string | null,
): Promise<ThreadView | null> => {
  const first = await readPage(callApp, caller, thread, null);
  if (first === null || first.gone) return first;

  const turns = [...first.turns];
  const after = sentAt === null ? null : turnsAfter(sentAt);
  let cursor = first.nextCursor;
  // While an answer is awaited, older pages are read until they reach when the message was sent, so later talk in the chat cannot hide the answer.
  for (let page = 1; page < MAX_PAGES; page++) {
    const oldest = turns.at(-1)?.startedAt ?? null;
    if (after === null || cursor === null || oldest === null || oldest < after)
      break;

    const older = await readPage(callApp, caller, thread, cursor);
    if (older === null || older.gone) return older;
    turns.push(...older.turns);
    cursor = older.nextCursor;
  }

  return { gone: false, active: first.active, turns };
};

// A runaway cursor stops here; the answer wait then gives up as for a lost turn.
const MAX_PAGES = 20;

const readPage = async (
  callApp: AppCall,
  caller: string,
  thread: string,
  cursor: string | null,
) => {
  // A Codex thread shows a delegation only in the call's output, so outputs are read but cut short.
  const answer = await callApp(caller, "read_thread", {
    threadId: thread,
    turnLimit: 5,
    includeOutputs: true,
    maxOutputCharsPerItem: OUTPUT_CHARS,
    ...(cursor === null ? {} : { cursor }),
  });
  const text = resultText(answer);
  // Protocol 1 names a failed tool by its kind and repeats the app's text as the message.
  const failure = typeof answer.message === "string" ? answer.message : text;
  if (answer.outcome !== "done" && failure.includes(NO_THREAD))
    return { gone: true as const };
  if (answer.outcome !== "done") return null;

  const parsed = parseJson(text);
  const read = parsed.isOk() && isObject(parsed.value) ? parsed.value : null;
  if (read === null || !isObject(read.thread)) return null;

  const status = isObject(read.thread.status) ? read.thread.status.type : null;
  const turns = (Array.isArray(read.turns) ? read.turns : [])
    .filter(isObject)
    .map(turnOf);
  const page = isObject(read.page) ? read.page : {};
  const nextCursor =
    page.hasMore === true && typeof page.nextCursor === "string"
      ? page.nextCursor
      : null;
  return {
    gone: false as const,
    active: status === "active",
    turns,
    nextCursor,
  };
};

const OUTPUT_CHARS = 2000;

// What the app answers for a thread that was deleted or never existed.
const NO_THREAD = "No Codex thread found";

const resultText = (answer: Fields) => {
  const result = answer.result;
  if (!isObject(result) || !Array.isArray(result.content)) return "";

  return result.content
    .flatMap((item) =>
      isObject(item) && typeof item.text === "string" ? [item.text] : [],
    )
    .join("\n");
};

const turnOf = (turn: Record<string, unknown>): Turn => {
  const items = Array.isArray(turn.items) ? turn.items : [];
  const delegation = delegationOf(items[0]);
  return {
    status: String(turn.status ?? ""),
    startedAt: numberOrNull(turn.startedAt),
    completedAt: numberOrNull(turn.completedAt),
    origin:
      delegation !== null
        ? "delegated"
        : isObject(items[0]) && items[0].type === "userMessage"
          ? "owner"
          : "other",
    delegation,
    answer: finalTextOf(items),
  };
};

const delegationOf = (first: unknown) => {
  if (!isObject(first)) return null;
  // A Claude thread shows a message from another thread as a user message carrying the delegation, and a Codex thread as the output of the call that sent it.

  const carried = first.codexDelegation;
  if (
    isObject(carried) &&
    typeof carried.sourceThreadId === "string" &&
    typeof carried.input === "string"
  )
    return { source: carried.sourceThreadId, input: carried.input };

  if (
    first.type === "functionCallOutput" &&
    isDelegatingTool(String(first.name))
  )
    return parseDelegation(isObject(first.output) ? first.output.text : null);
  if (first.type === "userMessage" && Array.isArray(first.content)) {
    const found: unknown = first.content.find(
      (part) => isObject(part) && typeof part.text === "string",
    );
    return parseDelegation(isObject(found) ? found.text : null);
  }

  return null;
};

const parseDelegation = (text: unknown) => {
  if (
    typeof text !== "string" ||
    !text.trimStart().startsWith("<codex_delegation>")
  )
    return null;

  return delegationFields(text);
};

const finalTextOf = (items: unknown[]) => {
  const messages = items.filter(
    (item): item is Record<string, unknown> =>
      isObject(item) && item.type === "agentMessage",
  );
  const finals = messages.filter((item) => item.phase === "final_answer");
  const chosen = finals.length > 0 ? finals : messages.slice(-1);
  return chosen.map((item) => String(item.text ?? "")).join("\n");
};

const numberOrNull = (value: unknown) =>
  typeof value === "number" ? value : null;

const busyReason = (view: { active: boolean; turns: Turn[] }, now: Date) => {
  const latest = view.turns[0];
  if (view.active || latest?.status === "inProgress")
    return "worker turn in progress";
  if (
    latest?.origin === "owner" &&
    latest.completedAt !== null &&
    now.getTime() - latest.completedAt * 1000 < OWNER_GRACE_MS
  )
    return "owner just talked to the worker";
  return null;
};

// A worker the owner just talked to may still be in that conversation, so a message waits this long after the owner's turn.
const OWNER_GRACE_MS = 5 * 60_000;

// The turn our message started: sent from our thread with our text, no earlier than we sent it.
const answerTo = (
  turns: readonly Turn[],
  sent: { at: string; sender: string; prompt: string },
) => {
  const after = turnsAfter(sent.at);
  return (
    turns
      .filter(
        (turn) =>
          turn.delegation !== null &&
          turn.delegation.source === sent.sender &&
          isPromptOf(turn.delegation.input, sent.prompt) &&
          turn.startedAt !== null &&
          turn.startedAt >= after,
      )
      // The read lists turns newest first.
      .at(-1)
  );
};

const isPromptOf = (input: string, prompt: string) =>
  // A Codex thread's copy may be cut short, so a nonempty start of the prompt counts as the prompt.
  input.trim() !== "" && prompt.startsWith(input.trimEnd());

const turnsAfter = (sentAt: string) =>
  Date.parse(sentAt) / 1000 - CLOCK_SKEW_SECONDS;

// The app stamps turns in whole seconds.
const CLOCK_SKEW_SECONDS = 5;
