mod daemon;

use std::{
    cell::RefCell,
    env,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use jiff::{Timestamp, Zoned, tz::TimeZone};
use serde_json::Value;

use self::daemon::{Daemon, DaemonDeps};
use crate::{
    app::{
        review::{
            self, PullRequest,
            config::{Choice, ConfiguredCommands, ReviewConfig},
            notice::{Notice, Selection},
            one_line,
            request::request_queries,
            state::{Found, ReviewState, Status},
            with_link_step,
        },
        time,
        watch::{
            SendOutcome,
            worker::{self, Workers as _},
        },
    },
    infra::{
        alerter,
        codex_app::{new_chat_link, open_link},
        command::{RunTool, run_tool},
        gh::rest::GhRest,
        git::worktree::{Checkout, prepare_worktree},
        harnexus::{Bridge, CallTimeouts, CodexWorkers, call_socket_path},
        launchctl::agent_path,
        record::BaseDirectories,
        review_record::{self, ReviewChat, ReviewPaths},
        stop::StopRequest,
        watch_record::{self, WatchPaths},
    },
    runtime::{
        agent, output,
        termination::{self, ProgramWatch},
        watch,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewEnv {
    pub(crate) home: PathBuf,
    pub(crate) codex_home: PathBuf,
    // Names the per-user profile of nix-darwin, which holds commands launchd's PATH lacks.
    pub(crate) user: Option<String>,
    pub(crate) paths: ReviewPaths,
    // The watch's records, whose registered sender also sends to the review chats.
    pub(crate) watch_state: PathBuf,
}

// The commands the review runs, each an absolute path once resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Commands {
    gh: PathBuf,
    git: PathBuf,
    alerter: PathBuf,
    open: PathBuf,
    osascript: PathBuf,
}

struct Tools<'run> {
    run: &'run RunTool<'run>,
    send: &'run SendToChat<'run>,
    paths: ReviewPaths,
    commands: Commands,
    config: ReviewConfig,
}

type SendToChat<'send> = dyn Fn(&str, &str, &StopRequest) -> Delivery + Sync + 'send;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivery {
    Sent,
    NotSent(String),
    // The chat may have it, so it is neither sent again nor replaced with a new chat.
    Unknown(String),
}

// `pr-wake review smoke`: one PR through the notification and the launch, as a request would go.
pub(crate) struct Smoke<'smoke> {
    pub(crate) config: &'smoke Path,
    pub(crate) pr: &'smoke str,
    pub(crate) requester: Option<&'smoke str>,
    pub(crate) no_open: bool,
}

// What a launch got to before it ended, which the state keeps either way.
struct Launched {
    pr: Option<PullRequest>,
    worktree: Option<PathBuf>,
    // The linked chat the review was for, whether or not the send went through.
    chat: Option<String>,
    result: Result<(), String>,
}

// Well under the one second within which a wait must notice a stop request.
const STOP_CHECK: Duration = Duration::from_millis(200);
const STATUS_ROWS: usize = 20;

// Runs until a stop request, or until the program is replaced so that launchd starts the new one.
pub(crate) fn run(config: &Path, env: &ReviewEnv) -> Result<(), String> {
    let stop = StopRequest::default();
    termination::install(stop.clone())
        .map_err(|error| format!("cannot handle termination signals: {error}"))?;

    let send = harnexus_sender(env);
    let tools = load_tools(config, env, &run_tool, &send)?;
    let _lock = match review_record::hold_lock(&env.paths) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return Err(format!(
                "another pr-wake review is running ({})",
                env.paths.state.display()
            ));
        }
        Err(error) => return Err(error.to_string()),
    };
    let state = review_record::load_state(&env.paths)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();

    let program = env::current_exe().ok().and_then(ProgramWatch::new);
    if program.is_none() {
        log_line("cannot inspect the program, so a reinstall is not noticed");
    }
    log_line(&format!(
        "watching every {}s with {} choices",
        tools.config.poll_interval_seconds,
        tools.config.choices.len()
    ));

    let replaced = program
        .as_ref()
        .map(|program| move || program.is_replaced());
    let wait = |duration| pause(&stop, duration);
    let ended = Daemon::new(
        DaemonDeps {
            tools: &tools,
            paths: &env.paths,
            log: &log_line,
            now: &Timestamp::now,
            pause: &wait,
            stop: &stop,
            replaced: replaced
                .as_ref()
                .map(|replaced| replaced as &(dyn Fn() -> bool + Sync)),
        },
        state,
    )
    .run();
    log_line("stopped");
    ended
}

