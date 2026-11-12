use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

pub(crate) struct AgentPlist<'plist> {
    pub(crate) label: &'plist str,
    pub(crate) program: &'plist [String],
    pub(crate) home: &'plist Path,
    // Names the nix-darwin profile on the agent's PATH.
    pub(crate) user: Option<&'plist str>,
    // Set besides HOME, PATH and LANG, written in this order.
    pub(crate) environment: &'plist [(&'plist str, String)],
    pub(crate) log: &'plist Path,
    pub(crate) process_type: ProcessType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessType {
    // For an agent the owner answers, as the review's notifications are.
    Interactive,
    Background,
}

// Restarted after a crash or a config error at most once a minute, but not after a clean stop.
const THROTTLE_SECONDS: u32 = 60;
// launchd waits 5 seconds by default, as `launchctl print` shows for an agent that sets no exit timeout.
// A stop may take 6 seconds while a child gets its SIGTERM grace, so this leaves room to write the records.
const EXIT_TIMEOUT_SECONDS: u32 = 15;

impl AgentPlist<'_> {
    #[must_use]
    pub(crate) fn render(&self) -> String {
        let path = agent_path(self.home, self.user)
            .iter()
            .map(|directory| directory.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");

        let mut plist = String::from(concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n<dict>\n",
        ));
        key(&mut plist, "\t", "Label");
        string(&mut plist, "\t", self.label);
        key(&mut plist, "\t", "ProgramArguments");
        plist.push_str("\t<array>\n");
        for argument in self.program {
            string(&mut plist, "\t\t", argument);
        }
        plist.push_str("\t</array>\n");

        key(&mut plist, "\t", "EnvironmentVariables");
        plist.push_str("\t<dict>\n");
        let home = self.home.to_string_lossy();
        let fixed = [
            ("HOME", &*home),
            ("PATH", path.as_str()),
            ("LANG", "en_US.UTF-8"),
        ];
        let set = self
            .environment
            .iter()
            .map(|(name, value)| (*name, value.as_str()));
        for (name, value) in fixed.into_iter().chain(set) {
            key(&mut plist, "\t\t", name);
            string(&mut plist, "\t\t", value);
        }
        plist.push_str("\t</dict>\n");

        key(&mut plist, "\t", "RunAtLoad");
        plist.push_str("\t<true/>\n");
        key(&mut plist, "\t", "KeepAlive");
        plist.push_str("\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n");
        key(&mut plist, "\t", "ThrottleInterval");
        integer(&mut plist, THROTTLE_SECONDS);
        key(&mut plist, "\t", "ExitTimeOut");
        integer(&mut plist, EXIT_TIMEOUT_SECONDS);

        // Notifications, the app's call socket and the gh keychain belong to the logged-in GUI session.
        key(&mut plist, "\t", "LimitLoadToSessionType");
        string(&mut plist, "\t", "Aqua");
        key(&mut plist, "\t", "ProcessType");
        string(&mut plist, "\t", self.process_type.name());

        let log = self.log.to_string_lossy();
        key(&mut plist, "\t", "StandardOutPath");
        string(&mut plist, "\t", &log);
        key(&mut plist, "\t", "StandardErrorPath");
        string(&mut plist, "\t", &log);
        plist.push_str("</dict>\n</plist>\n");
        plist
    }
}

impl ProcessType {
    const fn name(self) -> &'static str {
        match self {
            Self::Interactive => "Interactive",
            Self::Background => "Background",
        }
    }
}

