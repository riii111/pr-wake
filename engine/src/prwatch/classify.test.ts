import { describe, expect, test } from "bun:test";

import { classify } from "./classify.ts";
import type { IgnoreRule } from "./config.ts";
import type { PrComment } from "./github.ts";
import { PRESETS, type ReviewBot } from "./reviewers.ts";

const PR = "https://github.com/org/pay/pull/7";

const RULES: IgnoreRule[] = [
  { author: "coderabbitai[bot]", kind: "issue_comment" },
  {
    author: "github-actions[bot]",
    kind: "inline",
    bodyIncludes: "[phase-timing]",
  },
  { author: "blacksmith-sh[bot]" },
];

const BOTS: ReviewBot[] = [PRESETS.coderabbit];

const comment = (fields: Partial<PrComment>): PrComment => ({
  id: "inline:1",
  kind: "inline",
  author: "reviewer",
  url: `${PR}#discussion_r1`,
  createdAt: "2026-10-23T00:00:00Z",
  body: "please rename",
  path: "src/pay.kt",
  line: 42,
  reply: false,
  resolved: false,
  approved: false,
  ...fields,
});

const verdictsOf = (fields: Partial<PrComment>) =>
  classify([comment(fields)], "owner", RULES, BOTS).map((verdict) =>
    "finding" in verdict ? "notified" : verdict.ignored,
  );

describe("classify", () => {
  test.each([
    { name: "a reviewer's inline comment", fields: {}, expected: "notified" },
    {
      name: "a reviewer's reply in a thread",
      fields: { reply: true },
      expected: "notified",
    },
    {
      name: "a reviewer's review with a body",
      fields: { kind: "review", body: "LGTM with one question" },
      expected: "notified",
    },
    {
      name: "a reviewer's issue comment",
      fields: { kind: "issue_comment" },
      expected: "issue comment",
    },
    {
      name: "a new CodeRabbit thread",
      fields: { author: "coderabbitai[bot]" },
      expected: "notified",
    },
    {
      name: "a CodeRabbit review posting actionable comments",
      fields: {
        author: "coderabbitai[bot]",
        kind: "review",
        body: "**Actionable comments posted: 2**",
      },
      expected: "notified",
    },
    {
      name: "the owner's own comment",
      fields: { author: "Owner" },
      expected: "owner",
    },
    {
      name: "CodeRabbit's walkthrough",
      fields: { author: "coderabbitai[bot]", kind: "issue_comment" },
      expected: "ignore rule",
    },
    {
      name: "a phase-timing annotation",
      fields: {
        author: "github-actions[bot]",
        body: "📝 **[phase-timing]** <hook-duration>",
      },
      expected: "ignore rule",
    },
    {
      name: "another github-actions annotation",
      fields: { author: "github-actions[bot]", body: "lint: unused import" },
      expected: "notified",
    },
    {
      name: "a Blacksmith CI notice",
      fields: { author: "blacksmith-sh[bot]", kind: "issue_comment" },
      expected: "ignore rule",
    },
    {
      name: "CodeRabbit's reply to the owner",
      fields: { author: "coderabbitai[bot]", reply: true },
      expected: "CodeRabbit reply",
    },
    {
      name: "a CodeRabbit review without actionable comments",
      fields: { author: "coderabbitai[bot]", kind: "review", body: "nitpicks" },
      expected: "CodeRabbit review without actionable comments",
    },
    {
      name: "a review without a body",
      fields: { kind: "review", body: " \n" },
      expected: "empty review",
    },
    {
      name: "a CodeRabbit approval without actionable comments",
      fields: {
        author: "coderabbitai[bot]",
        kind: "review",
        approved: true,
        body: "",
      },
      expected: "CodeRabbit review without actionable comments",
    },
    {
      name: "a CodeRabbit finding in a resolved thread",
      fields: { author: "coderabbitai[bot]", resolved: true },
      expected: "resolved CodeRabbit thread",
    },
    {
      name: "a reviewer's reply in a resolved thread",
      fields: { reply: true, resolved: true },
      expected: "notified",
    },
  ] satisfies {
    name: string;
    fields: Partial<PrComment>;
    expected: string;
  }[])("decides $name as $expected", ({ fields, expected }) => {
    expect(verdictsOf(fields)).toEqual([expected]);
  });

  test("labels CodeRabbit findings by severity and people by name", () => {
    const verdicts = classify(
      [
        comment({
          id: "inline:1",
          author: "coderabbitai[bot]",
          body: "**🔒 Security & Privacy** | **🟠 Major** | **⚡ Quick win**",
        }),
        comment({ id: "inline:2", author: "coderabbitai[bot]", body: "x" }),
        comment({
          id: "review:3",
          kind: "review",
          author: "coderabbitai[bot]",
          body: "**Actionable comments posted: 1**",
        }),
        comment({ id: "inline:4", author: "tanaka" }),
      ],
      "owner",
      RULES,
      BOTS,
    );

    expect(
      verdicts.map((verdict) =>
        "finding" in verdict ? verdict.finding.label : verdict.ignored,
      ),
    ).toEqual([
      "CodeRabbit・Major",
      "CodeRabbit",
      "CodeRabbit・レビュー",
      "tanaka",
    ]);
  });

  test("labels a person's approval and records whether it has a body", () => {
    const verdicts = classify(
      [
        comment({ id: "review:1", kind: "review", approved: true, body: "ok" }),
        comment({ id: "review:2", kind: "review", approved: true, body: " " }),
        comment({ id: "review:3", kind: "review", body: "why?" }),
        comment({
          id: "review:4",
          kind: "review",
          author: "coderabbitai[bot]",
          approved: true,
          body: "**Actionable comments posted: 1**",
        }),
      ],
      "owner",
      RULES,
      BOTS,
    );

    expect(
      verdicts.map((verdict) =>
        "finding" in verdict
          ? [verdict.finding.label, verdict.finding.approval]
          : verdict.ignored,
      ),
    ).toEqual([
      ["reviewer・Approve", "with_body"],
      ["reviewer・Approve", "without_body"],
      ["reviewer", null],
      ["CodeRabbit・レビュー", null],
    ]);
  });

  test("treats a configured bot like CodeRabbit and a bot it does not know like a person", () => {
    const other: ReviewBot = {
      id: "other",
      name: "OtherBot",
      login: "other-reviewer[bot]",
      status: null,
      actionableReview: null,
      noActionable: null,
      severity: String.raw`^\[(P[0-3])\]`,
    };

    const verdicts = classify(
      [
        comment({
          id: "inline:1",
          author: "other-reviewer[bot]",
          body: "[P1] x",
        }),
        comment({
          id: "inline:2",
          author: "other-reviewer[bot]",
          reply: true,
        }),
        comment({ id: "inline:3", author: "coderabbitai[bot]", reply: true }),
      ],
      "owner",
      RULES,
      [other],
    );

    expect(
      verdicts.map((verdict) =>
        "finding" in verdict
          ? [verdict.finding.label, verdict.finding.bot]
          : verdict.ignored,
      ),
    ).toEqual([
      ["OtherBot・P1", "other"],
      "OtherBot reply",
      ["coderabbitai[bot]", null],
    ]);
  });
});
