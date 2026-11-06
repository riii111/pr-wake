import { Result, TaggedError } from "better-result";

import type { AppCall } from "./call.ts";

export class ProtocolUnsupported extends TaggedError("ProtocolUnsupported")<{
  message: string;
}> {}

// Decides, before the watch sends anything, whether the running harnexus speaks a call protocol this engine accepts.
export type ProtocolCheck = (
  callApp: AppCall,
  caller: string,
) => Promise<Result<void, ProtocolUnsupported>>;

// TODO(S2a): replace with the v1 query of harnexus's public call protocol (version and supported tools), refuse an `invalid` answer from an older bridge as an unsupported version, and never fall back to v0.
export const unverifiedV0: ProtocolCheck = async () => Result.ok();
