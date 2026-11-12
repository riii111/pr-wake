use std::{
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder},
    io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

use regex::Regex;

use crate::{
    app::review::PullRequest,
    infra::{
        command::{RunTool, ToolCall},
        stop::StopRequest,
    },
};

// Git run on one checkout, which keeps its branch, HEAD and working files.
pub(crate) struct Checkout<'git> {
    pub(crate) run: &'git RunTool<'git>,
    pub(crate) git: &'git Path,
    pub(crate) path: &'git Path,
    pub(crate) stop: &'git StopRequest,
}

// A fetch of a large repository is the slowest step.
const TIMEOUT: Duration = Duration::from_mins(10);
const MAX_WORKTREE_COPIES: u32 = 20;
const WORKTREE_DIRECTORY: u32 = 0o700;

static GITHUB_REMOTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:https://github\.com/|ssh://git@github\.com/|git@github\.com:)([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?/?$",
    )
    .expect("the remote pattern is valid")
});

// A clean worktree at the PR head under `base`, reused when one is already there; one with
// changes is never reused.
pub(crate) fn prepare_worktree(
    checkout: &Checkout<'_>,
    base: &Path,
    pr: &PullRequest,
) -> Result<PathBuf, String> {
    // The configured URL, before any url.<base>.insteadOf rewriting, names the repository.
    let remote = checkout.git_at(checkout.path, ["config", "--get", "remote.origin.url"])?;
    let remote = String::from_utf8_lossy(&remote);
    if !remote_repo(&remote).is_some_and(|repo| repo.eq_ignore_ascii_case(&pr.repo)) {
        return Err(format!(
            "{}: origin {:?} is not {}",
            checkout.path.display(),
            remote.trim(),
            pr.repo
        ));
    }

    let (path, reused) = pick_worktree(checkout, base, &pr.head_sha)?;
    if reused {
        return Ok(path);
    }
    if !checkout.has_commit(&pr.head_sha) {
        fetch_head(checkout, pr)?;
    }
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(WORKTREE_DIRECTORY)
            .create(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    checkout.git_at(
        checkout.path,
        [
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            path.as_os_str(),
            pr.head_sha.as_ref(),
        ],
    )?;
    Ok(path)
}

impl Checkout<'_> {
    fn has_commit(&self, sha: &str) -> bool {
        self.git_at(self.path, ["cat-file", "-e", &format!("{sha}^{{commit}}")])
            .is_ok()
    }

    fn is_clean_at(&self, worktree: &Path, sha: &str) -> bool {
        let at_head = self
            .git_at(worktree, ["rev-parse", "HEAD"])
            .is_ok_and(|head| String::from_utf8_lossy(&head).trim() == sha);
        at_head
            && self
                .git_at(worktree, ["status", "--porcelain", "--untracked-files=all"])
                .is_ok_and(|status| String::from_utf8_lossy(&status).trim().is_empty())
    }

    fn git_at<A: AsRef<OsStr>>(
        &self,
        directory: &Path,
        arguments: impl IntoIterator<Item = A>,
    ) -> Result<Vec<u8>, String> {
        let mut all = vec![OsString::from("-C"), directory.as_os_str().to_owned()];
        all.extend(
            arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_owned()),
        );
        (self.run)(&ToolCall {
            program: self.git,
            arguments: all,
            timeout: TIMEOUT,
            stop: self.stop,
        })
    }
}

// The pull ref also serves heads from forks; the SHA itself covers a PR that moved on since it
// was read.
fn fetch_head(checkout: &Checkout<'_>, pr: &PullRequest) -> Result<(), String> {
    for reference in [format!("refs/pull/{}/head", pr.number), pr.head_sha.clone()] {
        let fetched = checkout.git_at(
            checkout.path,
            [
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "origin",
                &reference,
            ],
        );
        if fetched.is_ok() && checkout.has_commit(&pr.head_sha) {
            return Ok(());
        }
    }
    Err(format!(
        "{}: cannot fetch {}",
        checkout.path.display(),
        pr.head_sha
    ))
}

