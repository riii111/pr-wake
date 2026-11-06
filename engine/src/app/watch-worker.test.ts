import { describe, expect, test } from "bun:test";

import { Result } from "better-result";

import type { Fields } from "../lib/object.ts";
import { codexWorkers } from "./watch-worker.ts";

const answered = (read: unknown) => async () => ({
  outcome: "done",
  result: { content: [{ type: "text", text: JSON.stringify(read) }] },
});

const NOW = new Date(1_000_000_000);

const look = (
  call: (caller: string, tool: string, args: Fields) => Promise<Fields>,
  awaited: { at: string; sender: string; prompt: string } | null,
) =>
  codexWorkers(call, async () => Result.ok(null)).look(
    "reviewer",
    "worker",
    awaited,
    NOW,
  );

describe("codexWorkers.look", () => {
  test("matches an escaped delegation in a user message", async () => {
    const prompt = "read <file> & reply";
    const view = await look(
      answered({
        thread: { status: { type: "idle" } },
        turns: [
          {
            status: "completed",
            startedAt: 100,
            items: [
              {
                type: "userMessage",
                content: [
                  {
                    text: "  <codex_delegation><source_thread_id>reviewer</source_thread_id><input>read &lt;file&gt; &amp; reply</input></codex_delegation>",
                  },
                ],
              },
              { type: "agentMessage", phase: "final_answer", text: "done" },
            ],
          },
        ],
      }),
      { at: new Date(100_000).toISOString(), sender: "reviewer", prompt },
    );

    expect(view).toEqual({
      gone: false,
      busy: null,
      answer: { status: "completed", text: "done" },
    });
  });

  test("reads a Codex worker's delegation from the call output, even when cut short", async () => {
    const prompt =
      "$task-worker\nPR <https://github.com/org/pay/pull/7> で対応が必要なこと";
    const calls: Fields[] = [];

    const view = await look(
      async (caller, tool, args) => {
        calls.push({ caller, tool, ...args });
        return answered({
          thread: { status: { type: "idle" } },
          turns: [
            {
              status: "completed",
              startedAt: 100,
              completedAt: 160,
              items: [
                {
                  type: "functionCallOutput",
                  name: "send_message_to_thread",
                  output: {
                    text: "<codex_delegation>\n  <source_thread_id>reviewer</source_thread_id>\n  <input>$task-worker\nPR &lt;https://github.com/org/pay/pull/7&gt; で対応",
                    truncated: true,
                  },
                },
                { type: "agentMessage", phase: "commentary", text: "読むのだ" },
                {
                  type: "agentMessage",
                  phase: "final_answer",
                  text: "指摘なし",
                },
              ],
            },
          ],
        })();
      },
      { at: new Date(100_000).toISOString(), sender: "reviewer", prompt },
    );

    expect(calls).toMatchObject([
      {
        caller: "reviewer",
        tool: "read_thread",
        threadId: "worker",
        includeOutputs: true,
      },
    ]);
    expect(view).toEqual({
      gone: false,
      busy: null,
      answer: { status: "completed", text: "指摘なし" },
    });
  });

  test("reads older pages until it reaches the answer to the sent message", async () => {
    const sent = {
      at: new Date(100_000).toISOString(),
      sender: "reviewer",
      prompt: "$task-worker\nPR",
    };
    const app = pagedApp([
      [ownerTurn(400), ownerTurn(390)],
      [ownerTurn(300), delegatedTurn(100, sent.prompt)],
      [ownerTurn(50)],
      [ownerTurn(10)],
    ]);

    const view = await look(app.call, sent);

    expect(view !== null && !view.gone && view.answer?.text).toBe("answered");
    expect(app.cursors).toEqual([undefined, "page:1", "page:2"]);
  });

  test("reads only the latest page while no answer is awaited", async () => {
    const app = pagedApp([[ownerTurn(400)], [ownerTurn(300)]]);

    const view = await look(app.call, null);

    expect(view !== null && !view.gone && view.answer).toBeNull();
    expect(app.cursors).toEqual([undefined]);
  });

  test.each([
    {
      name: "a deleted thread as gone",
      answer: {
        outcome: "tool_error",
        result: {
          content: [
            { type: "text", text: "No Codex thread found for threadId: w" },
          ],
          isError: true,
        },
      },
      expected: { gone: true },
    },
    {
      name: "an app that cannot be reached as unknown",
      answer: { outcome: "not_sent", error: "ENOENT" },
      expected: null,
    },
  ])("reports $name", async ({ answer, expected }) => {
    const view = await look(async () => answer, null);

    expect(view).toEqual(expected);
  });
});

describe("codexWorkers.look busy reason", () => {
  const at = (secondsAgo: number) => NOW.getTime() / 1000 - secondsAgo;

  test.each([
    {
      name: "a running thread",
      active: true,
      latest: ownerTurn(at(600)),
      expected: "worker turn in progress",
    },
    {
      name: "a turn still in progress",
      active: false,
      latest: {
        ...ownerTurn(at(600)),
        status: "inProgress",
        completedAt: null,
      },
      expected: "worker turn in progress",
    },
    {
      name: "the owner's turn that ended a minute ago",
      active: false,
      latest: ownerTurn(at(60)),
      expected: "owner just talked to the worker",
    },
    {
      name: "the owner's turn that ended ten minutes ago",
      active: false,
      latest: ownerTurn(at(600)),
      expected: null,
    },
    {
      name: "another thread's turn that ended a minute ago",
      active: false,
      latest: delegatedTurn(at(60), "hi"),
      expected: null,
    },
  ])("tells $name", async ({ active, latest, expected }) => {
    const view = await look(
      answered({
        thread: { status: { type: active ? "active" : "idle" } },
        turns: [latest],
      }),
      null,
    );

    expect(view).toEqual({ gone: false, busy: expected, answer: null });
  });
});

const pagedApp = (pages: Fields[][]) => {
  const cursors: unknown[] = [];
  const call = async (_caller: string, _tool: string, args: Fields) => {
    cursors.push(args.cursor);
    const index =
      typeof args.cursor === "string" ? Number(args.cursor.slice(5)) : 0;
    const hasMore = index + 1 < pages.length;
    return answered({
      thread: { status: { type: "idle" } },
      turns: pages[index] ?? [],
      page: { hasMore, nextCursor: hasMore ? `page:${index + 1}` : null },
    })();
  };

  return { call, cursors };
};

const ownerTurn = (startedAt: number) => ({
  status: "completed",
  startedAt,
  completedAt: startedAt,
  items: [{ type: "userMessage", content: [{ type: "text", text: "hi" }] }],
});

const delegatedTurn = (startedAt: number, input: string) => ({
  status: "completed",
  startedAt,
  completedAt: startedAt,
  items: [
    {
      type: "userMessage",
      codexDelegation: { sourceThreadId: "reviewer", input },
    },
    { type: "agentMessage", phase: "final_answer", text: "answered" },
  ],
});
