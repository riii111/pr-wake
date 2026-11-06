import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { programWatch } from "./watch-program.ts";

const cleanups: (() => Promise<unknown>)[] = [];

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

const program = async () => {
  const dir = await mkdtemp(join(tmpdir(), "pr-wake-program-"));
  cleanups.push(() => rm(dir, { recursive: true, force: true }));
  const path = join(dir, "pr-wake-engine");
  await writeFile(path, "old");
  return { dir, path };
};

describe("programWatch", () => {
  test("tells a program swapped in at its path, as an install does, and not one left alone", async () => {
    const { dir, path } = await program();
    const replaced = await programWatch(path, true);
    const before = await replaced?.();

    await writeFile(join(dir, "next"), "old");
    await rename(join(dir, "next"), path);

    expect([before, await replaced?.()]).toEqual([false, true]);
  });

  test("waits while the program is missing", async () => {
    const { path } = await program();
    const replaced = await programWatch(path, true);

    await rm(path);

    expect(await replaced?.()).toBe(false);
  });

  test.each([
    { name: "a program run from source", compiled: false, exists: true },
    { name: "a program it cannot inspect", compiled: true, exists: false },
  ])("cannot tell for $name", async ({ compiled, exists }) => {
    const { path } = await program();
    if (!exists) await rm(path);

    expect(await programWatch(path, compiled)).toBeNull();
  });
});
