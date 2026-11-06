import { Result, TaggedError } from "better-result";

import { isObject } from "../lib/object.ts";

// A failure's message is shown to whoever ran the command and may carry a path or what a command printed, so the log records the tag alone.
export class GithubAuthFailed extends TaggedError("GithubAuthFailed")<{
  cause?: unknown;
  message: string;
}> {}

export class GithubUnreachable extends TaggedError("GithubUnreachable")<{
  cause: unknown;
  message: string;
}> {}

export class GithubRejected extends TaggedError("GithubRejected")<{
  message: string;
}> {}

export class StateUnreadable extends TaggedError("StateUnreadable")<{
  cause?: unknown;
  message: string;
}> {}

export class StateUnwritable extends TaggedError("StateUnwritable")<{
  cause: unknown;
  message: string;
}> {}

export class ConfigUnreadable extends TaggedError("ConfigUnreadable")<{
  cause: unknown;
  message: string;
}> {}

export class ConfigInvalid extends TaggedError("ConfigInvalid")<{
  message: string;
}> {}

export class WatchRefused extends TaggedError("WatchRefused")<{
  message: string;
}> {}

export class WatchReplaced extends TaggedError("WatchReplaced")<{
  message: string;
}> {}

export type WatchFailure =
  | GithubAuthFailed
  | GithubUnreachable
  | GithubRejected
  | StateUnreadable
  | StateUnwritable
  | ConfigUnreadable
  | ConfigInvalid
  | WatchRefused
  | WatchReplaced;

type IoError = { message: string; cause?: unknown };

export const refuse = (message: string) =>
  Result.err(new WatchRefused({ message }));

export const stateUnreadable = (error: IoError) =>
  new StateUnreadable({ cause: error, message: ioMessage(error) });

export const stateUnwritable = (error: IoError) =>
  new StateUnwritable({ cause: error, message: ioMessage(error) });

export const configUnreadable = (error: IoError) =>
  new ConfigUnreadable({ cause: error, message: ioMessage(error) });

const ioMessage = (error: IoError) => {
  const { cause } = error;
  const code =
    isObject(cause) && typeof cause.code === "string" ? cause.code : null;
  return code === null ? error.message : `${error.message} (${code})`;
};

export const jsonEqual = (a: unknown, b: unknown) => Bun.deepEquals(a, b, true);
