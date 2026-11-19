use std::{ffi::OsString, path::Path, time::Duration};

use serde::Deserialize;

use super::{
    command::{RunTool, ToolCall},
    stop::StopRequest,
};

pub(crate) struct Browser<'browser> {
    pub(crate) run: &'browser RunTool<'browser>,
    pub(crate) open: &'browser Path,
    pub(crate) plutil: &'browser Path,
    pub(crate) home: &'browser Path,
}

#[derive(Deserialize)]
struct Handler {
    #[serde(default, rename = "LSHandlerURLScheme")]
    scheme: Option<String>,
    #[serde(default, rename = "LSHandlerRoleAll")]
    bundle: Option<String>,
}

// Chromium browsers hand a URL given with --new-window to the running browser, which opens it in a new window.
const NEW_WINDOW_BROWSERS: [&str; 7] = [
    "com.google.chrome",
    "com.google.chrome.beta",
    "com.google.chrome.canary",
    "com.brave.browser",
    "com.microsoft.edgemac",
    "com.vivaldi.vivaldi",
    "org.chromium.chromium",
];
const HANDLERS: &str =
    "Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist";
const TIMEOUT: Duration = Duration::from_secs(30);

impl Browser<'_> {
    pub(crate) fn open_in_new_window(&self, url: &str, stop: &StopRequest) -> Result<(), String> {
        let arguments = match self.default_browser(stop) {
            Some(bundle) if NEW_WINDOW_BROWSERS.contains(&bundle.as_str()) => {
                vec!["-n", "-b", &bundle, "--args", "--new-window", url]
                    .into_iter()
                    .map(OsString::from)
                    .collect()
            }
            _ => vec![OsString::from(url)],
        };
        (self.run)(&ToolCall {
            program: self.open,
            arguments,
            timeout: TIMEOUT,
            stop,
        })
        .map(drop)
    }

    fn default_browser(&self, stop: &StopRequest) -> Option<String> {
        let output = (self.run)(&ToolCall {
            program: self.plutil,
            arguments: vec![
                OsString::from("-extract"),
                OsString::from("LSHandlers"),
                OsString::from("json"),
                OsString::from("-o"),
                OsString::from("-"),
                self.home.join(HANDLERS).into_os_string(),
            ],
            timeout: TIMEOUT,
            stop,
        })
        .ok()?;
        default_of(&output)
    }
}

fn default_of(handlers: &[u8]) -> Option<String> {
    serde_json::from_slice::<Vec<Handler>>(handlers)
        .ok()?
        .into_iter()
        .find(|handler| handler.scheme.as_deref() == Some("https"))?
        .bundle
        .map(|bundle| bundle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const URL: &str = "https://github.com/acme/app/pull/7";

    #[test]
    fn chromium_default_opens_a_new_window_and_others_open_as_usual() {
        for (name, handlers, expected) in [
            (
                "chrome",
                Ok(
                    r#"[{"LSHandlerURLScheme":"mailto","LSHandlerRoleAll":"com.apple.mail"},{"LSHandlerURLScheme":"https","LSHandlerRoleAll":"com.google.Chrome"}]"#,
                ),
                format!("open -n -b com.google.chrome --args --new-window {URL}"),
            ),
            (
                "safari",
                Ok(r#"[{"LSHandlerURLScheme":"https","LSHandlerRoleAll":"com.apple.safari"}]"#),
                format!("open {URL}"),
            ),
            ("never_chosen", Ok("[]"), format!("open {URL}")),
            ("unreadable", Err(()), format!("open {URL}")),
        ] {
            let opened = Mutex::new(Vec::new());
            let run = |call: &ToolCall<'_>| {
                if call.program == Path::new("plutil") {
                    return handlers
                        .map(|handlers| handlers.as_bytes().to_vec())
                        .map_err(|()| "no such file".to_owned());
                }
                let line = std::iter::once(call.program.as_os_str())
                    .chain(call.arguments.iter().map(OsString::as_os_str))
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ");
                opened.lock().expect("note the open").push(line);
                Ok(Vec::new())
            };
            let browser = Browser {
                run: &run,
                open: Path::new("open"),
                plutil: Path::new("plutil"),
                home: Path::new("/Users/me"),
            };

            let result = browser.open_in_new_window(URL, &StopRequest::default());

            assert_eq!(result, Ok(()), "case: {name}");
            assert_eq!(
                opened.into_inner().expect("read the opens"),
                [expected],
                "case: {name}"
            );
        }
    }
}
