import { Result, TaggedError } from "better-result";

class GitHubUnreachable extends TaggedError("GitHubUnreachable")<{
  cause: unknown;
  message: string;
}> {}

export type GraphqlAnswer = { status: number; text: string };

// Sends a read-only query to GitHub's GraphQL API.
export const postGraphql = (
  token: string,
  query: string,
  variables: Record<string, unknown>,
  timeoutMs = REQUEST_TIMEOUT_MS,
) =>
  Result.tryPromise({
    try: async (): Promise<GraphqlAnswer> => {
      const response = await fetch(GRAPHQL_URL, {
        method: "POST",
        headers: {
          authorization: `bearer ${token}`,
          "content-type": "application/json",
          "user-agent": "pr-wake",
        },
        body: JSON.stringify({ query, variables }),
        signal: AbortSignal.timeout(timeoutMs),
      });
      // The caller parses the body, so an answer GitHub sends with an error status still reaches it.
      return { status: response.status, text: await response.text() };
    },
    catch: (cause) =>
      new GitHubUnreachable({ cause, message: "cannot reach api.github.com" }),
  });

const GRAPHQL_URL = "https://api.github.com/graphql";
const REQUEST_TIMEOUT_MS = 30_000;
