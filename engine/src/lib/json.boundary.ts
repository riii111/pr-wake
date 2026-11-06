import { Result, TaggedError } from "better-result";

class JsonParseFailed extends TaggedError("JsonParseFailed")<{
  cause: unknown;
  message: string;
}> {}

export const parseJson = (text: string) =>
  Result.try({
    try: (): unknown => JSON.parse(text),
    catch: (cause) => new JsonParseFailed({ cause, message: "invalid JSON" }),
  });