// launchd gives an agent no login shell PATH, so the agent and the review's command lookup search these directories.
#[must_use]
pub(crate) fn agent_path(home: &Path, user: Option<&str>) -> Vec<PathBuf> {
    let mut directories = vec![home.join(".nix-profile/bin")];
    if let Some(user) = user {
        directories.push(Path::new("/etc/profiles/per-user").join(user).join("bin"));
    }
    directories.extend(
        [
            "/run/current-system/sw/bin",
            "/nix/var/nix/profiles/default/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .map(PathBuf::from),
    );
    directories
}

fn key(plist: &mut String, indent: &str, name: &str) {
    let _ = writeln!(plist, "{indent}<key>{name}</key>");
}

fn string(plist: &mut String, indent: &str, value: &str) {
    plist.push_str(indent);
    plist.push_str("<string>");
    for c in value.chars() {
        match c {
            '&' => plist.push_str("&amp;"),
            '<' => plist.push_str("&lt;"),
            '>' => plist.push_str("&gt;"),
            c => plist.push(c),
        }
    }
    plist.push_str("</string>\n");
}

fn integer(plist: &mut String, value: u32) {
    let _ = writeln!(plist, "\t<integer>{value}</integer>");
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::*;

    #[test]
    fn plist_runs_the_program_with_the_agent_path_in_the_gui_session() {
        let plist = AgentPlist {
            label: "io.example.agent",
            program: &[
                "/Users/me/bin/tool".to_owned(),
                "run".to_owned(),
                "a&b<c>".to_owned(),
            ],
            home: Path::new("/Users/me"),
            user: Some("me"),
            environment: &[("XDG_STATE_HOME", "/Users/me/st".to_owned())],
            log: Path::new("/Users/me/Library/Logs/tool.log"),
            process_type: ProcessType::Interactive,
        };

        let rendered = plist.render();

        assert_snapshot!(rendered, @r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
        <plist version="1.0">
        <dict>
        	<key>Label</key>
        	<string>io.example.agent</string>
        	<key>ProgramArguments</key>
        	<array>
        		<string>/Users/me/bin/tool</string>
        		<string>run</string>
        		<string>a&amp;b&lt;c&gt;</string>
        	</array>
        	<key>EnvironmentVariables</key>
        	<dict>
        		<key>HOME</key>
        		<string>/Users/me</string>
        		<key>PATH</key>
        		<string>/Users/me/.nix-profile/bin:/etc/profiles/per-user/me/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
        		<key>LANG</key>
        		<string>en_US.UTF-8</string>
        		<key>XDG_STATE_HOME</key>
        		<string>/Users/me/st</string>
        	</dict>
        	<key>RunAtLoad</key>
        	<true/>
        	<key>KeepAlive</key>
        	<dict>
        		<key>SuccessfulExit</key>
        		<false/>
        	</dict>
        	<key>ThrottleInterval</key>
        	<integer>60</integer>
        	<key>ExitTimeOut</key>
        	<integer>15</integer>
        	<key>LimitLoadToSessionType</key>
        	<string>Aqua</string>
        	<key>ProcessType</key>
        	<string>Interactive</string>
        	<key>StandardOutPath</key>
        	<string>/Users/me/Library/Logs/tool.log</string>
        	<key>StandardErrorPath</key>
        	<string>/Users/me/Library/Logs/tool.log</string>
        </dict>
        </plist>
        "#);
    }

    #[test]
    fn agent_path_covers_nix_homebrew_and_the_system() {
        let directories = agent_path(Path::new("/Users/me"), Some("me"));

        assert_eq!(
            directories,
            [
                "/Users/me/.nix-profile/bin",
                "/etc/profiles/per-user/me/bin",
                "/run/current-system/sw/bin",
                "/nix/var/nix/profiles/default/bin",
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ]
            .map(PathBuf::from)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_passes_plutil_lint() {
        let directory = tempfile::tempdir().expect("create a directory");
        let path = directory.path().join("agent.plist");
        let plist = AgentPlist {
            label: "io.example.agent",
            program: &["/Users/me/bin/tool".to_owned(), "a&b<c>".to_owned()],
            home: Path::new("/Users/me"),
            user: None,
            environment: &[("HARNEXUS_CALL_SOCKET", "on".to_owned())],
            log: Path::new("/Users/me/Library/Logs/tool.log"),
            process_type: ProcessType::Background,
        };
        std::fs::write(&path, plist.render()).expect("write the plist");

        let linted = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .expect("run plutil");

        assert!(
            linted.status.success(),
            "plutil -lint: {}",
            String::from_utf8_lossy(&linted.stdout)
        );
    }
}
