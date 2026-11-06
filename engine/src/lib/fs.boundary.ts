import { randomUUID } from "node:crypto";
import { constants, ftruncateSync, openSync, writeSync } from "node:fs";
import { mkdir, open, readdir, readFile, rename, rm, stat } from "node:fs/promises";
import { basename, dirname, join } from "node:path";

import { Result, TaggedError } from "better-result";

import { isObject } from "./object.ts";

class FileReadFailed extends TaggedError("FileReadFailed")<{
  path: string;
  cause: unknown;
  message: string;
}> {}

export class FileWriteFailed extends TaggedError("FileWriteFailed")<{
  path: string;
  cause: unknown;
  message: string;
}> {}

export class FileSyncFailed extends TaggedError("FileSyncFailed")<{
  path: string;
  cause: unknown;
  message: string;
}> {}

class DirectoryPrepareFailed extends TaggedError("DirectoryPrepareFailed")<{
  path: string;
  cause: unknown;
  message: string;
}> {}

export const readTextFileIfExists = (path: string) =>
  Result.tryPromise({
    try: async () => {
      try {
        return await readFile(path, "utf8");
      } catch (cause) {
        if (isMissingFile(cause)) return null;
        throw cause;
      }
    },
    catch: (cause) =>
      new FileReadFailed({ path, cause, message: `cannot read ${path}` }),
  });

export const readRegularTextFile = (path: string) =>
  Result.tryPromise({
    try: async () => {
      // A FIFO or device would keep the read waiting forever, so only a regular file is read.
      if (!(await stat(path)).isFile()) {
        throw new Error(`${path} is not a regular file`);
      }

      return await readFile(path, "utf8");
    },
    catch: (cause) =>
      new FileReadFailed({ path, cause, message: `cannot read ${path}` }),
  });

export const writeFileAtomic = (path: string, content: string) =>
  Result.gen(async function* () {
    yield* Result.await(replaceFile(path, content));
    // A FileSyncFailed here means the file already holds the new content but the rename may not survive a crash.
    yield* Result.await(syncParent(path, "wrote"));
    return Result.ok();
  });

const replaceFile = (path: string, content: string) =>
  Result.tryPromise({
    try: async () => {
      const temporary = await writeTemporary(path, content);
      // The rename happens only after the temporary file is flushed to disk, so readers see either the previous or the new content.
      await rename(temporary, path).catch(async (cause) => {
        await rm(temporary, { force: true }).catch(() => {});
        throw cause;
      });
    },
    catch: (cause) =>
      new FileWriteFailed({ path, cause, message: `cannot write ${path}` }),
  });

const writeTemporary = async (path: string, content: string) => {
  const temporary = join(
    dirname(path),
    `.${basename(path)}.${randomUUID()}.tmp`,
  );
  try {
    const file = await open(temporary, "wx", OWNER_ONLY);
    try {
      await file.writeFile(content, "utf8");
      await file.sync();
    } finally {
      await file.close();
    }
  } catch (cause) {
    await rm(temporary, { force: true }).catch(() => {});
    throw cause;
  }

  return temporary;
};

export const fileIdentity = async (path: string) => {
  const found = await stat(path).catch(() => null);
  return found?.isFile()
    ? {
        // The device and inode tell a file swapped in at the path from one changed in place.
        dev: found.dev,
        ino: found.ino,
        size: found.size,
        modifiedMs: Math.trunc(found.mtimeMs),
      }
    : null;
};

export const isDirectory = async (path: string) =>
  (await stat(path).catch(() => null))?.isDirectory() ?? false;

export const prepareDirectory = (path: string) =>
  Result.tryPromise({
    try: async () => {
      await mkdir(path, { recursive: true, mode: OWNER_ONLY_DIRECTORY });
      // The parent is synced too, so files created inside later cannot be lost with a directory entry that never reached the disk.
      await syncDirectory(dirname(path));
    },
    catch: (cause) =>
      new DirectoryPrepareFailed({
        path,
        cause,
        message: `cannot prepare directory ${path}`,
      }),
  });

export const listDirectoryIfExists = (path: string) =>
  Result.tryPromise({
    try: async () => {
      try {
        return await readdir(path);
      } catch (cause) {
        if (isMissingFile(cause) || isNotDirectory(cause)) return null;
        throw cause;
      }
    },
    catch: (cause) =>
      new FileReadFailed({ path, cause, message: `cannot list ${path}` }),
  });

// Takes the lock at path for the life of the process, or answers false while another process holds it.
export const holdProcessLock = (path: string) =>
  Result.try({
    try: () => {
      try {
        // macOS takes the lock with the open itself, and the kernel drops it when the holder exits, so a crash never leaves it behind.
        const fd = openSync(path, LOCK_FLAGS, OWNER_ONLY);
        ftruncateSync(fd);
        writeSync(fd, `${process.pid}\n`);
        // The descriptor is never closed, so the lock lasts as long as the process.
        return true;
      } catch (cause) {
        if (isObject(cause) && cause.code === "EAGAIN") return false;
        throw cause;
      }
    },
    catch: (cause) =>
      new FileWriteFailed({ path, cause, message: `cannot lock ${path}` }),
  });

const O_EXLOCK = 0x20;

const LOCK_FLAGS =
  constants.O_RDWR | constants.O_CREAT | constants.O_NONBLOCK | O_EXLOCK;

const syncParent = (path: string, action: string) =>
  Result.tryPromise({
    try: () => syncDirectory(dirname(path)),
    catch: (cause) =>
      new FileSyncFailed({
        path,
        cause,
        message: `${action} ${path} but cannot sync its directory`,
      }),
  });

const syncDirectory = async (path: string) => {
  const directory = await open(path, "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
};

const isMissingFile = (cause: unknown) =>
  cause instanceof Error && "code" in cause && cause.code === "ENOENT";

const isNotDirectory = (cause: unknown) =>
  cause instanceof Error && "code" in cause && cause.code === "ENOTDIR";

const OWNER_ONLY = 0o600;

const OWNER_ONLY_DIRECTORY = 0o700;
