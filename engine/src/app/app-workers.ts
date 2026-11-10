import type { Workers } from "../prwatch/worker.ts";
import { isT3Worker } from "./t3-worker.ts";

// The workers of both apps, each linked worker reached through the app its link names.
export const appWorkers = (codex: Workers, t3: Workers): Workers => {
  const of = (worker: string) => (isT3Worker(worker) ? t3 : codex);
  return {
    senderOf: (worker) => of(worker).senderOf(worker),
    look: (sender, worker, awaited, now) =>
      of(worker).look(sender, worker, awaited, now),
    send: (sender, worker, prompt) => of(worker).send(sender, worker, prompt),
  };
};
