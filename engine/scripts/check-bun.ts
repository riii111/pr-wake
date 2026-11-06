import { join } from "node:path";

// A compiled engine carries the Bun that built it, so a Bun other than the pinned one is stopped at the build rather than when the watch runs.
const manifest = await Bun.file(join(import.meta.dir, "../package.json")).json();
const pinned = /^bun@(.+)$/.exec(String(manifest.packageManager))?.[1] ?? null;
if (pinned !== Bun.version) {
  process.stderr.write(
    `build: Bun ${Bun.version} runs this build, while package.json pins ${pinned ?? "no Bun version"}\n`,
  );
  process.exitCode = 1;
}
