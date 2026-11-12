use std::{ffi::OsString, fmt::Write as _, path::Path, time::Duration};

use super::{
    command::{RunTool, ToolCall},
    stop::StopRequest,
};

// The documented route that opens a new local chat with the prompt in its composer; the owner
// sends it.
const NEW_CHAT: &str = "codex://new";
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

#[must_use]
pub(crate) fn new_chat_link(prompt: &str, chat_home: &Path) -> String {
    format!(
        "{NEW_CHAT}?prompt={}&path={}",
        query_escape(prompt),
        query_escape(&chat_home.to_string_lossy())
    )
}

pub(crate) fn open_link(
    run: &RunTool<'_>,
    open: &Path,
    link: &str,
    stop: &StopRequest,
) -> Result<(), String> {
    run(&ToolCall {
        program: open,
        arguments: vec![OsString::from(link)],
        timeout: OPEN_TIMEOUT,
        stop,
    })
    .map(drop)
}

// Every byte but the unreserved ones is escaped, and a space as %20 rather than "+", which a
// decoder may keep as a literal plus.
fn query_escape(text: &str) -> String {
    text.bytes()
        .fold(String::with_capacity(text.len()), |mut escaped, byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                escaped.push(char::from(byte));
            } else {
                let _ = write!(escaped, "%{byte:02X}");
            }
            escaped
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_carries_the_prompt_and_the_chat_home_intact() {
        let prompt = "[$code-review](/x/SKILL.md)\nPR: https://github.com/a/b/pull/1?x=1&y=2#frag 100% +ok レビュー";
        let path = Path::new("/Users/me/review worktrees/a&b");

        let link = new_chat_link(prompt, path);

        let query = link
            .strip_prefix("codex://new?")
            .expect("the new chat route");
        assert!(!query.contains([' ', '\n', '#', '+']), "{link}");
        let (prompt_part, path_part) = query.split_once('&').expect("two parameters");
        assert_eq!(
            unescape(prompt_part.strip_prefix("prompt=").expect("prompt")),
            prompt
        );
        assert_eq!(
            unescape(path_part.strip_prefix("path=").expect("path")),
            path.to_str().expect("text")
        );
    }

    #[test]
    fn space_is_escaped_as_percent_twenty() {
        assert_eq!(query_escape("a b+c"), "a%20b%2Bc");
    }

    fn unescape(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut decoded = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                let hex = text.get(index + 1..index + 3).expect("two hex digits");
                decoded.push(u8::from_str_radix(hex, 16).expect("hex"));
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        String::from_utf8(decoded).expect("UTF-8")
    }
}
