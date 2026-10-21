package main

import (
	"context"
	"net/url"
	"strings"
)

// The documented route opens a new local chat with the prompt in the composer; the user sends it.
const deepLinkBase = "codex://new"

func deepLink(prompt, path string) string {
	return deepLinkBase + "?prompt=" + queryEscape(prompt) + "&path=" + queryEscape(path)
}

// Spaces become %20 rather than "+", which a decoder may keep as a literal plus.
func queryEscape(s string) string {
	return strings.ReplaceAll(url.QueryEscape(s), "+", "%20")
}

func openDeepLink(ctx context.Context, r Runner, open, link string) error {
	_, err := r.Run(ctx, open, link)
	return err
}