// Read from the state alone.
pub(crate) fn status(paths: &ReviewPaths, zone: &TimeZone) -> Result<String, String> {
    let state = review_record::load_state(paths).map_err(|error| error.to_string())?;
    Ok(state.map_or_else(
        || format!("no state yet: {}\n", paths.state_file().display()),
        |state| status_text(&state, zone),
    ))
}

// Each line of the report goes to `out` as it is known, so a later failure keeps what was found.
pub(crate) fn check(
    config: &Path,
    env: &ReviewEnv,
    run: &RunTool<'_>,
    stop: &StopRequest,
    out: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let send = harnexus_sender(env);
    let tools = load_tools(config, env, run, &send)?;
    let commands = &tools.commands;
    out(&format!("config: {}", config.display()));
    out(&format!(
        "commands: gh={} git={} alerter={} open={} osascript={}",
        commands.gh.display(),
        commands.git.display(),
        commands.alerter.display(),
        commands.open.display(),
        commands.osascript.display()
    ));
    out(&format!(
        "pollIntervalSeconds: {}",
        tools.config.poll_interval_seconds
    ));
    out(&format!(
        "worktreeRoot: {}",
        tools.config.worktree_root.display()
    ));
    for (repo, checkout) in &tools.config.repositories {
        out(&format!("repository: {repo} -> {}", checkout.display()));
    }
    for choice in &tools.config.choices {
        out(&format!(
            "choice: {} {:?} -> {}",
            choice.id,
            choice.label,
            choice.skill_path.display()
        ));
    }

    let github = github(&tools, stop);
    out(&format!("github login: {}", github.current_login()?));
    for query in request_queries(&tools.config.owners(), &tools.config.teams) {
        let hits = github.search_pull_requests(&query)?;
        out(&format!("pending ({query}): {}", hits.len()));
        if let Some(first) = hits.first() {
            github
                .review_requests(&first.repo, first.number)
                .map_err(|error| {
                    format!("issue events of {}#{}: {error}", first.repo, first.number)
                })?;
        }
    }
    Ok(())
}

pub(crate) fn smoke(
    smoke: &Smoke<'_>,
    env: &ReviewEnv,
    run: &RunTool<'_>,
    stop: &StopRequest,
    out: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let (repo, number) = review::parse_pr_ref(smoke.pr)?;
    let send = harnexus_sender(env);
    let tools = load_tools(smoke.config, env, run, &send)?;
    let pr = github(&tools, stop).pull_request(&repo, number)?;
    out(&format!(
        "pr: {}#{} {} head={}",
        pr.repo, pr.number, pr.title, pr.head_sha
    ));

    let request = Found {
        key: format!("smoke:{}#{}", pr.repo, pr.number),
        repo: pr.repo.clone(),
        number: pr.number,
        title: pr.title.clone(),
        requester: smoke.requester.map(str::to_owned),
        team: None,
    };
    let selection = alerter::notify(
        run,
        &tools.commands.alerter,
        &Notice::of(&request, &tools.config.choices),
        tools.config.notification_timeout_seconds,
        stop,
    )?;
    out(&format!("selection: {}", selection.name()));
    let Selection::Chosen(index) = selection else {
        return Ok(());
    };
    let choice = tools
        .config
        .choices
        .get(index)
        .ok_or_else(|| format!("no choice {index} in the config"))?;
    out(&format!("choice: {} ({})", choice.id, choice.skill));

    if smoke.no_open {
        let worktree = prepare(&tools, &pr, stop)?;
        let prompt = choice.render(&pr, smoke.requester, &worktree);
        out(&format!("worktree: {}", worktree.display()));
        match linked_chat(&tools, &pr)? {
            Some(chat) => {
                out(&format!("chat: {}", chat.thread));
                out(&format!("prompt: {prompt}"));
            }
            None => out(&format!(
                "link: {}",
                new_chat_link(&with_link_step(&prompt, &pr.url), &tools.config.chat_home)
            )),
        }
        return Ok(());
    }
    let launched = launch(&tools, choice, &pr.repo, pr.number, smoke.requester, stop);
    if let Some(worktree) = &launched.worktree {
        out(&format!("worktree: {}", worktree.display()));
    }
    launched.result?;
    match &launched.chat {
        Some(chat) => out(&format!("sent: the review went to the linked chat {chat}")),
        None => out("opened: send the prompt in the new Codex chat to start the review"),
    }
    Ok(())
}

