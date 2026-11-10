import { Result, TaggedError } from "better-result";

import { type AppCall, callApp } from "./app/call.ts";
import { appWorkers } from "./app/app-workers.ts";
import { checkV1, type ProtocolCheck } from "./app/protocol.ts";
import {
  isT3Worker,
  T3_SENDER,
  T3_WORKER,
  t3Binary,
  type T3Run,
  t3Workers,
} from "./app/t3-worker.ts";
import { programWatch } from "./app/watch-program.ts";
import { codexWorkers, senderFor } from "./app/watch-worker.ts";
import { type OptionTypes, parseOptions } from "./lib/args.boundary.ts";
import type { Fields } from "./lib/object.ts";
import { runCommand, runWithInput } from "./lib/process.boundary.ts";
import { type Env, homeOf, watchPaths } from "./paths.ts";
import { dryRun, release, status, watchForever } from "./prwatch/commands.ts";
import { createGitHub, type GitHub } from "./prwatch/github.ts";
import {
  githubPr,
  linkedChat,
  loadLink,
  loadSender,
  recordLink,
  recordSender,
} from "./prwatch/store.ts";

export const NAME = "pr-wake-engine";

export type EngineDeps = {
  env: Env;
  github: GitHub;
  callApp: AppCall;
  checkProtocol: ProtocolCheck;
  runT3: T3Run;
  // Only the watch writes as it runs, since it never returns.
  print: (line: string) => void;
  sleep: (ms: number) => Promise<unknown>;
  // The running program, which the watch leaves for a newer one installed at the same path.
  program: { path: string; compiled: boolean };
};

export const engineDeps = (env: Env): EngineDeps => ({
  env,
  github: createGitHub(
    (argv, runEnv) => runCommand(argv[0] ?? "gh", argv.slice(1), runEnv),
    env,
  ),
  callApp: (caller, tool, args) => callApp(env, caller, tool, args),
  checkProtocol: checkV1(env),
  runT3: async (args, input, timeoutMs) => {
    const bin = t3Binary(env, homeOf(env), (name) =>
      Bun.which(name, { PATH: env.PATH ?? "" }),
    );
    if (bin === null)
      return {
        ran: false,
        started: false,
        message: "HARNEXUS_T3_BIN must be an absolute path",
      };
    const ran = await runWithInput(bin, args, input, env, timeoutMs);
    return ran.isOk()
      ? { ran: true, ...ran.value }
      : { ran: false, started: ran.error.started, message: ran.error.message };
  },
  print: (line) => {
    process.stdout.write(line);
  },
  sleep: (ms) => Bun.sleep(ms),
  // Only a compiled engine is its own program; run from source, the program is Bun.
  program: {
    path: process.execPath,
    compiled: Bun.main.startsWith("/$bunfs/"),
  },
});

type WatchArgs = {
  dryRun: boolean;
  prs: string[];
  release: string | undefined;
  status: boolean;
  sender: boolean;
  link: string | undefined;
  takeOver: boolean;
};

class EngineFailed extends TaggedError("EngineFailed")<{ message: string }> {}

const fail = (message: string) => Result.err(new EngineFailed({ message }));

