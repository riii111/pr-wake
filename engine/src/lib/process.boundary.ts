import { type ExecFileException, execFile } from "node:child_process";

import { Result, TaggedError } from "better-result";

import { isObject } from "./object.ts";

export class CommandFailed extends TaggedError("CommandFailed")<{
  command: string;
  cause: unknown;
  message: string;
}> {}

export type CommandOutput = {
  status: number;
  stdout: string;
  stderr: string;
};

export const runCommand = (
  file: string,
  args: readonly string[],
  env: Record<string, string | undefined>,
  timeoutMs = COMMAND_TIMEOUT_MS,
) =>
  Result.tryPromise({
    try: async (): Promise<CommandOutput> => {
      const { error, stdout, stderr } = await captureCommand(file, args, {
        env,
        timeoutMs,
      });
      if (error === null) return { status: 0, stdout, stderr };
      // A command that ran is ok whatever its exit status, so the caller decides what a failure means; one that could not start or hung past the timeout is an error.
      if (typeof error.code === "number" && !error.killed)
        return { status: error.code, stdout, stderr };
      throw error;
    },
    catch: (cause) =>
      new CommandFailed({
        command: file,
        cause,
        message:
          isObject(cause) && cause.killed === true
            ? `${file} was stopped after ${timeoutMs} ms`
            : `cannot run ${file}`,
      }),
  });

// The one place a command's end is awaited, answering a failed command with its error rather than failing.
export const captureCommand = (
  file: string,
  args: readonly string[],
  options: { env?: Record<string, string | undefined>; timeoutMs: number },
) =>
  new Promise<{
    error: ExecFileException | null;
    stdout: string;
    stderr: string;
  }>((resolve) => {
    execFile(
      file,
      [...args],
      {
        env: options.env,
        timeout: options.timeoutMs,
        maxBuffer: COMMAND_OUTPUT_LIMIT,
      },
      (error, stdout, stderr) => resolve({ error, stdout, stderr }),
    );
  });

const COMMAND_TIMEOUT_MS = 10_000;

const COMMAND_OUTPUT_LIMIT = 16 * 1024 * 1024;
