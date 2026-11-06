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

export const callApp = async (
  env: Env,
  caller: string,
  tool: string,
  args: Fields,
  timeoutMs = CALL_TIMEOUT_MS,
): Promise<Fields> => {
  const path = callSocketPath(env);
  if (typeof path !== "string")
    return { outcome: "not_sent", error: `${path.error}; ${SOCKET_HINT}` };
  const line = JSON.stringify({ threadId: caller, tool, arguments: args });
  const answer = await requestLine(path, line, timeoutMs);
  if (answer.isErr()) {
    const reason = answer.error.code ?? answer.error.message;
    // The request may already have reached the App once connected.
    return answer.error.connected
      ? { outcome: "unknown", error: `no answer: ${reason}` }
      : {
          outcome: "not_sent",
          error: `${path}: ${reason}; ${socketHint(answer.error.code)}`,
        };
  }

  const parsed = parseJson(answer.value);
  if (parsed.isErr())
    return { outcome: "unknown", error: "no answer: invalid JSON" };
  if (!isObject(parsed.value) || !Object.hasOwn(parsed.value, "outcome"))
    return { outcome: "unknown", error: "no answer: answer without outcome" };
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

const CALL_TIMEOUT_MS = 15 * 60_000;

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
