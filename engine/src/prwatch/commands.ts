import { Result } from "better-result";

import { holdProcessLock, prepareDirectory } from "../lib/fs.boundary.ts";
import type { Fields } from "../lib/object.ts";
import { classify, type Verdict } from "./classify.ts";
import { loadConfig, type WatchConfig } from "./config.ts";
import { nextEvents } from "./events.ts";
import {
  refuse,
  type StateUnwritable,
  stateUnwritable,
  type WatchRefused,
  WatchReplaced,
} from "./failure.ts";
import type { GitHub, PrDetail, PrRef } from "./github.ts";
import { messageOf } from "./message.ts";
import type { WatchState } from "./state.ts";
import {
  freshState,
  githubPr,
  listLinks,
  loadSender,
  loadWatchState,
  lockPath,
  samePr,
  saveWatchState,
} from "./store.ts";
import { runCycle, type WatchDeps, type WatchPaths } from "./watcher.ts";

// Runs the PR watch until it is stopped or its program is replaced.
export const watchForever = (
  deps: WatchDeps,
  sleep: (ms: number) => Promise<unknown>,
  replaced: () => Promise<boolean> = async () => false,
) =>
  Result.gen(async function* () {
    yield* Result.await(acquireLock(deps.paths.state));
    deps.log({ event: "started", pid: process.pid });

    const cycleDeps = { ...deps, over: deps.over ?? new Set<string>() };
    for (;;) {
      // Checked before each cycle, so no cycle runs on a program replaced while the watch slept.
      if (await replaced()) {
        deps.log({ event: "replaced" });
        // Ending between cycles, never mid-send, lets the service that keeps the watch running start the new program.
        return Result.err(
          new WatchReplaced({
            message:
              "the watch's program was replaced; exiting to be restarted",
          }),
        );
      }

      const cycle = await runCycle(cycleDeps);
      // A failed cycle is logged and retried, so a config or network error never ends the watch.
      if (cycle.isErr())
        deps.log({ event: "cycle_failed", error: cycle.error._tag });
      await sleep(
        cycle.isOk() ? cycle.value.pollIntervalSeconds * 1000 : RETRY_MS,
      );
    }
  });

const RETRY_MS = 60_000;

const acquireLock = (
  root: string,
): Promise<Result<void, StateUnwritable | WatchRefused>> =>
  Result.gen(async function* () {
    yield* Result.await(prepareDirectory(root).then(io));

    // One watch at a time, so two never send the same event.
    const held = yield* io(holdProcessLock(lockPath(root)));
    return held ? Result.ok() : refuse("another PR watch is running");
  });

const io = <T>(
  result: Result<T, { message: string; cause?: unknown }>,
): Result<T, StateUnwritable> => result.mapError(stateUnwritable);

// Decides from each PR's saved watch state what the watch would send, without writing any.
export const dryRun = (
  deps: { paths: WatchPaths; github: GitHub },
  urls: readonly string[],
) =>
  Result.gen(async function* () {
    const config = yield* Result.await(loadConfig(deps.paths.config));
    const refs = yield* Result.await(dryRunTargets(deps.github, config, urls));
    const links = yield* Result.await(listLinks(deps.paths.state));

    const reports: Fields[] = [];
    for (const ref of refs) {
      const account = yield* accountFor(config, ref.repo);
      const link = links.find((each) => samePr(each, ref));
      const state =
        link === undefined
          ? freshState()
          : yield* Result.await(loadWatchState(deps.paths.state, link));
      const detail = yield* Result.await(
        deps.github.detail(account, ref, config.reviewBots),
      );
      reports.push(
        reportOf(detail, state, link?.linkedAt ?? EPOCH, account, config),
      );
    }

    return Result.ok<Fields>({ dryRun: true, prs: reports });
  });