const watch = async (args: WatchArgs, deps: EngineDeps) => {
  const refused = refuseInCloud(deps.env);
  if (refused.isErr()) return Result.err(refused.error);
  const paths = watchPaths(deps.env);
  if (args.release !== undefined) return release(paths.state, args.release);
  if (args.status) return status(paths.state);
  if (args.sender) return registerSender(deps, paths.state);
  if (args.link !== undefined)
    return linkCaller(deps, args.link, args.takeOver);
  const { github } = deps;
  if (args.dryRun) return dryRun({ paths, github }, args.prs);
  const log = (entry: Fields) =>
    deps.print(
      `${JSON.stringify({ time: new Date().toISOString(), ...entry })}\n`,
    );
  const replaced = await programWatch(
    deps.program.path,
    deps.program.compiled,
  );
  if (replaced === null) log({ event: "replace_check_off" });
  // Said once at start, since without a registered chat no PR is ever sent to.
  const registered = await loadSender(paths.state);
  if (registered.isErr() || registered.value === null)
    log({
      event: "no_sender_registered",
      ...(registered.isErr() ? { error: registered.error.message } : {}),
    });
  // Checked before the watch can send, so a harnexus without the expected protocol is never written to.
  const supported = await deps.checkProtocol();
  if (supported.isErr()) return Result.err(supported.error);
  // A bridge replaced by an older one while the watch runs ends it between cycles, as at start; a closed app only waits.
  let unsupported: string | null = null;
  const callApp: AppCall = async (caller, tool, sent) => {
    const answer = await deps.callApp(caller, tool, sent);
    if (answer.outcome === "unsupported" && unsupported === null) {
      unsupported = String(answer.message ?? "unsupported");
      log({ event: "protocol_unsupported", message: unsupported });
    }
    return answer;
  };
  return watchForever(
    {
      paths,
      github,
      workers: appWorkers(
        codexWorkers(callApp, () => loadSender(paths.state)),
        t3Workers(deps.runT3),
      ),
      now: () => new Date(),
      log,
    },
    deps.sleep,
    replaced ?? undefined,
    () =>
      unsupported === null
        ? null
        : `cannot use the harnexus call socket: ${unsupported}`,
  );
};

// The calling chat becomes the one the watch sends every message from.
const registerSender = (deps: EngineDeps, root: string) =>
  Result.gen(async function* () {
    const thread = yield* callerId(deps.env);
    if (isT3Worker(thread))
      return fail(
        "watch --sender is for a Codex App chat; a T3 Code worker is sent to through the t3 CLI, without a sender",
      );
    yield* Result.await(recordSender(root, thread));
    return Result.ok<Fields>({ sender: thread });
  });

const NOT_LINKABLE =
  "the PR watch needs the URL https://github.com/<owner>/<repo>/pull/<number>";

// Links the calling chat to a PR; only a take-over moves a PR another chat is watched for.
const linkCaller = (deps: EngineDeps, value: string, takeOver: boolean) =>
  Result.gen(async function* () {
    const worker = yield* callerId(deps.env);
    const pr = githubPr(value);
    if (pr === null) return fail(`${value} is not watched: ${NOT_LINKABLE}`);
    const root = watchPaths(deps.env).state;
    if (!takeOver) {
      const current = yield* Result.await(linkedChat(root, pr));
      if (current?.active && current.worker !== worker)
        return fail(
          `${pr.url} is already watched for chat ${current.worker}; link it with --take-over from the chat that takes it over`,
        );
    }
    // Only the link is read, so an unreadable state never stops a chat from linking its PR.
    const current = yield* Result.await(loadLink(root, pr));
    yield* Result.await(
      recordLink(root, {
        ...pr,
        worker,
        // Linking the same PR again keeps its start, so comments made since are still reported.
        linkedAt:
          current?.worker === worker
            ? current.linkedAt
            : new Date().toISOString(),
      }),
    );
    // Warns about a linked chat nothing can send to, which the link alone would show as watched.
    const sender = isT3Worker(worker)
      ? T3_SENDER
      : senderFor(await loadSender(root), worker);
    return Result.ok<Fields>({
      threadId: worker,
      watched: pr.url,
      ...(typeof sender === "string" ? {} : { warning: sender.missing }),
    });
  });

const callerId = (env: Env) => {
  // T3 Code sets its thread for every agent and job it starts, and runs Codex there with a CODEX_THREAD_ID of Codex's own, so its variable decides first.
  const t3 = env[T3_THREAD_ENV];
  const variable =
    t3 !== undefined && t3 !== "" ? T3_THREAD_ENV : "CODEX_THREAD_ID";
  const value = env[variable];
  if (value === undefined || value === "")
    return fail(`CODEX_THREAD_ID is not set; run ${NAME} from a thread`);
  // A chat ID is pasted into socket requests and t3 arguments, so it may carry no whitespace or control characters.
  if (!/^[^\s\p{Cc}]+$/u.test(value) || value.startsWith("client-new-thread:"))
    return fail(`${variable} must be a confirmed chat ID without whitespace`);
  return Result.ok(variable === T3_THREAD_ENV ? `${T3_WORKER}${value}` : value);
};

