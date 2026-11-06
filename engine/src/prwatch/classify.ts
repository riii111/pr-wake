import type { IgnoreRule } from "./config.ts";
import type { PrComment } from "./github.ts";
import { botOf, type ReviewBot, severityOf } from "./reviewers.ts";
import type { Finding } from "./state.ts";

export type Verdict =
  | { comment: PrComment; finding: Finding }
  | { comment: PrComment; ignored: string };

export const classify = (
  comments: readonly PrComment[],
  owner: string,
  rules: readonly IgnoreRule[],
  bots: readonly ReviewBot[],
): Verdict[] =>
  comments.map((comment) => {
    const bot = botOf(bots, comment.author);
    const reason = ignoredReason(comment, owner, rules, bot);
    return reason === null
      ? { comment, finding: findingOf(comment, bot) }
      : { comment, ignored: reason };
  });

const ignoredReason = (
  comment: PrComment,
  owner: string,
  rules: readonly IgnoreRule[],
  bot: ReviewBot | null,
) => {
  if (comment.author.toLowerCase() === owner.toLowerCase()) return "owner";
  if (rules.some((rule) => matches(rule, comment))) return "ignore rule";
  // Findings are left on the code or in a review; the conversation carries requests and replies such as "will look".
  if (comment.kind === "issue_comment") return "issue comment";

  if (bot !== null) {
    // A bot's replies answer the owner in a thread already reported when it opened.
    if (comment.kind === "inline" && comment.reply) return `${bot.name} reply`;
    if (
      comment.kind === "review" &&
      bot.actionableReview !== null &&
      !comment.body.includes(bot.actionableReview)
    )
      return `${bot.name} review without actionable comments`;
    // A bot's finding resolved before the watch read it was already handled; a person's reply there still reopens the talk.
    if (comment.kind === "inline" && comment.resolved)
      return `resolved ${bot.name} thread`;
  }

  if (
    comment.kind === "review" &&
    !isApproval(comment, bot) &&
    comment.body.trim() === ""
  )
    return "empty review";
  return null;
};

const matches = (rule: IgnoreRule, comment: PrComment) =>
  rule.author.toLowerCase() === comment.author.toLowerCase() &&
  (rule.kind === undefined || rule.kind === comment.kind) &&
  (rule.bodyIncludes === undefined || comment.body.includes(rule.bodyIncludes));

const findingOf = (comment: PrComment, bot: ReviewBot | null): Finding => ({
  // Only the comment's kind, author and place are kept, so a body can never steer what the worker is told.
  id: comment.id,
  kind: comment.kind,
  bot: bot?.id ?? null,
  label: labelOf(comment, bot),
  path: comment.path,
  line: comment.line,
  url: comment.url,
  createdAt: comment.createdAt,
  approval: approvalOf(comment, bot),
});

const isApproval = (comment: PrComment, bot: ReviewBot | null) =>
  // A review bot's approval is read like its other reviews.
  bot === null && comment.kind === "review" && comment.approved;

const approvalOf = (
  comment: PrComment,
  bot: ReviewBot | null,
): Finding["approval"] => {
  if (!isApproval(comment, bot)) return null;
  return comment.body.trim() === "" ? "without_body" : "with_body";
};

const labelOf = (comment: PrComment, bot: ReviewBot | null) => {
  if (isApproval(comment, bot)) return `${comment.author}・Approve`;
  if (bot === null) return comment.author;
  if (comment.kind === "review") return `${bot.name}・レビュー`;
  const severity = severityOf(bot, comment.body);
  return severity === undefined ? bot.name : `${bot.name}・${severity}`;
};
