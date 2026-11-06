import { homedir } from "node:os";
import { join } from "node:path";

import type { WatchPaths } from "./prwatch/watcher.ts";

export type Env = Record<string, string | undefined>;

export const homeOf = (env: Env) => env.HOME ?? homedir();

// A relative directory is ignored, as the XDG spec has it, so the working directory never picks the config or the state.
const absoluteOr = (value: string | undefined, fallback: string) =>
  value?.startsWith("/") ? value : fallback;

// The config and records harnexus-task watch kept, so a watch moved to pr-wake goes on from them.
export const watchPaths = (env: Env): WatchPaths => ({
  config: join(
    absoluteOr(env.XDG_CONFIG_HOME, join(homeOf(env), ".config")),
    "harnexus",
    "pr-watch.json",
  ),
  state: join(
    absoluteOr(env.XDG_STATE_HOME, join(homeOf(env), ".local/state")),
    "taskctl",
    "watch",
  ),
});
