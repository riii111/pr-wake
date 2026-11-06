import { z } from "zod";

import { ITEM_KINDS } from "./config.ts";

export type WatchState = z.infer<typeof WATCH_STATE>;

export type WatchEvent = z.infer<typeof WATCH_EVENT>;

export type Finding = z.infer<typeof FINDING>;

export type PrSummary = z.infer<typeof PR_SUMMARY>;

export type BotStatus = z.infer<typeof BOT_STATUS>;

export type FailedCheck = z.infer<typeof FAILED_CHECK>;

export const parseWatchState = (value: unknown) =>
  STORED_WATCH_STATE.safeParse(value);

export const botKey = (id: string) => `${BOT_KEY}${id}`;

// Marks a bot whose reviews the watch has taken in.
const BOT_KEY = "bot:";

// Zod builds each schema from the ones it names, so they are declared from the leaves up.
// A check run is read into a commit status's terms: PENDING until it completes, then its conclusion, with an empty description.
const BOT_STATUS = z.object({
  state: z.string(),
  description: z.string(),
  createdAt: z.string(),
});

// What the light poll reads; a change in any of it is what makes the full read worth its cost.
const PR_SUMMARY = z.object({
  state: z.string(),
  isDraft: z.boolean(),
  head: z.string(),
  updatedAt: z.string(),
  mergeable: z.string(),
  // The progress of each review bot that keeps one, by the bot's id.
  bots: z.record(z.string(), BOT_STATUS.nullable()),
  rollup: z.string().nullable(),
});

const FAILED_CHECK = z.object({ name: z.string(), url: z.string().nullable() });

const FINDING = z.object({
  id: z.string(),
  kind: z.enum(ITEM_KINDS),
  // The id of the review bot that wrote it, or null for a person.
  bot: z.string().nullable(),
  label: z.string(),
  path: z.string().nullable(),
  line: z.number().nullable(),
  url: z.string(),
  createdAt: z.string(),
  // A person's approval, which is told even without a body; null for any other finding.
  approval: z.enum(["with_body", "without_body"]).nullable(),
});

const REVIEW_DONE = z.object({
  type: z.literal("review_done"),
  // The bot's id and the name shown to the worker.
  bot: z.string(),
  name: z.string(),
  head: z.string(),
  findings: z.number(),
  summaryAgrees: z.boolean(),
});

const CONFLICT = z.object({ type: z.literal("conflict"), head: z.string() });

const CI_FAILED = z.object({
  type: z.literal("ci_failed"),
  head: z.string(),
  check: FAILED_CHECK,
});

const WATCH_EVENT = z.discriminatedUnion("type", [
  z.object({ type: z.literal("finding"), finding: FINDING }),
  REVIEW_DONE,
  CONFLICT,
  CI_FAILED,
]);

const DELIVERED = z.object({
  at: z.string(),
  sender: z.string(),
  prompt: z.string(),
  findings: z.array(FINDING),
  followUp: z.boolean(),
});

const WATCH_STATE = z.object({
  status: z.enum(["watching", "draft", "ended", "stopped", "halted"]),
  reason: z.string().nullable(),
  summary: PR_SUMMARY.nullable(),
  checkedAt: z.string().nullable(),
  changedAt: z.string().nullable(),
  // Keys of every event already queued or sent, so none is sent twice.
  seen: z.array(z.string()),
  pending: z.array(WATCH_EVENT),
  followUp: z.array(FINDING),
  conflictHead: z.string().nullable(),
  delivered: DELIVERED.nullable(),
  // The chat the delivery fields belong to; a different chat in the link means the PR was handed over.
  worker: z.string().nullable(),
  // The message whose delivery could not be confirmed; nothing more is sent for the PR until it is released.
  halted: z.record(z.string(), z.unknown()).nullable(),
});

// Records written before review bots were configurable name CodeRabbit in their own fields, and are read as the coderabbit preset.
const STORED_FINDING = FINDING.extend({
  bot: FINDING.shape.bot.optional(),
  approval: FINDING.shape.approval.optional(),
  coderabbit: z.boolean().optional(),
}).transform(({ coderabbit, ...finding }): Finding => ({
  ...finding,
  bot:
    finding.bot !== undefined
      ? finding.bot
      : coderabbit === true
        ? "coderabbit"
        : null,
  // A finding written before approvals were told is never one.
  approval: finding.approval ?? null,
}));

const STORED_SUMMARY = PR_SUMMARY.extend({
  bots: PR_SUMMARY.shape.bots.optional(),
  coderabbit: BOT_STATUS.nullable().optional(),
}).transform(({ coderabbit, bots, ...summary }): PrSummary => ({
  ...summary,
  bots: bots ?? { coderabbit: coderabbit ?? null },
}));

const LEGACY_REVIEW_DONE = z
  .object({
    type: z.literal("coderabbit_done"),
    head: z.string(),
    findings: z.number(),
    summaryAgrees: z.boolean(),
  })
  .transform(({ head, findings, summaryAgrees }): WatchEvent => ({
    type: "review_done",
    bot: "coderabbit",
    name: "CodeRabbit",
    head,
    findings,
    summaryAgrees,
  }));

const STORED_EVENT = z.union([
  z.object({ type: z.literal("finding"), finding: STORED_FINDING }),
  REVIEW_DONE,
  CONFLICT,
  CI_FAILED,
  LEGACY_REVIEW_DONE,
]);

const STORED_WATCH_STATE = WATCH_STATE.extend({
  summary: STORED_SUMMARY.nullable(),
  pending: z.array(STORED_EVENT),
  followUp: z.array(STORED_FINDING),
  delivered: DELIVERED.extend({ findings: z.array(STORED_FINDING) }).nullable(),
  // A state written before a PR followed its chat has no worker, which the watch takes as the linked one.
  worker: WATCH_STATE.shape.worker.default(null),
}).transform((state): WatchState => ({
  ...state,
  // Its CodeRabbit key is taken as the bot already known, so the watch does not read CodeRabbit as newly added.
  seen:
    state.summary !== null && !state.seen.some((key) => key.startsWith(BOT_KEY))
      ? [...state.seen, botKey("coderabbit")]
      : state.seen,
}));
