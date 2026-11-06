import { Result, TaggedError } from "better-result";

import type { Env } from "../paths.ts";
import { queryCapabilities } from "./call.ts";

export class ProtocolUnsupported extends TaggedError("ProtocolUnsupported")<{
  message: string;
}> {}

// Decides, before the watch sends anything, whether the running harnexus speaks a call protocol this engine accepts.
export type ProtocolCheck = () => Promise<Result<void, ProtocolUnsupported>>;

// The tools the watch reads workers and sends to them with.
export const WATCH_OPERATIONS = ["read_thread", "send_message_to_thread"];

// The watch starts only once the bridge answers protocol 1 with every tool it uses; it never falls back to the versionless protocol.
export const checkV1 =
  (env: Env): ProtocolCheck =>
  async () => {
    const offered = await queryCapabilities(env);
    if (offered.outcome !== "done")
      return Result.err(
        new ProtocolUnsupported({
          message: `cannot use the harnexus call socket: ${offered.message}`,
        }),
      );
    const missing = WATCH_OPERATIONS.filter(
      (op) => !offered.operations.includes(op),
    );
    return missing.length === 0
      ? Result.ok()
      : Result.err(
          new ProtocolUnsupported({
            message: `the harnexus bridge does not offer ${missing.join(", ")}; update harnexus and restart the app`,
          }),
        );
  };
