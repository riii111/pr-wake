// A review bot is described by data alone, so moving to another AI reviewer changes the config, not the watch.
export type ReviewBot = {
  // The key the watch records the bot's reviews under; keep it when other fields change.
  id: string;
  // Shown to the worker.
  name: string;
  login: string;
  // What the bot keeps on a head while it reviews it: a commit status, or the check run of the workflow that runs it.
  // Without one, its findings are sent as they come and no completion is told.
  status: BotProgress | null;
  // A review body without this text carries no findings of its own; null takes every review body.
  actionableReview: string | null;
  // A PR comment with this text says the bot found nothing, which the watch checks against the findings it saw.
  noActionable: string | null;
  // Read from an inline finding; the first group is the severity shown to the worker.
  severity: string | null;
};

type BotProgress =
  | { context: string; completed: string }
  | { checkRun: string };

export const PRESETS = {
  coderabbit: {
    id: "coderabbit",
    name: "CodeRabbit",
    login: "coderabbitai[bot]",
    status: { context: "CodeRabbit", completed: "Review completed" },
    actionableReview: "Actionable comments posted",
    noActionable: "No actionable comments were generated",
    // CodeRabbit opens a finding with a bold category and severity, such as **🟠 Major**.
    severity: String.raw`\*\*[^*\n]*?\b(Critical|Major|Minor|Trivial|Info)\*\*`,
  },
  // Claude posts its review with no body and a free-form summary comment, so only its inline findings are read.
  claude: {
    id: "claude",
    name: "Claude",
    login: "claude[bot]",
    status: { checkRun: "Claude AI Review" },
    actionableReview: null,
    noActionable: null,
    // The review prompt has Claude open a finding with its severity in brackets, such as [warning].
    severity: String.raw`^\[(critical|warning|suggestion)\]`,
  },
} as const satisfies Record<string, ReviewBot>;

export const botOf = (bots: readonly ReviewBot[], login: string) =>
  bots.find((bot) => bot.login.toLowerCase() === login.toLowerCase()) ?? null;

export const severityOf = (bot: ReviewBot, body: string) =>
  bot.severity === null ? undefined : compiled(bot.severity).exec(body)?.[1];

const compiled = (source: string) => {
  const kept = PATTERNS.get(source);
  if (kept !== undefined) return kept;

  const pattern = new RegExp(source);
  PATTERNS.set(source, pattern);
  return pattern;
};

// Each pattern is compiled once, since every inline comment of every read is matched against it.
const PATTERNS = new Map<string, RegExp>();