fn pick_worktree(
    checkout: &Checkout<'_>,
    base: &Path,
    sha: &str,
) -> Result<(PathBuf, bool), String> {
    for copy in 1..=MAX_WORKTREE_COPIES {
        let path = if copy == 1 {
            base.to_owned()
        } else {
            let mut numbered = base.as_os_str().to_owned();
            numbered.push(format!("-{copy}"));
            PathBuf::from(numbered)
        };
        match fs::metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((path, false)),
            Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
            Ok(_) if checkout.is_clean_at(&path, sha) => return Ok((path, true)),
            Ok(_) => {}
        }
    }
    Err(format!(
        "{}: {MAX_WORKTREE_COPIES} worktrees of this head exist and none is clean; remove those no longer needed",
        base.display()
    ))
}

fn remote_repo(remote: &str) -> Option<&str> {
    GITHUB_REMOTE
        .captures(remote.trim())
        .and_then(|found| found.get(1))
        .map(|repo| repo.as_str())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::infra::command::ExternalCommand;

    // A checkout of acme/app whose origin is served from a local bare repository, with the PR
    // head reachable only through refs/pull/12/head as on GitHub.
    struct GitRepo {
        directory: tempfile::TempDir,
        checkout: PathBuf,
        pr: PullRequest,
    }

    #[test]
    fn remote_url_names_its_github_repository() {
        for (name, remote, expected) in [
            (
                "scp_like",
                "git@github.com:acme/app.git\n",
                Some("acme/app"),
            ),
            ("https", "https://github.com/acme/app", Some("acme/app")),
            ("ssh", "ssh://git@github.com/acme/app.git", Some("acme/app")),
            ("other_host", "https://gitlab.com/acme/app.git", None),
        ] {
            assert_eq!(remote_repo(remote), expected, "case: {name}");
        }
    }

    #[test]
    fn worktree_is_pinned_to_the_pr_head_and_leaves_the_checkout() {
        let repo = GitRepo::new();
        let before = repo.git(&repo.checkout, &["rev-parse", "HEAD"]);

        let path = repo.prepare().expect("prepare the worktree");

        assert_eq!(repo.git(&path, &["rev-parse", "HEAD"]), repo.pr.head_sha);
        assert!(
            !repo.succeeds(&path, &["symbolic-ref", "-q", "HEAD"]),
            "the worktree must be detached"
        );
        assert_eq!(repo.git(&repo.checkout, &["rev-parse", "HEAD"]), before);
        assert_eq!(
            repo.git(&repo.checkout, &["branch", "--show-current"]),
            "main"
        );
        assert_eq!(repo.git(&repo.checkout, &["status", "--porcelain"]), "");
    }

    #[test]
    fn worktree_with_changes_is_never_reused() {
        let repo = GitRepo::new();
        let first = repo.prepare().expect("prepare the worktree");
        let again = repo.prepare().expect("prepare it again");
        let edited = first.join("file.txt");
        fs::write(&edited, "left over\n").expect("edit the worktree");

        let second = repo.prepare().expect("prepare another");

        assert_eq!(again, first, "a clean worktree must be reused");
        assert_ne!(second, first);
        assert_eq!(repo.git(&second, &["status", "--porcelain"]), "");
        assert_eq!(
            fs::read_to_string(&edited).expect("read the change"),
            "left over\n"
        );
    }

    #[test]
    fn checkout_of_another_repository_gets_no_worktree() {
        let directory = tempfile::tempdir().expect("create a directory");
        let checkout = directory.path().join("src/app");
        let repo = GitRepo {
            checkout: checkout.clone(),
            pr: PullRequest {
                repo: "acme/app".to_owned(),
                number: 12,
                title: String::new(),
                url: String::new(),
                head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                base_ref: "main".to_owned(),
            },
            directory,
        };
        repo.git(
            repo.directory.path(),
            &["init", "-q", checkout.to_str().expect("text")],
        );
        repo.git(
            &checkout,
            &["remote", "add", "origin", "git@github.com:acme/other.git"],
        );

        let prepared = repo.prepare();

        assert!(prepared.is_err(), "{prepared:?}");
        assert!(!repo.worktree_root().exists(), "no worktree may be created");
    }

    impl GitRepo {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("create a directory");
            let root = directory.path().to_owned();
            let bare = root.join("origin.git");
            let seed = root.join("seed");
            let checkout = root.join("src/app");
            let mut repo = Self {
                checkout: checkout.clone(),
                pr: PullRequest {
                    repo: "acme/app".to_owned(),
                    number: 12,
                    title: String::new(),
                    url: String::new(),
                    head_sha: String::new(),
                    base_ref: "main".to_owned(),
                },
                directory,
            };
            let text = |path: &Path| path.to_str().expect("text").to_owned();
            repo.git(&root, &["init", "-q", "--bare", "-b", "main", &text(&bare)]);
            repo.git(&root, &["init", "-q", "-b", "main", &text(&seed)]);
            fs::write(seed.join("file.txt"), "base\n").expect("write the base");
            repo.git(&seed, &["add", "."]);
            repo.git(&seed, &["commit", "-q", "-m", "base"]);
            repo.git(&seed, &["push", "-q", &text(&bare), "main"]);
            fs::write(seed.join("file.txt"), "change\n").expect("write the change");
            repo.git(&seed, &["commit", "-q", "-am", "change"]);
            repo.git(
                &seed,
                &["push", "-q", &text(&bare), "HEAD:refs/pull/12/head"],
            );
            repo.pr.head_sha = repo.git(&seed, &["rev-parse", "HEAD"]);
            repo.git(&root, &["clone", "-q", &text(&bare), &text(&checkout)]);
            repo.git(
                &checkout,
                &["remote", "set-url", "origin", "git@github.com:acme/app.git"],
            );
            repo.git(
                &checkout,
                &[
                    "config",
                    &format!("url.file://{}.insteadOf", text(&bare)),
                    "git@github.com:acme/app.git",
                ],
            );
            repo
        }

        fn prepare(&self) -> Result<PathBuf, String> {
            let run = |call: &ToolCall<'_>| {
                let mut command =
                    ExternalCommand::new(call.program, call.timeout).arguments(&call.arguments);
                for (name, value) in self.environment() {
                    command = command.env(name, value);
                }
                let output = command.run(call.stop).map_err(|error| error.to_string())?;
                if output.code == 0 {
                    Ok(output.stdout)
                } else {
                    Err(String::from_utf8_lossy(&output.stderr).into_owned())
                }
            };
            let stop = StopRequest::default();
            let checkout = Checkout {
                run: &run,
                git: Path::new("git"),
                path: &self.checkout,
                stop: &stop,
            };
            let base = self.worktree_root().join("acme/app").join(format!(
                "pr-12-{}",
                self.pr.head_sha.get(..12).unwrap_or_default()
            ));
            prepare_worktree(&checkout, &base, &self.pr)
        }

        fn worktree_root(&self) -> PathBuf {
            self.directory.path().join("worktrees")
        }

        fn git(&self, directory: &Path, arguments: &[&str]) -> String {
            let output = self
                .command(directory, arguments)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        fn succeeds(&self, directory: &Path, arguments: &[&str]) -> bool {
            self.command(directory, arguments)
                .output()
                .expect("run git")
                .status
                .success()
        }

        fn command(&self, directory: &Path, arguments: &[&str]) -> Command {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(directory)
                .args(arguments)
                .envs(self.environment());
            command
        }

        // The owner's git config would otherwise reach the test, as hooks or signing.
        fn environment(&self) -> [(&'static str, OsString); 6] {
            [
                (
                    "GIT_CONFIG_GLOBAL",
                    self.directory.path().join("gitconfig").into_os_string(),
                ),
                ("GIT_CONFIG_NOSYSTEM", "1".into()),
                ("GIT_AUTHOR_NAME", "t".into()),
                ("GIT_AUTHOR_EMAIL", "t@example.com".into()),
                ("GIT_COMMITTER_NAME", "t".into()),
                ("GIT_COMMITTER_EMAIL", "t@example.com".into()),
            ]
        }
    }
}
