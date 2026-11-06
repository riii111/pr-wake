import { connect } from "node:net";

import { Result, TaggedError } from "better-result";

import { isObject } from "./object.ts";

class SocketRequestFailed extends TaggedError("SocketRequestFailed")<{
  path: string;
  code: string | null;
  // False when the connection itself failed, so the line was never written.
  connected: boolean;
  message: string;
}> {}

export const requestLine = (path: string, line: string, timeoutMs: number) => {
  let connected = false;
  return Result.tryPromise({
    try: () =>
      new Promise<string>((resolve, reject) => {
        const socket = connect(path);
        let answer = "";
        const timer = setTimeout(() => {
          socket.destroy();
          reject(Object.assign(new Error("timed out"), { code: "ETIMEDOUT" }));
        }, timeoutMs);
        socket.setEncoding("utf8");
        socket.on("connect", () => {
          connected = true;
          // An empty line is only a probe: the server answers nothing to it but the connection proves it is alive.
          if (line === "") {
            clearTimeout(timer);
            socket.destroy();
            resolve("");
            return;
          }

          socket.write(`${line}\n`);
        });
        socket.on("data", (chunk: string) => {
          answer += chunk;
        });
        socket.on("end", () => {
          clearTimeout(timer);
          resolve(answer.trimEnd());
        });
        socket.on("error", (error) => {
          clearTimeout(timer);
          reject(error);
        });
      }),
    catch: (cause) =>
      new SocketRequestFailed({
        path,
        code:
          isObject(cause) && typeof cause.code === "string" ? cause.code : null,
        connected,
        message: `cannot reach ${path}`,
      }),
  });
};
