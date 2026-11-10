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

export class InputCommandFailed extends TaggedError("InputCommandFailed")<{
  command: string;
  // Whether the command ran, so a caller can tell a command that may have acted from one that never started.
  started: boolean;
  message: string;
}> {}

// Runs a command with its input on stdin, so a message never shows in the process list or passes for an option.
export const runWithInput = (
  file: string,
  args: readonly string[],
  input: string,
  env: Record<string, string | undefined>,
  timeoutMs: number,
) => {
  let started = false;
  return Result.tryPromise({
    try: async (): Promise<CommandOutput> => {
      const child = Bun.spawn([file, ...args], {
        env,
        stdin: new Blob([input]),
        stdout: "pipe",
        stderr: "pipe",
        timeout: timeoutMs,
      });
      started = true;
      const [stdout, stderr, status] = await Promise.all([
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
        child.exited,
      ]);
      if (child.signalCode !== null)
        throw new Error(`${file} was stopped by ${child.signalCode}`);
      return { status, stdout, stderr };
    },
    catch: (cause) =>
      new InputCommandFailed({
        command: file,
        started,
        message:
          cause instanceof Error && started
            ? cause.message
            : `cannot run ${file}`,
      }),
  });
};
