import { Result } from "better-result";
import { z } from "zod";

import { readRegularTextFile } from "../lib/fs.boundary.ts";
import { parseJson } from "../lib/json.boundary.ts";
import { compileRegex } from "../lib/regex.boundary.ts";
import {
  ConfigInvalid,
  type ConfigUnreadable,
  configUnreadable,
} from "./failure.ts";
import { PRESETS, type ReviewBot } from "./reviewers.ts";

export type WatchConfig = Omit<z.infer<typeof CONFIG>, "reviewBots"> & {
  reviewBots: ReviewBot[];
};

export type IgnoreRule = z.infer<typeof IGNORE_RULE>;

export type ItemKind = (typeof ITEM_KINDS)[number];

const invalid = (message: string) => Result.err(new ConfigInvalid({ message }));

export const loadConfig = (
  path: string,
): Promise<Result<WatchConfig, ConfigInvalid | ConfigUnreadable>> =>
  Result.gen(async function* () {
    const text = yield* Result.await(
      readRegularTextFile(path).then((read) => read.mapError(configUnreadable)),
    );
    const raw = parseJson(text);
    if (raw.isErr()) return invalid(`${path} is not valid JSON`);
    const parsed = CONFIG.safeParse(raw.value);
    if (!parsed.success)
      return invalid(`${path} is invalid: ${z.prettifyError(parsed.error)}`);

    // Lower-cased once here, since GitHub matches owners and names without case.
    const repositories = Object.fromEntries(
      Object.entries(parsed.data.repositories).map(([name, value]) => [
        name.toLowerCase(),
        value,
      ]),
    );
    const reviewBots = parsed.data.reviewBots.map((bot) =>
      typeof bot === "string" ? { ...PRESETS[bot] } : bot,
    );
    const ids = reviewBots.map((bot) => bot.id);
    if (new Set(ids).size !== ids.length)
      return invalid(`${path} is invalid: reviewBots repeats an id`);
    // A comment is credited to one bot by its login, so two bots cannot share one.
    const logins = reviewBots.map((bot) => bot.login.toLowerCase());
    if (new Set(logins).size !== logins.length)
      return invalid(`${path} is invalid: reviewBots repeats a login`);
    return Result.ok({ ...parsed.data, repositories, reviewBots });
  });

// Zod builds each schema from the ones it names, so they are declared from the leaves up.
const LOGIN = z.string().regex(/^[A-Za-z0-9][A-Za-z0-9_.-]*(\[bot\])?$/);

export const ITEM_KINDS = ["inline", "review", "issue_comment"] as const;

const IGNORE_RULE = z.strictObject({
  author: LOGIN,
  kind: z.enum(ITEM_KINDS).optional(),
  bodyIncludes: z.string().min(1).optional(),
});

const CLOCK = z.string().regex(/^([01][0-9]|2[0-3]):[0-5][0-9]$/);

const PATTERN = z
  .string()
  .min(1)
  .refine((source) => compileRegex(source).isOk(), {
    message: "is not a regular expression",
  });

const REVIEW_BOT = z.strictObject({
  id: z.string().regex(/^[a-z0-9-]+$/),
  name: z.string().min(1),
  login: LOGIN,
  status: z
    .union([
      z.strictObject({
        context: z.string().min(1),
        completed: z.string().min(1),
      }),
      z.strictObject({ checkRun: z.string().min(1) }),
    ])
    .nullable()
    .default(null),
  actionableReview: z.string().min(1).nullable().default(null),
  noActionable: z.string().min(1).nullable().default(null),
  severity: PATTERN.nullable().default(null),
});

const PRESET_NAMES = Object.keys(PRESETS) as [keyof typeof PRESETS];

// The account is fixed per repository, so switching the active gh account never points a poll at a repository the other account cannot see.
const CONFIG = z.strictObject({
  repositories: z.record(
    z.string().regex(/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/),
    z.strictObject({ account: LOGIN }),
  ),
  // A preset by name, or a bot described in full.
  reviewBots: z
    .array(z.union([z.enum(PRESET_NAMES), REVIEW_BOT]))
    .default(["coderabbit"]),
  ignore: z.array(IGNORE_RULE).default([]),
  pollIntervalSeconds: z.number().int().min(30).default(60),
  idleIntervalSeconds: z.number().int().min(60).default(600),
  idleAfterHours: z.number().positive().default(72),
  quietHours: z
    .strictObject({ start: CLOCK, end: CLOCK })
    .nullable()
    .default({ start: "22:00", end: "05:00" }),
  // Put at the top of every message, as a skill to use or the way to report.
  instructions: z.string().trim().min(1).nullable().default(null),
});