// The config is checked before the running agent stops, so a bad config leaves it running.
pub(crate) fn install(env: &ReviewEnv) -> Result<(), String> {
    let config = &env.paths.config;
    if !config.exists() {
        return Err(format!(
            "{} not found; copy config.example.json from the pr-wake repository and edit it",
            config.display()
        ));
    }
    check_from_terminal(config, env)?;
    agent::remove_legacy(agent::LEGACY_REVIEW, &env.home)?;
    agent::install(agent::REVIEW, &env.home, env.user.as_deref(), &|name| {
        env::var_os(name)
    })
}

// Run in the review chat, which `CODEX_THREAD_ID` names.
pub(crate) fn link(
    env: &ReviewEnv,
    pr: &str,
    caller: Result<String, String>,
    now: Timestamp,
) -> Result<String, String> {
    let (repo, number) = review::parse_pr_ref(pr)?;
    let thread = caller?;
    if worker::is_t3_worker(&thread) {
        return Err(
            "pr-wake review link is for the Codex App chat the review opened, not a T3 Code thread"
                .to_owned(),
        );
    }
    review_record::record_chat(
        &env.paths,
        &ReviewChat {
            repo: repo.clone(),
            number,
            thread: thread.clone(),
            linked_at: time::format_timestamp(now),
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(format!(
        "linked {repo}#{number} to chat {thread}; its re-requests are sent here"
    ))
}

pub(crate) fn link_from_chat(env: &ReviewEnv, pr: &str) -> Result<(), String> {
    let caller = watch::caller_id(&|name| env::var_os(name)).map_err(|failure| failure.to_string());
    print_line(&link(env, pr, caller, Timestamp::now())?);
    Ok(())
}

pub(crate) fn show_status(env: &ReviewEnv) -> Result<(), String> {
    agent::print_loaded(agent::REVIEW);
    output::write_stdout(&status(&env.paths, &TimeZone::system())?);
    Ok(())
}

pub(crate) fn check_from_terminal(config: &Path, env: &ReviewEnv) -> Result<(), String> {
    let stop = stop_on_termination()?;
    check(config, env, &run_tool, &stop, &mut print_line)
}

pub(crate) fn smoke_from_terminal(trial: &Smoke<'_>, env: &ReviewEnv) -> Result<(), String> {
    let stop = stop_on_termination()?;
    smoke(trial, env, &run_tool, &stop, &mut print_line)
}

impl ReviewEnv {
    #[must_use]
    pub(crate) fn from_process() -> Option<Self> {
        let home = env::home_dir()?;
        let codex_home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".codex"));
        let user = agent::user();
        let directories = BaseDirectories::from_environment()?;
        Some(Self {
            home,
            codex_home,
            user,
            paths: ReviewPaths::of(&directories),
            watch_state: WatchPaths::of(&directories).state,
        })
    }
}

fn load_tools<'run>(
    config: &Path,
    env: &ReviewEnv,
    run: &'run RunTool<'run>,
    send: &'run SendToChat<'run>,
) -> Result<Tools<'run>, String> {
    let config = review_record::load_config(config, &env.home, &env.codex_home)?;
    let commands = resolve_commands(
        &config.commands,
        &agent_path(&env.home, env.user.as_deref()),
    )?;
    Ok(Tools {
        run,
        send,
        paths: env.paths.clone(),
        commands,
        config,
    })
}

fn harnexus_sender(env: &ReviewEnv) -> impl Fn(&str, &str, &StopRequest) -> Delivery + Sync {
    |thread, prompt, stop| {
        let socket = call_socket_path(
            env::var_os("HARNEXUS_CALL_SOCKET").as_deref(),
            env::var_os("HARNEXUS_STATE_PATH").as_deref(),
            Some(&env.home),
        );
        send_to_chat(&env.watch_state, socket, thread, prompt, stop)
    }
}

// Sent from the chat registered for the watch, as a chat cannot message itself.
fn send_to_chat(
    watch_state: &Path,
    socket: Result<PathBuf, String>,
    thread: &str,
    prompt: &str,
    stop: &StopRequest,
) -> Delivery {
    let registered = watch_record::load_sender(watch_state).map_err(|error| error.to_string());
    let sender = match worker::sender_for(registered, thread) {
        Ok(sender) => sender,
        Err(reason) => return Delivery::NotSent(reason),
    };

    let bridge = Bridge::new(socket, CallTimeouts::STANDARD, stop.clone());
    let message = RefCell::new(String::new());
    let mut codex = CodexWorkers::new(
        Box::new(|caller, tool, arguments| {
            let answer = bridge.call(caller, tool, &arguments);
            answer
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .clone_into(&mut message.borrow_mut());
            answer
        }),
        Box::new(|| Ok(None)),
    );
    let sent = codex.send(&sender, thread, prompt);
    let reason = format!("{}: {}", sent.reported, message.borrow());
    match sent.outcome {
        SendOutcome::Done => Delivery::Sent,
        SendOutcome::NotSent => Delivery::NotSent(reason),
        SendOutcome::Unknown(_) => Delivery::Unknown(reason),
    }
}