const T3_THREAD_ENV = "T3CODE_THREAD_ID";

const EXECUTION_ENV = "HARNEXUS_EXECUTION_ENV";

// The watch needs the Codex App on the same Mac, so a Cloud session is refused with the reason rather than a missing-socket hint.
const refuseInCloud = (env: Env) => {
  const explicit = env[EXECUTION_ENV];
  if (explicit !== undefined && explicit !== "") {
    if (explicit !== "cloud" && explicit !== "local")
      return fail(`${EXECUTION_ENV} must be cloud or local, not "${explicit}"`);
    return explicit === "cloud" ? cloudRefusal() : Result.ok();
  }
  const cloud =
    (env.CODEX_ENVIRONMENT_ID ?? "") !== "" || env.CLAUDE_CODE_REMOTE === "true";
  return cloud ? cloudRefusal() : Result.ok();
};

const cloudRefusal = () =>
  fail(
    "watch needs the Codex App and is unavailable in Cloud; follow a PR from a Cloud session with that Cloud's own PR notifications",
  );

const OPTIONS = {
  "dry-run": { type: "boolean" },
  pr: { type: "string", multiple: true },
  release: { type: "string" },
  status: { type: "boolean" },
  sender: { type: "boolean" },
  link: { type: "string" },
  "take-over": { type: "boolean" },
  help: { type: "boolean", short: "h" },
} satisfies OptionTypes;

const USAGE = `usage:\n  ${NAME} watch [--dry-run [--pr PR]... | --release PR | --status | --sender | --link PR [--take-over]]\n`;

class UsageInvalid extends TaggedError("UsageInvalid")<{ message: string }> {}

const invalid = (message: string) => Result.err(new UsageInvalid({ message }));

const watchArgs = (values: Record<string, unknown>) => {
  const text = (key: string) =>
    typeof values[key] === "string" ? values[key] : undefined;
  const args: WatchArgs = {
    dryRun: values["dry-run"] === true,
    prs: Array.isArray(values.pr)
      ? values.pr.filter((pr) => typeof pr === "string")
      : [],
    release: text("release"),
    status: values.status === true,
    sender: values.sender === true,
    link: text("link"),
    takeOver: values["take-over"] === true,
  };
  if (args.prs.length > 0 && !args.dryRun)
    return invalid("--pr is only for --dry-run");
  if (args.takeOver && args.link === undefined)
    return invalid("--take-over is only for --link");
  const modes = [
    args.release !== undefined,
    args.dryRun,
    args.status,
    args.sender,
    args.link !== undefined,
  ];
  if (modes.filter(Boolean).length > 1)
    return invalid(
      "use only one of --dry-run, --release, --status, --sender and --link",
    );
  return Result.ok(args);
};

export type EngineRun = { code: number; stdout: string; stderr: string };

// Exit 0 prints one JSON line, 1 is a failure on stderr, 2 a usage error; harnexus-task watch answered alike.
export const runEngine = async (
  argv: readonly string[],
  deps: EngineDeps,
): Promise<EngineRun> => {
  const [command, ...rest] = argv;
  if (command === "-h" || command === "--help")
    return { code: 0, stdout: USAGE, stderr: "" };
  if (command !== "watch")
    return usageError("expected the subcommand watch");
  const parsed = parseOptions(rest, OPTIONS);
  if (parsed.isErr()) return usageError(parsed.error.message);
  if (parsed.value.help === true) return { code: 0, stdout: USAGE, stderr: "" };
  const args = watchArgs(parsed.value);
  if (args.isErr()) return usageError(args.error.message);
  const result = await watch(args.value, deps);
  return result.isOk()
    ? { code: 0, stdout: `${JSON.stringify(result.value)}\n`, stderr: "" }
    : { code: 1, stdout: "", stderr: `${NAME}: ${result.error.message}\n` };
};

const usageError = (message: string): EngineRun => ({
  code: 2,
  stdout: "",
  stderr: `${USAGE}${NAME}: ${message}\n`,
});