const dryRunTargets = (
  github: GitHub,
  config: WatchConfig,
  urls: readonly string[],
) =>
  Result.gen(async function* () {
    const refs: PrRef[] = [];
    for (const url of urls) {
      const ref = githubPr(url);
      if (ref === null) return refuse("--pr must be a GitHub pull request URL");
      refs.push(ref);
    }

    if (urls.length > 0) return Result.ok(refs);
    // Without --pr, the open PRs each account wrote in the configured repositories.

    for (const [repo, { account }] of Object.entries(config.repositories))
      refs.push(...(yield* Result.await(github.openPulls(account, repo))));
    return Result.ok(refs);
  });

const accountFor = (config: WatchConfig, repo: string) => {
  const account = config.repositories[repo.toLowerCase()]?.account;
  return account === undefined
    ? refuse(`${repo} is not in the repositories of the watch config`)
    : Result.ok(account);
};

// A PR no watch has linked has no state, so it is shown as read for the first time and its link time is never used.
const EPOCH = new Date(0).toISOString();

const reportOf = (
  detail: PrDetail,
  state: WatchState,
  linkedAt: string,
  account: string,
  config: WatchConfig,
) => {
  const verdicts = classify(
    detail.comments,
    account,
    config.ignore,
    config.reviewBots,
  );
  const { events } = nextEvents(detail, state, linkedAt, account, config);

  return {
    pr: detail.url,
    title: detail.title,
    draft: detail.isDraft,
    firstRead: state.summary === null,
    head: detail.head,
    bots: detail.bots,
    mergeable: detail.mergeable,
    failedRequired: detail.failedRequired,
    notified: verdicts.flatMap((verdict) =>
      "finding" in verdict ? [verdict.finding] : [],
    ),
    ignored: verdicts.flatMap(ignoredOf),
    prompt:
      detail.isDraft || events.length === 0
        ? null
        : messageOf(detail.url, events, config.instructions),
  };
};

const ignoredOf = (verdict: Verdict) =>
  "ignored" in verdict
    ? [
        {
          id: verdict.comment.id,
          author: verdict.comment.author,
          kind: verdict.comment.kind,
          reason: verdict.ignored,
          url: verdict.comment.url,
        },
      ]
    : [];

// Lets a halted PR's watch go on, taking its halted message as delivered.
export const release = (root: string, url: string) =>
  Result.gen(async function* () {
    const ref = githubPr(url);
    if (ref === null)
      return refuse("--release must be a GitHub pull request URL");

    const links = yield* Result.await(listLinks(root));
    const link = links.find((each) => samePr(each, ref));
    if (link === undefined)
      return refuse(`${ref.url} is not linked to a worker`);

    const state = yield* Result.await(loadWatchState(root, link));
    if (state.status !== "halted") return refuse(`${ref.url} is not halted`);
    // The owner checked the app, so the halted message must never be sent again.

    yield* Result.await(
      saveWatchState(root, link, {
        ...state,
        status: "watching",
        reason: null,
        pending: [],
        followUp: [],
        delivered: null,
        halted: null,
      }),
    );
    return Result.ok<Fields>({ released: ref.url, worker: link.worker });
  });

// What each linked PR's watch is doing, read from the records alone whether or not a watch runs.
export const status = (root: string) =>
  Result.gen(async function* () {
    const prs: Fields[] = [];
    for (const link of yield* Result.await(listLinks(root))) {
      // An unreadable record is shown as such, as the watch logs and skips it.
      const loaded = await loadWatchState(root, link);
      if (loaded.isErr()) {
        prs.push({
          pr: link.url,
          worker: link.worker,
          status: "unreadable",
          reason: loaded.error.message,
        });
        continue;
      }

      const state = loaded.value;
      prs.push({
        pr: link.url,
        worker: link.worker,
        status: state.status,
        reason: state.reason,
        pending: state.pending.length,
        followUp: state.followUp.length,
        awaitingAnswer: state.delivered !== null,
        // The running watch has yet to hand the PR to the chat it is now linked to.
        handingOver: state.worker !== null && state.worker !== link.worker,
        checkedAt: state.checkedAt,
      });
    }

    // An unreadable sender record is shown as such, like an unreadable PR record.
    const sender = await loadSender(root);
    return Result.ok<Fields>(
      sender.isOk()
        ? { sender: sender.value, prs }
        : { sender: null, senderError: sender.error.message, prs },
    );
  });