// Reads the PR again, so the worktree is pinned to the head at the time the owner chose.
fn launch(
    tools: &Tools<'_>,
    choice: &Choice,
    repo: &str,
    number: u64,
    requester: Option<&str>,
    stop: &StopRequest,
) -> Launched {
    let pr = match github(tools, stop).pull_request(repo, number) {
        Ok(pr) => pr,
        Err(error) => {
            return Launched {
                pr: None,
                worktree: None,
                chat: None,
                result: Err(error),
            };
        }
    };
    let worktree = match prepare(tools, &pr, stop) {
        Ok(worktree) => worktree,
        Err(error) => {
            return Launched {
                pr: Some(pr),
                worktree: None,
                chat: None,
                result: Err(error),
            };
        }
    };
    let prompt = choice.render(&pr, requester, &worktree);
    let ended = |chat, result| Launched {
        pr: Some(pr.clone()),
        worktree: Some(worktree.clone()),
        chat,
        result,
    };

    let linked = match linked_chat(tools, &pr) {
        Ok(linked) => linked,
        Err(error) => return ended(None, Err(error)),
    };
    // A linked PR stays in its chat, so a failed send is reported rather than split off into a new chat.
    if let Some(chat) = linked {
        let error = match (tools.send)(&chat.thread, &prompt, stop) {
            Delivery::Sent => return ended(Some(chat.thread), Ok(())),
            Delivery::NotSent(reason) => {
                format!("could not send to chat {}: {reason}", chat.thread)
            }
            Delivery::Unknown(reason) => format!(
                "the send to chat {} could not be confirmed; check that chat before asking it again: {reason}",
                chat.thread
            ),
        };
        return ended(Some(chat.thread), Err(error));
    }

    let link = new_chat_link(&with_link_step(&prompt, &pr.url), &tools.config.chat_home);
    let result = open_link(tools.run, &tools.commands.open, &link, stop)
        .map_err(|error| format!("open deep link: {error}"));
    ended(None, result)
}

fn linked_chat(tools: &Tools<'_>, pr: &PullRequest) -> Result<Option<ReviewChat>, String> {
    review_record::load_chat(&tools.paths, &pr.repo, pr.number)
        .map_err(|error| format!("read the linked review chat: {error}"))
}

fn prepare(tools: &Tools<'_>, pr: &PullRequest, stop: &StopRequest) -> Result<PathBuf, String> {
    let checkout = tools.config.checkout_of(&pr.repo).ok_or_else(|| {
        format!(
            "{} has no local checkout in repositories of the config",
            pr.repo
        )
    })?;
    prepare_worktree(
        &Checkout {
            run: tools.run,
            git: &tools.commands.git,
            path: checkout,
            stop,
        },
        &review::worktree_path(&tools.config.worktree_root, pr),
        pr,
    )
}

fn github<'tools>(tools: &'tools Tools<'_>, stop: &'tools StopRequest) -> GhRest<'tools> {
    GhRest {
        run: tools.run,
        gh: &tools.commands.gh,
        stop,
    }
}

