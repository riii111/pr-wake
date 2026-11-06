import { fileIdentity } from "../lib/fs.boundary.ts";

// A check of whether a newer engine was installed over the running one.
export const programWatch = async (path: string, compiled: boolean) => {
  // Null when the watch cannot tell, so the caller can say so once rather than run stale code without a word.
  if (!compiled) return null;
  const started = await fileIdentity(path);
  if (started === null) return null;

  return async () => {
    // An install swaps the file at the program's path, so a different file there means a newer engine is installed.
    const now = await fileIdentity(path);
    // A program missing for now is being reinstalled, and there is nothing to start until it is back.
    return now !== null && !Bun.deepEquals(now, started, true);
  };
};
