import { createHash } from "node:crypto";
import { dirname, join } from "node:path";

import { Result } from "better-result";
import { z } from "zod";

import {
  isDirectory,
  listDirectoryIfExists,
  prepareDirectory,
  readTextFileIfExists,
  writeFileAtomic,
} from "../lib/fs.boundary.ts";
import { parseJson } from "../lib/json.boundary.ts";
import { isObject } from "../lib/object.ts";
import {
  StateUnreadable,
  type StateUnwritable,
  stateUnreadable,
  stateUnwritable,
} from "./failure.ts";
import type { PrRef } from "./github.ts";
import { parseWatchState, type WatchState } from "./state.ts";

// Every path below is under the root the caller gives, so the watch never decides where its records live.
export type PrLink = PrRef & { url: string; worker: string; linkedAt: string };

export const githubPr = (value: string) => {
  const matched = GITHUB_PR.exec(value);
  if (matched === null) return null;

  const [, owner, name, number] = matched;
  return {
    repo: `${owner}/${name}`,
    number: Number(number),
    url: `https://github.com/${owner}/${name}/pull/${number}`,
  };
};

// The watch reaches GitHub only, so a PR on another host is named but never linked.
const GITHUB_PR =
  /^https:\/\/github\.com\/([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)\/pull\/([1-9][0-9]*)(?:[/?#]\S*)?$/;

export const samePr = (a: PrRef, b: PrRef) =>
  a.repo.toLowerCase() === b.repo.toLowerCase() && a.number === b.number;

export const recordLink = (root: string, link: PrLink) =>
  // Only the link is written here; the running watch is the one writer of a PR's state, and it notices the new chat itself.
  saveJson(linkPath(root, link), link);

export const loadLink = async (
  root: string,
  ref: PrRef,
): Promise<Result<PrLink | null, StateUnreadable>> =>
  (await loadJson(linkPath(root, ref))).map(linkOf);

// Whether a PR's watch still runs for its chat, for a caller deciding whether another chat may take it.
export const linkedChat = (root: string, ref: PrRef) =>
  Result.gen(async function* () {
    const link = yield* Result.await(loadLink(root, ref));
    if (link === null) return Result.ok(null);
    const state = yield* Result.await(loadWatchState(root, ref));
    return Result.ok({
      worker: link.worker,
      // Only a watch stopped for a gone chat has let it go; a halted one may have just reached it.
      active: state.status !== "stopped" && state.status !== "ended",
    });
  });

const linkPath = (root: string, ref: PrRef) =>
  join(root, keyOf(ref), "link.json");

export const listLinks = (root: string, skip: ReadonlySet<string> = EMPTY) =>
  Result.gen(async function* () {
    const names = yield* Result.await(
      listDirectoryIfExists(root).then((listed) =>
        listed.mapError(stateUnreadable),
      ),
    );
    const links: PrLink[] = [];
    for (const key of (names ?? []).sort()) {
      // The keys in skip are not read at all, which spares the watch every PR it has finished with.
      if (skip.has(key) || !(await isDirectory(join(root, key)))) continue;

      const loaded = await loadJson(join(root, key, "link.json"));
      // An unreadable link is skipped, so one broken record cannot stop the watch of every other PR.
      const link = linkOf(loaded.isOk() ? loaded.value : null);
      if (link !== null) links.push(link);
    }

    return Result.ok(links);
  });

const EMPTY: ReadonlySet<string> = new Set();

const linkOf = (value: unknown): PrLink | null =>
  isObject(value) &&
  typeof value.repo === "string" &&
  typeof value.number === "number" &&
  typeof value.url === "string" &&
  typeof value.worker === "string" &&
  typeof value.linkedAt === "string"
    ? {
        repo: value.repo,
        number: value.number,
        url: value.url,
        worker: value.worker,
        linkedAt: value.linkedAt,
      }
    : null;

export const freshState = (): WatchState => ({
  status: "watching",
  reason: null,
  summary: null,
  checkedAt: null,
  changedAt: null,
  seen: [],
  pending: [],
  followUp: [],
  conflictHead: null,
  delivered: null,
  worker: null,
  halted: null,
});

export const isOver = (state: WatchState) =>
  // No command brings an ended PR back, so a watch may stop reading it for as long as it runs; a stopped one is left readable, since naming it from another worker resumes it.
  state.status === "ended";

export const loadWatchState = (root: string, ref: PrRef) =>
  Result.gen(async function* () {
    const path = statePath(root, ref);
    const loaded = yield* Result.await(loadJson(path));
    if (loaded === undefined) return Result.ok(freshState());

    const parsed = parseWatchState(loaded);
    // A damaged record is refused rather than read as empty, so the tracking in it is never reset by the next save.
    if (!parsed.success)
      return Result.err(
        new StateUnreadable({
          cause: parsed.error,
          message: `${path} is invalid: ${z.prettifyError(parsed.error)}`,
        }),
      );
    return Result.ok<WatchState>(parsed.data);
  });

export const saveWatchState = (
  root: string,
  ref: PrRef,
  state: WatchState,
): Promise<Result<void, StateUnwritable>> =>
  saveJson(statePath(root, ref), state);

export const lockPath = (root: string) => join(root, "watch.lock");

// The one chat the watch sends every message from.
export const recordSender = (root: string, thread: string) =>
  saveJson(senderPath(root), { thread, at: new Date().toISOString() });

export const loadSender = (root: string) =>
  Result.gen(async function* () {
    const loaded = yield* Result.await(loadJson(senderPath(root)));
    return Result.ok(
      isObject(loaded) && typeof loaded.thread === "string"
        ? loaded.thread
        : null,
    );
  });

const senderPath = (root: string) => join(root, "sender.json");

const statePath = (root: string, ref: PrRef) =>
  join(root, keyOf(ref), "state.json");

export const keyOf = (ref: PrRef) =>
  // Keyed by the PR, so the latest worker to name it is the one told about it.
  createHash("sha256")
    .update(`${ref.repo.toLowerCase()}#${ref.number}`)
    .digest("hex")
    .slice(0, 16);

const loadJson = (path: string) =>
  Result.gen(async function* () {
    const text = yield* Result.await(
      readTextFileIfExists(path).then((read) => read.mapError(stateUnreadable)),
    );
    // A missing file is undefined, so a stored JSON null is still a record to check.
    if (text === null) return Result.ok(undefined);
    return parseJson(text).mapError(
      (cause) =>
        new StateUnreadable({ cause, message: `${path} is not valid JSON` }),
    );
  });

const saveJson = (path: string, data: unknown) =>
  Result.gen(async function* () {
    yield* Result.await(
      prepareDirectory(dirname(path)).then((made) =>
        made.mapError(stateUnwritable),
      ),
    );
    yield* Result.await(
      // Indented by one space like the other taskctl records, so the files read alike.
      writeFileAtomic(path, JSON.stringify(data, null, 1)).then((written) =>
        written.mapError(stateUnwritable),
      ),
    );
    return Result.ok();
  });