fn status_text(state: &ReviewState, zone: &TimeZone) -> String {
    let local =
        |at: Timestamp, format: &str| at.to_zoned(zone.clone()).strftime(format).to_string();
    let mut lines = vec![
        format!("login: {}", state.login.as_deref().unwrap_or_default()),
        format!(
            "last poll: {}",
            state
                .last_poll
                .map_or_else(|| "never".to_owned(), |at| local(at, "%Y-%m-%d %H:%M:%S"))
        ),
    ];
    if let Some(error) = &state.last_error {
        lines.push(format!("last error: {error}"));
    }

    let mut requests = state
        .requests
        .values()
        .filter(|request| request.status != Status::Baseline)
        .collect::<Vec<_>>();
    requests.sort_by(|left, right| {
        (right.updated_at, &left.found.key).cmp(&(left.updated_at, &right.found.key))
    });
    for request in requests.iter().take(STATUS_ROWS) {
        let mut line = format!(
            "{}  {:<9} {}#{} {}",
            local(request.updated_at, "%m-%d %H:%M"),
            request.status.name(),
            request.found.repo,
            request.found.number,
            one_line(&request.found.title)
        );
        if let Some(choice) = &request.choice {
            line.push_str("  [");
            line.push_str(choice);
            line.push(']');
        }
        if let Some(error) = &request.error {
            line.push_str("  error: ");
            line.push_str(error);
        }
        lines.push(line);
    }
    if requests.len() > STATUS_ROWS {
        lines.push(format!("... {} more", requests.len() - STATUS_ROWS));
    }

    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn resolve_commands(
    configured: &ConfiguredCommands,
    directories: &[PathBuf],
) -> Result<Commands, String> {
    let resolve = |name: &str, configured: &Option<PathBuf>| {
        configured
            .clone()
            .map_or_else(|| find_command(name, directories), Ok)
    };
    Ok(Commands {
        gh: resolve("gh", &configured.gh)?,
        git: resolve("git", &configured.git)?,
        alerter: resolve("alerter", &configured.alerter)?,
        open: resolve("open", &configured.open)?,
        osascript: resolve("osascript", &configured.osascript)?,
    })
}

fn find_command(name: &str, directories: &[PathBuf]) -> Result<PathBuf, String> {
    directories
        .iter()
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| {
            let searched = directories
                .iter()
                .map(|directory| directory.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name} not found in {searched}; set commands.{name} in the config")
        })
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|found| found.is_file() && found.permissions().mode() & 0o111 != 0)
}

// Ctrl-C in the terminal stops what the command waits on, so no notification is left on screen.
fn stop_on_termination() -> Result<StopRequest, String> {
    let stop = StopRequest::default();
    termination::install(stop.clone())
        .map_err(|error| format!("cannot handle termination signals: {error}"))?;
    Ok(stop)
}

fn print_line(line: &str) {
    output::write_stdout(&format!("{line}\n"));
}

// launchd sends the agent's standard error to its log.
fn log_line(message: &str) {
    let at = Zoned::now().strftime("%Y/%m/%d %H:%M:%S").to_string();
    output::write_stderr(&format!("{at} {message}\n"));
}

