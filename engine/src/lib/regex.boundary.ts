import { Result, TaggedError } from "better-result";

class RegexInvalid extends TaggedError("RegexInvalid")<{
  cause: unknown;
  message: string;
}> {}

export const compileRegex = (source: string) =>
  Result.try({
    try: () => new RegExp(source),
    catch: (cause) =>
      new RegexInvalid({ cause, message: "not a regular expression" }),
  });
