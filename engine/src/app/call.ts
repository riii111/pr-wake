import { dirname, isAbsolute, join } from "node:path";

import { parseJson } from "../lib/json.boundary.ts";
import { type Fields, isObject } from "../lib/object.ts";
import { requestLine } from "../lib/socket.boundary.ts";
import { type Env, homeOf } from "../paths.ts";

// A request to harnexus's public call socket: one JSON line out, one JSON line back.
export type AppCall = (
  caller: string,
  tool: string,
  args: Fields,
) => Promise<Fields>;

export const PROTOCOL_VERSION = 1;

// What the bridge offers, or why it cannot be used: not_sent when it could not be asked, unsupported when it answered without protocol 1.
export type Capabilities =
  | { outcome: "done"; operations: readonly string[] }
  | { outcome: "not_sent" | "unsupported"; message: string };

export const queryCapabilities = async (
  env: Env,
  timeoutMs = QUERY_TIMEOUT_MS,
): Promise<Capabilities> => {
  const path = callSocketPath(env);
  if (typeof path !== "string")
    return { outcome: "not_sent", message: `${path.error}; ${SOCKET_HINT}` };
  const answer = await requestLine(
    path,
    JSON.stringify({ version: PROTOCOL_VERSION, query: "capabilities" }),
    timeoutMs,
  );
  // The query changes nothing, so any failure of it leaves the call unsent.
  if (answer.isErr())
    return {
      outcome: "not_sent",
      message: `${path}: ${answer.error.code ?? answer.error.message}; ${socketHint(answer.error.code)}`,
    };
  const offered = parseJson(answer.value).unwrapOr(null);
  // An older bridge answers the query as an invalid call; it is never sent the versionless form instead.
  if (
    !isObject(offered) ||
    offered.outcome !== "done" ||
    offered.version !== PROTOCOL_VERSION ||
    !Array.isArray(offered.operations)
  )
    return {
      outcome: "unsupported",
      message: `the harnexus bridge does not offer call protocol ${PROTOCOL_VERSION}; update harnexus and restart the app`,
    };
  return {
    outcome: "done",
    operations: offered.operations.filter((op) => typeof op === "string"),
  };
};

// Sends a call only after the bridge said it offers protocol 1 and the tool, so an older bridge never receives it.
export const callApp = async (
  env: Env,
  caller: string,
  tool: string,
  args: Fields,
  timeoutMs = CALL_TIMEOUT_MS,
): Promise<Fields> => {
  const offered = await queryCapabilities(env);
  if (offered.outcome !== "done") return offered;
  if (!offered.operations.includes(tool))
    return {
      outcome: "unsupported",
      message: `the harnexus bridge does not offer ${tool}`,
    };

  const path = callSocketPath(env);
  if (typeof path !== "string")
    return { outcome: "not_sent", message: path.error };
  const line = JSON.stringify({
    version: PROTOCOL_VERSION,
    threadId: caller,
    tool,
    arguments: args,
  });
  const answer = await requestLine(path, line, timeoutMs);
  if (answer.isErr()) {
    const reason = answer.error.code ?? answer.error.message;
    // The request may already have reached the App once connected.
    return answer.error.connected
      ? { outcome: "unknown", message: `no answer: ${reason}` }
      : {
          outcome: "not_sent",
          message: `${path}: ${reason}; ${socketHint(answer.error.code)}`,
        };
  }

  const parsed = parseJson(answer.value);
  if (parsed.isErr())
    return { outcome: "unknown", message: "no answer: invalid JSON" };
  if (!isObject(parsed.value) || typeof parsed.value.outcome !== "string")
    return { outcome: "unknown", message: "no answer: answer without outcome" };
  return parsed.value;
};

const CALL_SOCKET_ENV = "HARNEXUS_CALL_SOCKET";
const STATE_PATH_ENV = "HARNEXUS_STATE_PATH";

// Where harnexus serves the call socket, read by harnexus's documented settings so both sides agree.
export const callSocketPath = (env: Env): string | { error: string } => {
  const value = env[CALL_SOCKET_ENV];
  if (value === "off") return { error: `${CALL_SOCKET_ENV} is off` };
  if (value !== undefined && value !== "" && value !== "on")
    return isAbsolute(value)
      ? value
      : { error: `${CALL_SOCKET_ENV} must be on, off or an absolute path` };
  const state = env[STATE_PATH_ENV];
  if (state !== undefined && state !== "" && !isAbsolute(state))
    return { error: `${STATE_PATH_ENV} must be an absolute path` };
  const threads =
    state === undefined || state === ""
      ? join(homeOf(env), ".local/state/harnexus/threads.json")
      : state;
  return join(dirname(threads), "call.sock");
};

// Longer than the longest wait the bridge allows, so the bridge always answers first.
const CALL_TIMEOUT_MS = 15 * 60_000;
const QUERY_TIMEOUT_MS = 10_000;

const SOCKET_HINT =
  "open the harnexus app with its call socket on and run pr-wake outside the sandbox";

const socketHint = (code: string | null) => {
  switch (code) {
    case "ENOENT":
      return `no bridge has opened the socket; ${SOCKET_HINT}`;
    case "ECONNREFUSED":
      return "a bridge that has exited left the socket; the running bridge takes it over within seconds, so the next cycle tries again";
    case "EPERM":
    case "EACCES":
      return "the sandbox blocked the connection; run pr-wake outside the sandbox";
    default:
      return SOCKET_HINT;
  }
};
