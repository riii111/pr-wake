import { classify } from "./classify.ts";
import type { WatchConfig } from "./config.ts";
import type { PrDetail } from "./github.ts";
import type { ReviewBot } from "./reviewers.ts";
import { botKey, type WatchEvent, type WatchState } from "./state.ts";

export const nextEvents = (
  detail: PrDetail,
  state: WatchState,
  linkedAt: string,
  owner: string,
  config: WatchConfig,
) =>
  // The first read of a PR takes what is already on it as seen, so starting the watch never sends what the owner has handled.
  state.summary === null
    ? { events: [] as WatchEvent[], ...baselineOf(detail, config.reviewBots) }
    : eventsOf(detail, state, linkedAt, owner, config);

const baselineOf = (detail: PrDetail, bots: readonly ReviewBot[]) => ({
  seen: [
    // Every comment is taken, findings or not, so one whose verdict changes later is still not new.
    ...detail.comments.map((comment) => comment.id),
    ...bots.flatMap((bot) => botBaseline(detail, bot)),
    ...detail.failedRequired.map((check) => checkKey(detail.head, check.name)),
  ],
  conflictHead: detail.mergeable === "CONFLICTING" ? detail.head : null,
});

const eventsOf = (
  detail: PrDetail,
  state: WatchState,
  linkedAt: string,
  // The owner's own account is fixed per repository, so its comments are known without another request.
  owner: string,
  config: WatchConfig,
) => {
  // Events are told apart from what was already queued or sent by their keys, so a later read never repeats one.
  const seen = new Set(state.seen);
  const after = Date.parse(linkedAt);
  const fresh = (time: string) => Date.parse(time) >= after;

  const findings = newFindings(detail, seen, fresh, owner, config);
  const done = config.reviewBots.flatMap((bot) =>
    reviewDone(detail, bot, seen, fresh, findings),
  );
  const conflict = conflictOf(detail, state.conflictHead);
  const failed = failedChecks(detail, seen);

  return {
    events: [...done, ...findings, ...conflict.events, ...failed],
    seen: [...seen],
    conflictHead: conflict.head,
  };
};

const newFindings = (
  detail: PrDetail,
  seen: Set<string>,
  fresh: (time: string) => boolean,
  owner: string,
  config: WatchConfig,
) => {
  // A bot's findings wait for its review to complete, so they arrive with it in one message.
  const reviewing = new Set(
    Object.entries(detail.bots).flatMap(([id, status]) =>
      status?.state === "PENDING" ? [id] : [],
    ),
  );
  const events: WatchEvent[] = [];
  for (const verdict of classify(
    detail.comments,
    owner,
    config.ignore,
    config.reviewBots,
  )) {
    if (!("finding" in verdict)) continue;

    const { finding } = verdict;
    if (finding.bot !== null && reviewing.has(finding.bot)) continue;
    if (!fresh(finding.createdAt) || seen.has(finding.id)) continue;

    seen.add(finding.id);
    events.push({ type: "finding", finding });
  }

  return events;
};

const reviewDone = (
  detail: PrDetail,
  bot: ReviewBot,
  seen: Set<string>,
  fresh: (time: string) => boolean,
  findings: readonly WatchEvent[],
): WatchEvent[] => {
  // A bot added after the PR's first read starts from its reviews as they stand, as the first read does.
  if (!seen.has(botKey(bot.id))) {
    for (const key of botBaseline(detail, bot)) seen.add(key);
    return [];
  }

  const key = reviewKey(bot, detail.head);
  const status = detail.bots[bot.id];
  if (
    status == null ||
    !reviewCompleted(detail, bot) ||
    !fresh(status.createdAt) ||
    seen.has(key)
  )
    return [];

  seen.add(key);
  // Whether the review found anything is decided by its new inline findings; the summary comment only confirms it.
  const inline = findings.filter(
    (event) =>
      event.type === "finding" &&
      event.finding.bot === bot.id &&
      event.finding.kind === "inline",
  ).length;
  // A bot without a summary to check against always agrees.
  const summaryAgrees =
    bot.noActionable === null ||
    (inline === 0) === detail.noActionable.includes(bot.id);
  return [
    {
      type: "review_done",
      bot: bot.id,
      name: bot.name,
      head: detail.head,
      findings: inline,
      summaryAgrees,
    },
  ];
};

const reviewCompleted = (detail: PrDetail, bot: ReviewBot) => {
  const status = detail.bots[bot.id];
  if (bot.status === null || status == null || status.state !== "SUCCESS")
    return false;
  // A check run has no description; its success alone says the review ended.
  return (
    !("completed" in bot.status) ||
    status.description.includes(bot.status.completed)
  );
};

const botBaseline = (detail: PrDetail, bot: ReviewBot) => [
  botKey(bot.id),
  ...(reviewCompleted(detail, bot) ? [reviewKey(bot, detail.head)] : []),
];

const reviewKey = (bot: ReviewBot, head: string) =>
  // The id leads, so the keys CodeRabbit's reviews were recorded under before bots were configurable still match.
  `${bot.id}:${head}`;

const conflictOf = (detail: PrDetail, notified: string | null) => {
  // Told once per head while the PR conflicts, so a base that keeps moving does not repeat it.
  if (detail.mergeable === "CONFLICTING" && notified !== detail.head)
    return {
      events: [{ type: "conflict", head: detail.head } satisfies WatchEvent],
      head: detail.head,
    };
  return {
    events: [],
    head: detail.mergeable === "MERGEABLE" ? null : notified,
  };
};

const failedChecks = (detail: PrDetail, seen: Set<string>) =>
  detail.failedRequired.flatMap((check): WatchEvent[] => {
    const key = checkKey(detail.head, check.name);
    if (seen.has(key)) return [];

    seen.add(key);
    return [{ type: "ci_failed", head: detail.head, check }];
  });

const checkKey = (head: string, name: string) => `ci:${head}:${name}`;