fn pause(stop: &StopRequest, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    while !stop.is_requested() {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(STOP_CHECK));
    }
    false
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs, iter,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use super::*;
    use crate::{
        app::watch::pr::{PrLink, PrRef},
        infra::command::ToolCall,
    };

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    // A home with a review config whose commands are fakes answered by `answer`.
    struct ReviewHome {
        directory: tempfile::TempDir,
        env: ReviewEnv,
        declines: AtomicBool,
        calls: Mutex<Vec<String>>,
    }

    #[test]
    fn configured_command_wins_and_others_are_found_in_order() {
        let directory = tempfile::tempdir().expect("create a directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        for (folder, name, mode) in [
            (&first, "git", 0o644),
            (&second, "git", 0o755),
            (&first, "alerter", 0o755),
            (&second, "alerter", 0o755),
            (&second, "open", 0o755),
            (&second, "osascript", 0o755),
        ] {
            write_program(&folder.join(name), mode);
        }
        let configured = ConfiguredCommands {
            gh: Some(PathBuf::from("/opt/gh")),
            ..ConfiguredCommands::default()
        };

        let commands = resolve_commands(&configured, &[first.clone(), second.clone()]);

        assert_eq!(
            commands,
            Ok(Commands {
                gh: PathBuf::from("/opt/gh"),
                git: second.join("git"),
                alerter: first.join("alerter"),
                open: second.join("open"),
                osascript: second.join("osascript"),
            })
        );
    }

    #[test]
    fn missing_command_names_the_setting_that_replaces_the_lookup() {
        let directory = tempfile::tempdir().expect("create a directory");

        let commands = resolve_commands(
            &ConfiguredCommands::default(),
            &[directory.path().to_owned()],
        );

        let error = commands.expect_err("nothing is installed");
        assert!(error.contains("set commands.gh in the config"), "{error}");
    }

    #[test]
    fn status_lists_the_latest_requests_without_the_baseline() {
        let mut state = ReviewState::default();
        let at = |text: &str| text.parse::<Timestamp>().expect("parse the time");
        state.apply_poll(vec![found(1)], at("2026-11-12T00:00:00Z"), true);
        state.apply_poll(
            vec![found(1), found(2), found(3)],
            at("2026-11-12T01:00:00Z"),
            false,
        );
        let launched = state.requests.get_mut("acme/app#2@2").expect("request");
        launched.status = Status::Launched;
        launched.choice = Some("code".to_owned());
        launched.updated_at = at("2026-11-12T02:30:00Z");
        let failed = state.requests.get_mut("acme/app#3@3").expect("request");
        failed.status = Status::Failed;
        failed.error = Some("no checkout".to_owned());
        state.login = Some("me".to_owned());
        state.last_poll = Some(at("2026-11-12T02:31:05Z"));

        let text = status_text(&state, &TimeZone::fixed(jiff::tz::offset(9)));

        assert_eq!(
            text,
            "login: me\nlast poll: 2026-11-12 11:31:05\n11-12 11:30  launched  acme/app#2 PR 2  [code]\n11-12 10:00  failed    acme/app#3 PR 3  error: no checkout\n"
        );
    }

    #[test]
    fn status_without_a_state_says_where_it_would_be() {
        let directory = tempfile::tempdir().expect("create a directory");
        let paths = ReviewPaths {
            config: directory.path().join("review.json"),
            state: directory.path().join("review"),
        };

        let text = status(&paths, &TimeZone::UTC);

        assert_eq!(
            text,
            Ok(format!("no state yet: {}\n", paths.state_file().display()))
        );
    }

    #[test]
    fn status_shows_twenty_rows_and_counts_the_rest() {
        let mut state = ReviewState::default();
        state.apply_poll(
            (1..=22).map(found).collect(),
            "2026-11-12T01:00:00Z".parse().expect("parse the time"),
            false,
        );

        let text = status_text(&state, &TimeZone::UTC);

        assert_eq!(text.lines().count(), 2 + 20 + 1);
        assert_eq!(text.lines().last(), Some("... 2 more"));
        assert!(text.contains("last poll: never"), "{text}");
    }

    #[test]
    fn pause_ends_early_on_a_stop_request() {
        let stop = StopRequest::default();
        let started = Instant::now();

        let (completed, stopped) = thread::scope(|scope| {
            let waiter = scope.spawn(|| pause(&stop, Duration::from_secs(60)));
            thread::sleep(Duration::from_millis(50));
            stop.request();
            (
                pause(&StopRequest::default(), Duration::ZERO),
                waiter.join().expect("join"),
            )
        });

        assert!(completed);
        assert!(!stopped);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn check_reports_the_config_and_what_is_pending() {
        let home = ReviewHome::new();
        let mut lines = Vec::new();

        let checked = check(
            &home.env.paths.config,
            &home.env,
            &|call| home.answer(call, "[]"),
            &StopRequest::default(),
            &mut |line| lines.push(line.to_owned()),
        );

        assert_eq!(checked, Ok(()));
        let checkout = home.directory.path().join("src/app");
        let skill = home
            .directory
            .path()
            .join(".codex/skills/code-review/SKILL.md");
        assert_eq!(
            lines,
            [
                format!("config: {}", home.env.paths.config.display()),
                "commands: gh=/x/gh git=/x/git alerter=/x/alerter open=/x/open osascript=/x/osascript".to_owned(),
                "pollIntervalSeconds: 60".to_owned(),
                format!(
                    "worktreeRoot: {}",
                    home.directory.path().join("worktrees").display()
                ),
                format!("repository: acme/app -> {}", checkout.display()),
                format!("choice: code \"コードレビュー\" -> {}", skill.display()),
                "github login: me".to_owned(),
                "pending (is:pr is:open archived:false user-review-requested:@me user:acme): 1"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn check_that_cannot_read_the_events_names_the_pr_after_what_it_found() {
        let home = ReviewHome::new();
        let mut lines = Vec::new();

        let checked = check(
            &home.env.paths.config,
            &home.env,
            &|call| home.answer(call, "not json"),
            &StopRequest::default(),
            &mut |line| lines.push(line.to_owned()),
        );

        let error = checked.expect_err("the events are unreadable");
        assert!(error.starts_with("issue events of acme/app#3: "), "{error}");
        assert_eq!(
            lines.last().map(String::as_str),
            Some("pending (is:pr is:open archived:false user-review-requested:@me user:acme): 1")
        );
    }

    #[test]
    fn smoke_without_opening_prints_the_worktree_and_the_link() {
        let home = ReviewHome::new();
        let mut lines = Vec::new();
        let smoke_run = Smoke {
            config: &home.env.paths.config,
            pr: "https://github.com/acme/app/pull/3",
            requester: Some("tanaka"),
            no_open: true,
        };

        let smoked = smoke(
            &smoke_run,
            &home.env,
            &|call| home.answer(call, "[]"),
            &StopRequest::default(),
            &mut |line| lines.push(line.to_owned()),
        );

        assert_eq!(smoked, Ok(()));
        let worktree = home.directory.path().join(format!(
            "worktrees/acme/app/pr-3-{}",
            HEAD.get(..12).unwrap_or_default()
        ));
        let prompt = format!(
            "https://github.com/acme/app/pull/3 {} tanaka",
            worktree.display()
        );
        assert_eq!(
            lines,
            [
                format!("pr: acme/app#3 T head={HEAD}"),
                "selection: chosen".to_owned(),
                "choice: code (code-review)".to_owned(),
                format!("worktree: {}", worktree.display()),
                format!(
                    "link: {}",
                    new_chat_link(
                        &with_link_step(&prompt, "https://github.com/acme/app/pull/3"),
                        &home.directory.path().join("review-home")
                    )
                ),
            ]
        );
        assert!(!home.ran("/x/open"), "--no-open must not open the link");
    }

    #[test]
    fn link_records_the_calling_chat_apart_from_the_watch_of_the_pr() {
        let home = ReviewHome::new();
        let now = "2026-11-13T00:00:00Z".parse().expect("parse the time");
        let pr = PrRef {
            repo: "acme/app".to_owned(),
            number: 3,
        };
        let watched = PrLink {
            pr: pr.clone(),
            url: "https://github.com/acme/app/pull/3".to_owned(),
            worker: "worker-chat".to_owned(),
            linked_at: "2026-11-12T00:00:00.000Z".to_owned(),
        };
        watch_record::record_link(&home.env.watch_state, &watched).expect("link the worker");

        let linked = link(
            &home.env,
            "https://github.com/acme/app/pull/3",
            Ok("review-chat".to_owned()),
            now,
        );

        assert_eq!(
            linked,
            Ok("linked acme/app#3 to chat review-chat; its re-requests are sent here".to_owned())
        );
        assert_eq!(
            review_record::load_chat(&home.env.paths, "acme/app", 3)
                .expect("read the chat")
                .map(|chat| chat.thread),
            Some("review-chat".to_owned())
        );
        assert_eq!(
            watch_record::load_link(&home.env.watch_state, &pr).expect("read the watch link"),
            Some(watched)
        );
    }

    #[test]
    fn link_needs_a_codex_chat_and_a_pr() {
        for (name, pr, caller, expected) in [
            (
                "t3_thread",
                "acme/app#3",
                Ok("t3:thread".to_owned()),
                "not a T3 Code thread",
            ),
            (
                "no_chat",
                "acme/app#3",
                Err("CODEX_THREAD_ID is not set".to_owned()),
                "CODEX_THREAD_ID is not set",
            ),
            (
                "not_a_pr",
                "acme/app",
                Ok("review-chat".to_owned()),
                "expected owner/repo#number",
            ),
        ] {
            let home = ReviewHome::new();

            let linked = link(&home.env, pr, caller, Timestamp::UNIX_EPOCH);

            let error = linked.expect_err(name);
            assert!(error.contains(expected), "case: {name}: {error}");
            assert_eq!(
                review_record::load_chat(&home.env.paths, "acme/app", 3).expect("read no chat"),
                None,
                "case: {name}"
            );
        }
    }

    #[test]
    fn send_without_a_sender_or_a_bridge_is_not_sent() {
        for (name, sender, expected) in [
            ("no_sender", None, "no chat is registered to send from"),
            (
                "sender_is_the_chat",
                Some("review-chat"),
                "the registered sender is this chat itself",
            ),
            ("no_bridge", Some("sender-chat"), "not_sent: no socket"),
        ] {
            let directory = tempfile::tempdir().expect("create a directory");
            if let Some(sender) = sender {
                watch_record::record_sender(directory.path(), sender, "2026-11-13T00:00:00.000Z")
                    .expect("register the sender");
            }

            let delivery = send_to_chat(
                directory.path(),
                Err("no socket".to_owned()),
                "review-chat",
                "review again",
                &StopRequest::default(),
            );

            let Delivery::NotSent(reason) = delivery else {
                panic!("case: {name}: {delivery:?}");
            };
            assert!(reason.contains(expected), "case: {name}: {reason}");
        }
    }

    #[test]
    fn smoke_that_is_declined_stops_at_the_selection() {
        let home = ReviewHome::new();
        home.decline();
        let mut lines = Vec::new();
        let smoke_run = Smoke {
            config: &home.env.paths.config,
            pr: "acme/app#3",
            requester: None,
            no_open: false,
        };

        let smoked = smoke(
            &smoke_run,
            &home.env,
            &|call| home.answer(call, "[]"),
            &StopRequest::default(),
            &mut |line| lines.push(line.to_owned()),
        );

        assert_eq!(smoked, Ok(()));
        assert_eq!(
            lines,
            [
                format!("pr: acme/app#3 T head={HEAD}"),
                "selection: declined".to_owned()
            ]
        );
        assert!(!home.ran("/x/git"));
    }

    impl ReviewHome {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("create a home");
            let root = directory.path();
            let skill = root.join(".codex/skills/code-review/SKILL.md");
            fs::create_dir_all(skill.parent().expect("a parent")).expect("create the skill");
            fs::write(&skill, "# code-review").expect("write the skill");
            let config = root.join("config/pr-wake/review.json");
            fs::create_dir_all(config.parent().expect("a parent")).expect("create the config");
            fs::write(
                &config,
                serde_json::json!({
                    "repositories": {"acme/app": "~/src/app"},
                    "worktreeRoot": "~/worktrees",
                    "commands": {
                        "gh": "/x/gh",
                        "git": "/x/git",
                        "alerter": "/x/alerter",
                        "open": "/x/open",
                        "osascript": "/x/osascript",
                    },
                    "choices": [{
                        "id": "code",
                        "label": "コードレビュー",
                        "skill": "code-review",
                        "prompt": "{{.PR.URL}} {{.Worktree}} {{.Requester}}",
                    }],
                })
                .to_string(),
            )
            .expect("write the config");
            let env = ReviewEnv {
                home: root.to_owned(),
                codex_home: root.join(".codex"),
                user: None,
                paths: ReviewPaths {
                    config,
                    state: root.join("state/pr-wake/review"),
                },
                watch_state: root.join("state/harnexus-crew/watch"),
            };
            Self {
                directory,
                env,
                declines: AtomicBool::new(false),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn answer(&self, call: &ToolCall<'_>, events: &str) -> Result<Vec<u8>, String> {
            let line = iter::once(call.program.as_os_str())
                .chain(call.arguments.iter().map(OsString::as_os_str))
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().expect("note the call").push(line.clone());
            let checkout = self.directory.path().join("src/app").display().to_string();
            let answer = match line.as_str() {
                "/x/gh api user" => r#"{"login":"me"}"#.to_owned(),
                "/x/gh api repos/acme/app/pulls/3" => format!(
                    r#"{{"number":3,"title":"T","html_url":"https://github.com/acme/app/pull/3","head":{{"sha":"{HEAD}"}},"base":{{"ref":"main","repo":{{"full_name":"acme/app"}}}}}}"#
                ),
                "/x/gh api --paginate repos/acme/app/issues/3/events?per_page=100" => events.to_owned(),
                _ if line.starts_with("/x/gh api --paginate -X GET search/issues") => {
                    r#"{"incomplete_results":false,"items":[{"number":3,"title":"T","updated_at":"u","repository_url":"https://api.github.com/repos/acme/app"}]}"#.to_owned()
                }
                _ if line.starts_with("/x/alerter ") => {
                    if self.declines.load(Ordering::SeqCst) {
                        r#"{"activationType":"closed"}"#.to_owned()
                    } else {
                        r#"{"activationType":"actionClicked","activationValue":"コードレビュー"}"#.to_owned()
                    }
                }
                _ if line == format!("/x/git -C {checkout} config --get remote.origin.url") => {
                    "git@github.com:acme/app.git\n".to_owned()
                }
                _ if line.starts_with(&format!("/x/git -C {checkout} ")) => String::new(),
                _ => return Err(format!("unexpected command: {line}")),
            };
            Ok(answer.into_bytes())
        }

        fn decline(&self) {
            self.declines.store(true, Ordering::SeqCst);
        }

        fn ran(&self, prefix: &str) -> bool {
            self.calls
                .lock()
                .expect("read the calls")
                .iter()
                .any(|line| line.starts_with(prefix))
        }
    }

    fn found(number: u64) -> Found {
        Found {
            key: format!("acme/app#{number}@{number}"),
            repo: "acme/app".to_owned(),
            number,
            title: format!("PR {number}"),
            requester: None,
            team: None,
        }
    }

    fn write_program(path: &Path, mode: u32) {
        fs::create_dir_all(path.parent().expect("a parent")).expect("create the directory");
        fs::write(path, "#!/bin/sh\n").expect("write the program");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set the mode");
    }
}
