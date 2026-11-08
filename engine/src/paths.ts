import { homedir } from "node:os";
import { join } from "node:path";

import type { WatchPaths } from "./prwatch/watcher.ts";

export type Env = Record<string, string | undefined>;

export const homeOf = (env: Env) => env.HOME ?? homedir();

// A relative directory is ignored, as the XDG spec has it, so the working directory never picks the config or the state.
const absoluteOr = (value: string | undefined, fallback: string) =>
  value?.startsWith("/") ? value : fallback;

// The watch shares its state directory with harnexus-crew.
export const watchPaths = (env: Env): WatchPaths => ({
  config: join(
    absoluteOr(env.XDG_CONFIG_HOME, join(homeOf(env), ".config")),
    "harnexus",
    "pr-watch.json",
  ),
  state: join(
    absoluteOr(env.XDG_STATE_HOME, join(homeOf(env), ".local/state")),
    "harnexus-crew",
    "watch",
  ),
});
