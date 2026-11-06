// How the app marks a message one thread sent to another, read from harnexus's public thread view.
export const isDelegatingTool = (name: string) => DELEGATING_TOOLS.has(name);

// Thread reads can truncate the input; keep its available prefix for correlation.
export const delegationFields = (text: string) => ({
  source: sourceThread(text) ?? "",
  input: unescapeXml(/<input>([\s\S]*?)(?:<\/input>|$)/.exec(text)?.[1] ?? ""),
});

const sourceThread = (text: string) => {
  for (const match of text.matchAll(SOURCE_THREAD)) {
    const source = match[1];
    if (source !== undefined) return source;
  }

  return null;
};

const unescapeXml = (text: string) =>
  text
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");

const DELEGATING_TOOLS: ReadonlySet<string> = new Set([
  "create_thread",
  "send_message_to_thread",
]);

const SOURCE_THREAD = /<source_thread_id>\s*([^<\s]+)\s*<\/source_thread_id>/g;
