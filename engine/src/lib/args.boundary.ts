import { parseArgs } from "node:util";

import { Result, TaggedError } from "better-result";

class ArgumentsInvalid extends TaggedError("ArgumentsInvalid")<{
  message: string;
}> {}

export type OptionTypes = Record<
  string,
  { type: "string" | "boolean"; short?: string; multiple?: boolean }
>;

export const parseOptions = (args: readonly string[], options: OptionTypes) =>
  Result.try({
    try: (): Record<string, unknown> =>
      // Strict, so an unknown option or a stray argument stops the run instead of being ignored.
      parseArgs({ args: [...args], options, strict: true }).values,
    catch: (cause) =>
      new ArgumentsInvalid({
        message: cause instanceof Error ? cause.message : String(cause),
      }),
  });
