import json
import os
import sys
from pathlib import Path


event = json.load(sys.stdin)
arguments = event.get("tool_input", {})
if not event.get("tool_name", "").endswith("__send_message_to_thread"):
    sys.exit()

home = Path.home()
if Path(event.get("cwd", "")).resolve() != (home / "review-home").resolve():
    sys.exit()

state = Path(os.environ.get("XDG_STATE_HOME") or home / ".local/state")
try:
    sender = json.loads((state / "harnexus-crew/watch/sender.json").read_text())["thread"]
except (OSError, ValueError, KeyError):
    sys.exit()

if arguments.get("threadId") == sender:
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": (
                "このチャットは pr-wake の起動用です。"
                "調査結果は依頼したレビュー担当に、レビュー結果はレビューチャットに返してください。"
            ),
        },
    }))
