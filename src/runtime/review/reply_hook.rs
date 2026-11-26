use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::infra::{
    record::{self, JsonLayout},
    watch_record,
};

const MATCHER: &str = "^mcp__.*__send_message_to_thread$";
const LABEL: &str = "Checking review reply destination";

pub(super) fn install(home: &Path, program: &str, watch_state: &Path) -> Result<PathBuf, String> {
    let path = home.join(".codex/hooks.json");
    let mut config = record::load_json::<Value>(&path)
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| json!({}));
    let watch_state = watch_state
        .to_str()
        .ok_or("the watch state path is not UTF-8")?;
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let handler = json!({
        "type": "command",
        "command": format!("{} review check-reply {}", quote(program), quote(watch_state)),
        "timeout": 5,
        "statusMessage": LABEL,
    });
    let changed = register(&mut config, handler)
        .ok_or_else(|| format!("{}: invalid hooks config", path.display()))?;
    if changed {
        record::save_json(&path, &config, JsonLayout::TwoSpacesAndNewline)
            .map_err(|error| error.to_string())?;
    }
    Ok(path)
}

fn register(config: &mut Value, handler: Value) -> Option<bool> {
    let groups = config
        .as_object_mut()?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()?
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()?;
    for group in groups
        .iter_mut()
        .filter(|group| group["matcher"] == MATCHER)
    {
        let handlers = group.get_mut("hooks")?.as_array_mut()?;
        if let Some(existing) = handlers
            .iter_mut()
            .find(|entry| entry["statusMessage"] == LABEL)
        {
            if *existing == handler {
                return Some(false);
            }
            *existing = handler;
            return Some(true);
        }
    }
    groups.push(json!({ "matcher": MATCHER, "hooks": [handler] }));
    Some(true)
}

pub(super) fn denial(event: &Value, watch_state: &Path) -> Result<Option<Value>, String> {
    let Some(target) = event["tool_input"]["threadId"].as_str() else {
        return Ok(None);
    };
    let sender = watch_record::load_sender(watch_state).map_err(|error| error.to_string())?;
    Ok((sender.as_deref() == Some(target)).then(|| json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "このチャットは pr-wake の起動用です。調査結果は依頼したレビュー担当に、レビュー結果はレビューチャットに返してください。",
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_preserves_other_hooks_and_updates_its_own_without_duplicates() {
        let home = tempfile::tempdir().expect("temporary project");
        let path = home.path().join(".codex/hooks.json");
        let existing =
            json!({"matcher": MATCHER, "hooks": [{"type": "command", "command": "other hook"}]});
        let config = json!({"hooks": {"PreToolUse": [existing]}});
        record::save_json(&path, &config, JsonLayout::TwoSpacesAndNewline).expect("write hooks");

        let state = home.path().join("watcher's state");
        install(home.path(), "/old/pr-wake", &state).expect("install hook");
        install(home.path(), "/new/pr-wake", &state).expect("update hook");
        install(home.path(), "/new/pr-wake", &state).expect("reinstall hook");

        let installed: Value = record::load_required_json(&path).expect("read hooks");
        let hooks = installed["hooks"]["PreToolUse"]
            .as_array()
            .expect("hook groups");
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0], existing);
        assert_eq!(
            hooks[1]["hooks"][0]["command"],
            format!(
                "'/new/pr-wake' review check-reply '{}/watcher'\\''s state'",
                home.path().display()
            )
        );
    }

    #[test]
    fn only_the_registered_sender_is_rejected() {
        let state = tempfile::tempdir().expect("temporary state");
        let call = |target| json!({"tool_input": {"threadId": target}});
        assert_eq!(denial(&call("sender"), state.path()), Ok(None));
        watch_record::record_sender(state.path(), "sender", "2026-11-26T00:00:00Z")
            .expect("record sender");

        let result = denial(&call("sender"), state.path()).expect("read sender");
        assert_eq!(
            result.expect("denial")["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert_eq!(denial(&call("review"), state.path()), Ok(None));
    }
}
