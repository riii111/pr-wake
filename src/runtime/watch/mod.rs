mod cycle;
mod log;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    env,
    ffi::OsString,
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use jiff::{Timestamp, Zoned};
use serde::Serialize;
use serde_json::{Map, Value};

use self::{
    cycle::{CycleDeps, Watch},
    log::{Entry, Log, StdoutLog},
};
use crate::{
    app::{
        time,
        watch::{
            self, WorkerView,
            classify::{self, Verdict},
            config::WatchConfig,
            events, message,
            pr::{self, GitHub, ItemKind, PrDetail, PrLink, PrRef, ReadError},
            state::{BotStatus, Delivered, FailedCheck, Finding, WatchState, WatchStatus},
            worker::{self as workers, Sent, T3_SENDER, T3_WORKER, Workers},
        },
    },
    infra::{
        command::ExternalCommand,
        gh::GhApi,
        harnexus::{Bridge, CallApp, CallTimeouts, CodexWorkers, call_socket_path},
        record::{BaseDirectories, RecordError, RecordErrorKind},
        stop::StopRequest,
        t3::{T3Ran, T3Workers, find_on_path, t3_binary},
        watch_record::{self, WatchPaths},
    },
    runtime::{
        Env, agent, output,
        termination::{self, ProgramWatch},
    },
};

// What `pr-wake watch` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Run,
    DryRun(Vec<String>),
    Release(String),
    // Lines for a person unless `json`, which a program such as harnexus-crew reads.
    Status { json: bool },
    Sender,
    Link { pr: String, take_over: bool },
}

// A failure's message is shown to whoever ran the command and may carry a path or what a command
// printed, so the log records the tag alone.
#[derive(Debug)]
pub(crate) enum Failure {
    Config(RecordError),
    State(RecordError),
    Github(ReadError),
    Refused(String),
    Replaced,
}

// A bridge call as one thread, answered with the bridge's outcome.
type BridgeCall<'call> = dyn Fn(&str, &str, &Map<String, Value>) -> Map<String, Value> + 'call;

// The workers of both apps, each linked worker reached through the app its link names.
pub(crate) struct AppWorkers<Codex, T3> {
    pub(crate) codex: Codex,
    pub(crate) t3: T3,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkOutput {
    thread_id: String,
    watched: String,
    // A linked chat nothing can send to, which the link alone would show as watched.
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

#[derive(Serialize)]
struct SenderOutput {
    sender: String,
}

#[derive(Serialize)]
struct ReleaseOutput {
    released: String,
    worker: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusOutput {
    sender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_error: Option<String>,
    prs: Vec<PrStatus>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum PrStatus {
    // Shown as such, as the watch logs and skips it.
    Unreadable {
        pr: String,
        worker: String,
        status: &'static str,
        reason: String,
    },
    Read(ReadStatus),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadStatus {
    pr: String,
    worker: String,
    status: WatchStatus,
    reason: Option<String>,
    pending: usize,
    follow_up: usize,
    awaiting_answer: bool,
    // The running watch has yet to hand the PR to the chat it is now linked to.
    handing_over: bool,
    checked_at: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DryRunOutput {
    dry_run: bool,
    prs: Vec<Report>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    pr: String,
    title: String,
    draft: bool,
    first_read: bool,
    head: String,
    bots: BTreeMap<String, Option<BotStatus>>,
    mergeable: String,
    failed_required: Vec<FailedCheck>,
    notified: Vec<Finding>,
    ignored: Vec<IgnoredReport>,
    prompt: Option<String>,
}

#[derive(Serialize)]
struct IgnoredReport {
    id: String,
    author: String,
    kind: ItemKind,
    reason: String,
    url: String,
}

const NOT_LINKABLE: &str =
    "the PR watch needs the URL https://github.com/<owner>/<repo>/pull/<number>";

const CLOUD_REFUSAL: &str = "watch needs the Codex App and is unavailable in Cloud; follow a PR from a Cloud session with that Cloud's own PR notifications";

const T3_THREAD_ENV: &str = "T3CODE_THREAD_ID";
const CODEX_THREAD_ENV: &str = "CODEX_THREAD_ID";
const EXECUTION_ENV: &str = "HARNEXUS_EXECUTION_ENV";

// A PR no watch has linked has no state, so it is shown as read for the first time and its link
// time is never used.
const EPOCH: &str = "1970-01-01T00:00:00.000Z";

// Well under the one second within which a wait must notice a stop request.
const STOP_CHECK: Duration = Duration::from_millis(200);

// A command prints its result as one JSON line; the watch itself prints only its log.
pub(crate) fn run(mode: Mode, env: Env<'_>) -> Result<Option<String>, Failure> {
    refuse_in_cloud(env)?;
    let paths = watch_paths()?;
    let now = Timestamp::now();
    match mode {
        Mode::Release(url) => json(&release(&paths.state, &url)?),
        Mode::Status { json: true } => json(&status(&paths.state)?),
        Mode::Status { json: false } => {
            agent::print_loaded(agent::WATCH);
            Ok(Some(status_text(&status(&paths.state)?)))
        }
        Mode::Sender => json(&register_sender(env, &paths.state, now)?),
        Mode::Link { pr, take_over } => json(&link(env, &paths.state, &pr, take_over, now)?),
        Mode::DryRun(urls) => {
            let stop = StopRequest::default();
            json(&dry_run(&paths, &mut gh_api(&stop), &urls)?)
        }
        Mode::Run => daemon(env, &paths).map(|()| None),
    }
}

// The config is checked with a dry run before the running agent stops, so a failed check leaves
// it running.
pub(crate) fn install(env: Env<'_>) -> Result<(), String> {
    let config = watch_paths().map_err(|failure| failure.to_string())?.config;
    if !config.exists() {
        return Err(format!(
            "{} not found; write it as the README shows",
            config.display()
        ));
    }
    output::write_stdout("$ pr-wake watch dry-run\n");
    run(Mode::DryRun(Vec::new()), env).map_err(|failure| failure.to_string())?;
    let home = home_of(env).ok_or("the home directory is unknown")?;
    agent::remove_legacy(agent::LEGACY_WATCH, &home)?;
    agent::install(agent::WATCH, &home, agent::user().as_deref(), env)
}

fn watch_paths() -> Result<WatchPaths, Failure> {
    BaseDirectories::from_environment()
        .map(|directories| WatchPaths::of(&directories))
        .ok_or_else(|| {
            Failure::Refused(
                "the home directory is unknown, so the watch has no records".to_owned(),
            )
        })
}

fn daemon(env: Env<'_>, paths: &WatchPaths) -> Result<(), Failure> {
    let stop = StopRequest::default();
    termination::install(stop.clone())
        .map_err(|error| Failure::Refused(format!("cannot handle termination signals: {error}")))?;
    let log = StdoutLog;
    let program = env::current_exe().ok().and_then(ProgramWatch::new);
    if program.is_none() {
        log.write(Entry::new("replace_check_off"));
    }
    // Said once at start, since without a registered chat no Codex worker is ever sent to.
    match watch_record::load_sender(&paths.state) {
        Ok(Some(_)) => {}
        Ok(None) => log.write(Entry::new("no_sender_registered")),
        Err(error) => {
            log.write(Entry::new("no_sender_registered").with("error", error.to_string()));
        }
    }
    let bridge = Bridge::new(socket_of(env), CallTimeouts::STANDARD, stop.clone());
    // Checked before the watch can send, so a harnexus without the expected protocol is never
    // written to.
    bridge.check_v1().map_err(Failure::Refused)?;

    let unsupported = RefCell::new(None);
    let call = |caller: &str, tool: &str, arguments: &Map<String, Value>| {
        bridge.call(caller, tool, arguments)
    };
    let mut workers = AppWorkers {
        codex: CodexWorkers::new(
            watched_call(&call, &unsupported, &log),
            Box::new(|| watch_record::load_sender(&paths.state).map_err(|error| error.to_string())),
        ),
        t3: T3Workers::new(Box::new(|arguments, input, timeout| {
            run_t3(env, arguments, input, timeout, &stop)
        })),
    };
    let mut github = gh_api(&stop);
    let replaced = program
        .as_ref()
        .map(|program| move || program.is_replaced());
    cycle::watch_forever(Watch {
        cycle: CycleDeps {
            paths,
            github: &mut github,
            workers: &mut workers,
            now: &Zoned::now,
            log: &log,
        },
        stop: &stop,
        pause: &mut |wait| pause(&stop, wait),
        replaced: replaced
            .as_ref()
            .map(|replaced| replaced as &dyn Fn() -> bool),
        ended: &|| ended_by(&unsupported),
    })
}

// A bridge replaced by an older one while the watch runs ends it between cycles, as at start; a
// closed app only waits.
fn watched_call<'call>(
    call: &'call BridgeCall<'call>,
    unsupported: &'call RefCell<Option<String>>,
    log: &'call dyn Log,
) -> CallApp<'call> {
    Box::new(move |caller, tool, arguments| {
        let answer = call(caller, tool, &arguments);
        let refused = answer
            .get("outcome")
            .is_some_and(|outcome| outcome == "unsupported");
        if refused && unsupported.borrow().is_none() {
            let message = answer
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unsupported")
                .to_owned();
            log.write(Entry::new("protocol_unsupported").with("message", message.as_str()));
            *unsupported.borrow_mut() = Some(message);
        }
        answer
    })
}

fn ended_by(unsupported: &RefCell<Option<String>>) -> Option<String> {
    unsupported
        .borrow()
        .as_ref()
        .map(|message| format!("cannot use the harnexus call socket: {message}"))
}

// The watch needs the Codex App on the same Mac, so a Cloud session is refused with the reason
// rather than a missing-socket hint.
fn refuse_in_cloud(env: Env<'_>) -> Result<(), Failure> {
    if let Some(explicit) = present(env(EXECUTION_ENV)) {
        return match explicit.to_str() {
            Some("local") => Ok(()),
            Some("cloud") => Err(Failure::Refused(CLOUD_REFUSAL.to_owned())),
            _ => Err(Failure::Refused(format!(
                "{EXECUTION_ENV} must be cloud or local, not \"{}\"",
                explicit.to_string_lossy()
            ))),
        };
    }
    let cloud = present(env("CODEX_ENVIRONMENT_ID")).is_some()
        || env("CLAUDE_CODE_REMOTE").is_some_and(|remote| remote == "true");
    if cloud {
        Err(Failure::Refused(CLOUD_REFUSAL.to_owned()))
    } else {
        Ok(())
    }
}

// T3 Code sets its thread for every agent and job it starts, and runs Codex there with a
// CODEX_THREAD_ID of Codex's own, so its variable decides first.
fn caller_id(env: Env<'_>) -> Result<String, Failure> {
    let (variable, value) = present(env(T3_THREAD_ENV)).map_or_else(
        || (CODEX_THREAD_ENV, present(env(CODEX_THREAD_ENV))),
        |t3| (T3_THREAD_ENV, Some(t3)),
    );
    let Some(value) = value else {
        return Err(Failure::Refused(format!(
            "{CODEX_THREAD_ENV} is not set; run pr-wake from a thread"
        )));
    };
    // A chat ID is pasted into socket requests and t3 arguments, so it may carry no whitespace or
    // control characters.
    let confirmed = value.to_str().filter(|value| {
        !value.starts_with("client-new-thread:")
            && !value
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '\u{feff}')
    });
    let Some(value) = confirmed else {
        return Err(Failure::Refused(format!(
            "{variable} must be a confirmed chat ID without whitespace"
        )));
    };
    Ok(if variable == T3_THREAD_ENV {
        format!("{T3_WORKER}{value}")
    } else {
        value.to_owned()
    })
}

// The calling chat becomes the one the watch sends every Codex message from.
fn register_sender(env: Env<'_>, root: &Path, now: Timestamp) -> Result<SenderOutput, Failure> {
    let thread = caller_id(env)?;
    if workers::is_t3_worker(&thread) {
        return Err(Failure::Refused(
            "pr-wake watch sender is for a Codex App chat; a T3 Code worker is sent to through the t3 CLI, without a sender".to_owned(),
        ));
    }
    watch_record::record_sender(root, &thread, &time::format_timestamp(now))
        .map_err(Failure::State)?;
    Ok(SenderOutput { sender: thread })
}

// Links the calling chat to a PR; only a take-over moves a PR another chat is watched for.
fn link(
    env: Env<'_>,
    root: &Path,
    value: &str,
    take_over: bool,
    now: Timestamp,
) -> Result<LinkOutput, Failure> {
    let worker = caller_id(env)?;
    let Some(named) = pr::github_pr(value) else {
        return Err(Failure::Refused(format!(
            "{value} is not watched: {NOT_LINKABLE}"
        )));
    };
    if !take_over
        && let Some(current) = watch_record::load_link(root, &named.pr).map_err(Failure::State)?
    {
        let state = watch_record::load_watch_state(root, &named.pr).map_err(Failure::State)?;
        if state.status.holds_chat() && current.worker != worker {
            return Err(Failure::Refused(format!(
                "{} is already watched for chat {}; link it with --take-over from the chat that takes it over",
                named.url, current.worker
            )));
        }
    }
    // Only the link is read, so an unreadable state never stops a chat from taking its PR over.
    let current = watch_record::load_link(root, &named.pr).map_err(Failure::State)?;
    // Linking the same PR again keeps its start, so comments made since are still reported.
    let linked_at = match current {
        Some(current) if current.worker == worker => current.linked_at,
        _ => time::format_timestamp(now),
    };
    watch_record::record_link(
        root,
        &PrLink {
            pr: named.pr,
            url: named.url.clone(),
            worker: worker.clone(),
            linked_at,
        },
    )
    .map_err(Failure::State)?;
    let sender = if workers::is_t3_worker(&worker) {
        Ok(T3_SENDER.to_owned())
    } else {
        let registered = watch_record::load_sender(root).map_err(|error| error.to_string());
        workers::sender_for(registered, &worker)
    };
    Ok(LinkOutput {
        thread_id: worker,
        watched: named.url,
        warning: sender.err(),
    })
}

// Lets a halted PR's watch go on, taking its halted message as delivered.
fn release(root: &Path, url: &str) -> Result<ReleaseOutput, Failure> {
    let Some(named) = pr::github_pr(url) else {
        return Err(Failure::Refused(
            "pr-wake watch release needs a GitHub pull request URL".to_owned(),
        ));
    };
    let link = find_link(root, &named.pr)?
        .ok_or_else(|| Failure::Refused(format!("{} is not linked to a worker", named.url)))?;
    let state = watch_record::load_watch_state(root, &link.pr).map_err(Failure::State)?;
    let Some(released) = watch::release(&state) else {
        return Err(Failure::Refused(format!("{} is not halted", named.url)));
    };
    watch_record::save_watch_state(root, &link.pr, &released).map_err(Failure::State)?;
    Ok(ReleaseOutput {
        released: named.url,
        worker: link.worker,
    })
}

// What each linked PR's watch is doing, read from the records alone whether or not a watch runs.
fn status(root: &Path) -> Result<StatusOutput, Failure> {
    let prs = watch_record::list_links(root, &HashSet::new())
        .map_err(Failure::State)?
        .into_iter()
        .map(
            |link| match watch_record::load_watch_state(root, &link.pr) {
                Err(error) => PrStatus::Unreadable {
                    pr: link.url,
                    worker: link.worker,
                    status: "unreadable",
                    reason: error.to_string(),
                },
                Ok(state) => PrStatus::Read(ReadStatus {
                    handing_over: state
                        .worker
                        .as_ref()
                        .is_some_and(|worker| *worker != link.worker),
                    pr: link.url,
                    worker: link.worker,
                    status: state.status,
                    reason: state.reason,
                    pending: state.pending.len(),
                    follow_up: state.follow_up.len(),
                    awaiting_answer: state.delivered.is_some(),
                    checked_at: state.checked_at,
                }),
            },
        )
        .collect();
    // An unreadable sender record is shown as such, like an unreadable PR record.
    Ok(match watch_record::load_sender(root) {
        Ok(sender) => StatusOutput {
            sender,
            sender_error: None,
            prs,
        },
        Err(error) => StatusOutput {
            sender: None,
            sender_error: Some(error.to_string()),
            prs,
        },
    })
}

// One line for each PR and one for the sender, as a person reads them.
fn status_text(status: &StatusOutput) -> String {
    let mut lines = status
        .prs
        .iter()
        .map(|pr| match pr {
            PrStatus::Unreadable {
                pr, status, reason, ..
            } => format!("{pr} {status} ({reason}) pending=0"),
            PrStatus::Read(read) => {
                let reason = read
                    .reason
                    .as_ref()
                    .map_or_else(String::new, |reason| format!(" ({reason})"));
                let awaiting = if read.awaiting_answer {
                    " awaiting answer"
                } else {
                    ""
                };
                format!(
                    "{} {}{reason} pending={}{awaiting}",
                    read.pr,
                    read.status.name(),
                    read.pending
                )
            }
        })
        .collect::<Vec<_>>();
    lines.push(match (&status.sender_error, &status.sender) {
        (Some(error), _) => format!("sender: unreadable ({error})"),
        (None, Some(sender)) => format!("sender: {sender}"),
        (None, None) => "sender: none; run `pr-wake watch sender` in a chat kept for it".to_owned(),
    });
    lines.join("\n")
}

// Decides from each PR's saved watch state what the watch would send, without writing any.
fn dry_run(
    paths: &WatchPaths,
    github: &mut dyn GitHub,
    urls: &[String],
) -> Result<DryRunOutput, Failure> {
    let config = watch_record::load_config(&paths.config).map_err(Failure::Config)?;
    let targets = dry_run_targets(github, &config, urls)?;
    let links = watch_record::list_links(&paths.state, &HashSet::new()).map_err(Failure::State)?;

    let mut reports = Vec::with_capacity(targets.len());
    for target in targets {
        let account = config.account_for(&target.repo).ok_or_else(|| {
            Failure::Refused(format!(
                "{} is not in the repositories of the watch config",
                target.repo
            ))
        })?;
        let link = links.iter().find(|link| link.pr.is_same(&target));
        let state = match link {
            Some(link) => {
                watch_record::load_watch_state(&paths.state, &link.pr).map_err(Failure::State)?
            }
            None => WatchState::default(),
        };
        let detail = github
            .detail(account, &target, &config.review_bots)
            .map_err(Failure::Github)?;
        let linked_at = link.map_or(EPOCH, |link| link.linked_at.as_str());
        reports.push(report_of(&detail, &state, linked_at, account, &config));
    }
    Ok(DryRunOutput {
        dry_run: true,
        prs: reports,
    })
}

// Without --pr, the open PRs each account wrote in the configured repositories.
fn dry_run_targets(
    github: &mut dyn GitHub,
    config: &WatchConfig,
    urls: &[String],
) -> Result<Vec<PrRef>, Failure> {
    if !urls.is_empty() {
        return urls
            .iter()
            .map(|url| {
                pr::github_pr(url).map(|named| named.pr).ok_or_else(|| {
                    Failure::Refused("--pr must be a GitHub pull request URL".to_owned())
                })
            })
            .collect();
    }
    let mut targets = Vec::new();
    for (repo, repository) in &config.repositories {
        targets.extend(
            github
                .open_pulls(&repository.account, repo)
                .map_err(Failure::Github)?,
        );
    }
    Ok(targets)
}

fn report_of(
    detail: &PrDetail,
    state: &WatchState,
    linked_at: &str,
    account: &str,
    config: &WatchConfig,
) -> Report {
    let verdicts = classify::classify(
        &detail.comments,
        account,
        &config.ignore,
        &config.review_bots,
    );
    let found = events::next_events(detail, state, linked_at, account, config);
    let mut notified = Vec::new();
    let mut ignored = Vec::new();
    for (verdict, comment) in verdicts.into_iter().zip(&detail.comments) {
        match verdict {
            Verdict::Notified(finding) => notified.push(finding),
            Verdict::Ignored(reason) => ignored.push(IgnoredReport {
                id: comment.id.clone(),
                author: comment.author.clone(),
                kind: comment.kind,
                reason: reason.to_string(),
                url: comment.url.clone(),
            }),
        }
    }
    let prompt = (!detail.summary.is_draft && !found.events.is_empty())
        .then(|| message::message_of(&detail.url, &found.events, config.instructions.as_deref()));
    Report {
        pr: detail.url.clone(),
        title: detail.title.clone(),
        draft: detail.summary.is_draft,
        first_read: state.summary.is_none(),
        head: detail.summary.head.clone(),
        bots: detail.summary.bots.clone(),
        mergeable: detail.summary.mergeable.clone(),
        failed_required: detail.failed_required.clone(),
        notified,
        ignored,
        prompt,
    }
}

fn find_link(root: &Path, pr: &PrRef) -> Result<Option<PrLink>, Failure> {
    Ok(watch_record::list_links(root, &HashSet::new())
        .map_err(Failure::State)?
        .into_iter()
        .find(|link| link.pr.is_same(pr)))
}

fn gh_api(stop: &StopRequest) -> GhApi<'_> {
    GhApi::new(
        Box::new(move |call| {
            let mut command =
                ExternalCommand::new("gh", call.timeout).arguments(call.arguments.iter().copied());
            if let Some(token) = call.token {
                command = command.env("GH_TOKEN", token);
            }
            if let Some(input) = call.input {
                command = command.input(input);
            }
            command.run(stop)
        }),
        Box::new(Instant::now),
    )
}

fn run_t3(
    env: Env<'_>,
    arguments: &[&str],
    input: &str,
    timeout: Duration,
    stop: &StopRequest,
) -> T3Ran {
    let binary = t3_binary(
        env("HARNEXUS_T3_BIN").as_deref(),
        env("T3CODE_HOME").as_deref(),
        &home_of(env).unwrap_or_default(),
        |name| find_on_path(name, env("PATH").as_deref()),
    );
    let Some(binary) = binary else {
        return T3Ran::NotRan {
            started: false,
            message: "HARNEXUS_T3_BIN must be an absolute path".to_owned(),
        };
    };
    match ExternalCommand::new(binary, timeout)
        .arguments(arguments.iter().copied())
        .input(input)
        .run(stop)
    {
        Ok(output) => T3Ran::Ran {
            code: output.code,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => T3Ran::NotRan {
            started: error.started(),
            message: error.to_string(),
        },
    }
}

fn socket_of(env: Env<'_>) -> Result<PathBuf, String> {
    call_socket_path(
        env("HARNEXUS_CALL_SOCKET").as_deref(),
        env("HARNEXUS_STATE_PATH").as_deref(),
        home_of(env).as_deref(),
    )
}

fn home_of(env: Env<'_>) -> Option<PathBuf> {
    present(env("HOME"))
        .map(PathBuf::from)
        .or_else(env::home_dir)
}

fn pause(stop: &StopRequest, wait: Duration) {
    let until = Instant::now() + wait;
    while !stop.is_requested() {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(STOP_CHECK));
    }
}

fn present(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn json(output: &impl Serialize) -> Result<Option<String>, Failure> {
    serde_json::to_string(output)
        .map(Some)
        .map_err(|error| Failure::Refused(format!("cannot write the result: {error}")))
}

impl<Codex: Workers, T3: Workers> AppWorkers<Codex, T3> {
    fn of(&mut self, worker: &str) -> &mut dyn Workers {
        if workers::is_t3_worker(worker) {
            &mut self.t3
        } else {
            &mut self.codex
        }
    }
}

impl<Codex: Workers, T3: Workers> Workers for AppWorkers<Codex, T3> {
    fn sender_of(&mut self, worker: &str) -> Result<String, String> {
        self.of(worker).sender_of(worker)
    }

    fn look(
        &mut self,
        sender: &str,
        worker: &str,
        awaited: Option<&Delivered>,
        now: Timestamp,
    ) -> Option<WorkerView> {
        self.of(worker).look(sender, worker, awaited, now)
    }

    fn send(&mut self, sender: &str, worker: &str, prompt: &str) -> Sent {
        self.of(worker).send(sender, worker, prompt)
    }
}

impl Failure {
    // The names the TypeScript watch logged, so a search over old and new logs finds both.
    #[must_use]
    pub(crate) const fn tag(&self) -> &'static str {
        match self {
            Self::Config(RecordError {
                kind: RecordErrorKind::Invalid(_),
                ..
            }) => "ConfigInvalid",
            Self::Config(_) => "ConfigUnreadable",
            Self::State(RecordError {
                kind: RecordErrorKind::Write(_) | RecordErrorKind::Lock(_),
                ..
            }) => "StateUnwritable",
            Self::State(_) => "StateUnreadable",
            Self::Github(error) => error.kind.tag(),
            Self::Refused(_) => "WatchRefused",
            Self::Replaced => "WatchReplaced",
        }
    }
}

impl Display for Failure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) | Self::State(error) => error.fmt(formatter),
            Self::Refused(message) => formatter.write_str(message),
            Self::Github(error) => error.fmt(formatter),
            Self::Replaced => {
                formatter.write_str("the watch's program was replaced; exiting to be restarted")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs};

    use jiff::tz::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::app::watch::{
        pr::PrComment,
        reviewers::ReviewBot,
        state::{PrSummary, WatchEvent},
    };

    const PR: &str = "https://github.com/org/pay/pull/7";
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    // GitHub as the dry run reads it, noting each request.
    struct FakeGitHub<'asked> {
        asked: &'asked RefCell<Vec<String>>,
        detail: PrDetail,
    }

    struct Named<'asked> {
        app: &'static str,
        asked: &'asked RefCell<Vec<String>>,
    }

    mod dry_run {
        use super::*;

        #[test]
        fn verdicts_and_message_are_reported_for_every_open_pr_without_writing_its_state() {
            let records = Records::new();
            watch_record::record_link(&records.paths.state, &link_of(7, "worker"))
                .expect("record the link");
            let read = WatchState {
                summary: Some(summary()),
                seen: vec!["bot:coderabbit".to_owned()],
                ..WatchState::default()
            };
            watch_record::save_watch_state(&records.paths.state, &pay(7), &read)
                .expect("save the state");
            let asked = RefCell::new(Vec::new());

            let ran = dry_run(&records.paths, &mut github(&asked), &[]);

            let report = serde_json::to_value(ran.expect("dry run")).expect("write the report");
            assert_eq!(*asked.borrow(), ["open owner org/pay", "owner org/pay#7"]);
            assert_eq!(
                watch_record::load_watch_state(&records.paths.state, &pay(7)).expect("load"),
                read
            );
            assert_eq!(report["dryRun"], true);
            assert_eq!(report["prs"][0]["firstRead"], false);
            assert_eq!(
                report["prs"][0]["ignored"],
                json!([{
                    "id": "issue:2",
                    "author": "coderabbitai[bot]",
                    "kind": "issue_comment",
                    "reason": "ignore rule",
                    "url": format!("{PR}#issuecomment-2"),
                }])
            );
            let prompt = report["prs"][0]["prompt"].as_str().unwrap_or_default();
            assert!(
                prompt.contains(&format!("1. [CodeRabbit・Minor] a.kt:3 {PR}#discussion_r1")),
                "{prompt}"
            );
        }

        #[test]
        fn pr_without_watch_state_is_a_first_read_with_no_message_and_no_state_written() {
            for (name, linked) in [("linked_without_state", true), ("never_linked", false)] {
                let records = Records::new();
                if linked {
                    watch_record::record_link(&records.paths.state, &link_of(7, "worker"))
                        .expect("record the link");
                }
                let asked = RefCell::new(Vec::new());

                let ran = dry_run(&records.paths, &mut github(&asked), &[PR.to_owned()]);

                let report = serde_json::to_value(ran.expect("dry run")).expect("write it");
                let shown = &report["prs"][0];
                assert_eq!(
                    (&shown["firstRead"], &shown["prompt"]),
                    (&json!(true), &Value::Null),
                    "case: {name}"
                );
                assert_eq!(
                    shown["notified"].as_array().map(Vec::len),
                    Some(1),
                    "case: {name}"
                );
                assert_eq!(
                    watch_record::load_watch_state(&records.paths.state, &pay(7)).expect("load"),
                    WatchState::default(),
                    "case: {name}"
                );
                assert!(
                    !records
                        .paths
                        .state
                        .join(watch_record::pr_key(&pay(7)))
                        .join("state.json")
                        .exists(),
                    "case: {name}"
                );
            }
        }

        #[test]
        fn only_the_given_prs_are_read_and_a_repository_outside_the_config_refused() {
            let records = Records::new();
            let asked = RefCell::new(Vec::new());

            let given = dry_run(
                &records.paths,
                &mut github(&asked),
                &[format!("{PR}/files")],
            );
            let outside = dry_run(
                &records.paths,
                &mut github(&asked),
                &["https://github.com/org/other/pull/1".to_owned()],
            );

            assert!(given.is_ok(), "{:?}", given.err());
            assert_eq!(*asked.borrow(), ["owner org/pay#7"]);
            assert!(outside.is_err_and(|failure| {
                failure
                    .to_string()
                    .contains("org/other is not in the repositories")
            }),);
        }

        #[test]
        fn pr_flag_that_is_not_a_github_pr_is_refused() {
            let records = Records::new();
            let asked = RefCell::new(Vec::new());

            let refused = dry_run(&records.paths, &mut github(&asked), &["9".to_owned()]);

            assert!(refused.is_err_and(
                |failure| failure.to_string() == "--pr must be a GitHub pull request URL"
            ));
            assert!(asked.borrow().is_empty());
        }
    }

    mod release {
        use super::*;

        #[test]
        fn halted_pr_resumes_with_its_message_taken_as_delivered() {
            let records = Records::new();
            let root = &records.paths.state;
            watch_record::record_link(root, &link_of(7, "worker")).expect("record the link");
            let halted = WatchState {
                status: WatchStatus::Halted,
                pending: vec![WatchEvent::Conflict {
                    head: "a".to_owned(),
                }],
                halted: Some(Map::from_iter([("prompt".to_owned(), json!("x"))])),
                ..WatchState::default()
            };
            watch_record::save_watch_state(root, &pay(7), &halted).expect("save the state");

            let released = release(root, PR).expect("release");
            let again = release(root, PR);

            let state = watch_record::load_watch_state(root, &pay(7)).expect("load");
            assert_eq!(
                serde_json::to_value(released).expect("write it"),
                json!({ "released": PR, "worker": "worker" })
            );
            assert_eq!(
                (state.status, state.pending, state.halted),
                (WatchStatus::Watching, Vec::new(), None)
            );
            assert!(again.is_err_and(|failure| failure.to_string().contains("is not halted")));
        }

        #[test]
        fn pr_that_is_not_linked_or_not_a_pr_is_refused() {
            let records = Records::new();

            let unlinked = release(&records.paths.state, PR);
            let not_pr = release(&records.paths.state, "7");

            assert!(unlinked.is_err_and(
                |failure| failure.to_string() == format!("{PR} is not linked to a worker")
            ));
            assert!(not_pr.is_err_and(|failure| failure.to_string()
                == "pr-wake watch release needs a GitHub pull request URL"));
        }
    }

    mod status {
        use super::*;

        #[test]
        fn each_linked_pr_is_listed_with_what_its_watch_is_doing() {
            let records = Records::new();
            let root = &records.paths.state;
            watch_record::record_link(root, &link_of(7, "worker")).expect("record the link");
            let state = WatchState {
                status: WatchStatus::Halted,
                reason: Some("x".to_owned()),
                pending: vec![WatchEvent::Conflict {
                    head: "a".to_owned(),
                }],
                ..WatchState::default()
            };
            watch_record::save_watch_state(root, &pay(7), &state).expect("save the state");

            let shown = status(root).expect("status");

            // In the order the TypeScript watch printed, so old and new outputs compare line by line.
            assert_eq!(
                serde_json::to_string(&shown).expect("write it"),
                r#"{"sender":null,"prs":[{"pr":"https://github.com/org/pay/pull/7","worker":"worker","status":"halted","reason":"x","pending":1,"followUp":0,"awaitingAnswer":false,"handingOver":false,"checkedAt":null}]}"#
            );
        }

        #[test]
        fn unreadable_record_is_shown_as_such_and_the_others_still_listed() {
            let records = Records::new();
            let root = &records.paths.state;
            watch_record::record_link(root, &link_of(7, "worker")).expect("record the link");
            watch_record::record_link(root, &link_of(8, "worker")).expect("record the link");
            fs::write(
                root.join(watch_record::pr_key(&pay(7))).join("state.json"),
                "{",
            )
            .expect("damage the state");
            fs::write(root.join("sender.json"), "{").expect("damage the sender");

            let shown = serde_json::to_value(status(root).expect("status")).expect("write it");

            let mut statuses = shown["prs"]
                .as_array()
                .expect("the PRs")
                .iter()
                .map(|pr| pr["status"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>();
            statuses.sort();
            assert_eq!(statuses, ["unreadable", "watching"]);
            assert_eq!(shown["sender"], Value::Null);
            assert!(shown["senderError"].is_string(), "{shown}");
        }

        #[test]
        fn text_has_a_line_for_each_pr_and_one_for_the_sender() {
            let no_sender = "sender: none; run `pr-wake watch sender` in a chat kept for it";
            for (name, shown, expected) in [
                (
                    "watched_prs",
                    StatusOutput {
                        sender: None,
                        sender_error: None,
                        prs: vec![
                            read(7, WatchStatus::Watching, None, 2, true),
                            read(
                                8,
                                WatchStatus::Stopped,
                                Some("worker thread is gone"),
                                0,
                                false,
                            ),
                        ],
                    },
                    vec![
                        "https://github.com/org/pay/pull/7 watching pending=2 awaiting answer",
                        "https://github.com/org/pay/pull/8 stopped (worker thread is gone) pending=0",
                        no_sender,
                    ],
                ),
                (
                    "unreadable_pr",
                    StatusOutput {
                        sender: Some("chat-1".to_owned()),
                        sender_error: None,
                        prs: vec![PrStatus::Unreadable {
                            pr: PR.to_owned(),
                            worker: "w7".to_owned(),
                            status: "unreadable",
                            reason: "bad JSON".to_owned(),
                        }],
                    },
                    vec![
                        "https://github.com/org/pay/pull/7 unreadable (bad JSON) pending=0",
                        "sender: chat-1",
                    ],
                ),
                (
                    "unreadable_sender",
                    StatusOutput {
                        sender: None,
                        sender_error: Some("bad JSON".to_owned()),
                        prs: Vec::new(),
                    },
                    vec!["sender: unreadable (bad JSON)"],
                ),
            ] {
                let text = status_text(&shown);

                assert_eq!(text, expected.join("\n"), "case: {name}");
            }
        }

        fn read(
            number: u64,
            status: WatchStatus,
            reason: Option<&str>,
            pending: usize,
            awaiting_answer: bool,
        ) -> PrStatus {
            PrStatus::Read(ReadStatus {
                pr: format!("https://github.com/org/pay/pull/{number}"),
                worker: format!("w{number}"),
                status,
                reason: reason.map(str::to_owned),
                pending,
                follow_up: 0,
                awaiting_answer,
                handing_over: false,
                checked_at: None,
            })
        }
    }

    // The records harnexus-task watch left on a Mac, copied before each test.
    mod legacy_records {
        use super::*;

        #[test]
        fn watch_ends_before_writing_once_the_bridge_stops_offering_protocol_1() {
            let records = Records::legacy();
            let refusals = Cell::new(0);
            let unsupported = RefCell::new(None);
            let log = Recorded::default();
            // The bridge reads threads, then stops offering protocol 1, so the client writes no
            // send to it and answers each with its own unsupported outcome.
            let bridge = |_: &str, tool: &str, arguments: &Map<String, Value>| {
                if tool == "read_thread" {
                    return idle_thread(arguments);
                }
                refusals.set(refusals.get() + 1);
                Map::from_iter([
                    ("outcome".to_owned(), json!("unsupported")),
                    ("message".to_owned(), json!("old bridge")),
                ])
            };
            let asked = RefCell::new(Vec::new());
            let mut github = legacy_github(&asked);
            let mut workers = AppWorkers {
                codex: CodexWorkers::new(
                    watched_call(&bridge, &unsupported, &log),
                    Box::new(|| Ok(Some("sender-chat".to_owned()))),
                ),
                t3: T3Workers::new(Box::new(|_, _, _| T3Ran::NotRan {
                    started: false,
                    message: "no t3".to_owned(),
                })),
            };
            let now = || noon();
            let stop = StopRequest::default();

            let ended = cycle::watch_forever(Watch {
                cycle: CycleDeps {
                    paths: &records.paths,
                    github: &mut github,
                    workers: &mut workers,
                    now: &now,
                    log: &log,
                },
                stop: &stop,
                pause: &mut |_| {},
                replaced: None,
                ended: &|| ended_by(&unsupported),
            });

            assert!(
                matches!(&ended, Err(Failure::Refused(reason)) if reason == "cannot use the harnexus call socket: old bridge"),
                "{ended:?}"
            );
            assert_eq!(refusals.get(), 2);
            let root = &records.paths.state;
            let queued = watch_record::load_watch_state(root, &pay(9)).expect("load PR 9");
            assert_eq!(
                (queued.status, queued.pending.len()),
                (WatchStatus::Watching, 1)
            );
            let handed = watch_record::load_watch_state(root, &pay(7)).expect("load PR 7");
            assert_eq!(handed.status, WatchStatus::Watching);
            let logged = log.0.borrow();
            let events = logged.iter().map(|entry| entry.event).collect::<Vec<_>>();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == "protocol_unsupported")
                    .count(),
                1
            );
            assert_eq!(events.last(), Some(&"ended_watch"));
        }

        #[test]
        fn only_one_watch_writes_them_while_the_lock_is_held() {
            let records = Records::legacy();
            let _held = watch_record::hold_watch_lock(&records.paths.state)
                .expect("lock")
                .expect("the lock is free");
            let asked = RefCell::new(Vec::new());
            let sent = Cell::new(0);
            let bridge = |_: &str, tool: &str, arguments: &Map<String, Value>| {
                if tool == "send_message_to_thread" {
                    sent.set(sent.get() + 1);
                }
                idle_thread(arguments)
            };
            let unsupported = RefCell::new(None);
            let log = Recorded::default();
            let mut workers = CodexWorkers::new(
                watched_call(&bridge, &unsupported, &log),
                Box::new(|| Ok(Some("sender-chat".to_owned()))),
            );
            let now = || noon();

            let ended = cycle::watch_forever(Watch {
                cycle: CycleDeps {
                    paths: &records.paths,
                    github: &mut legacy_github(&asked),
                    workers: &mut workers,
                    now: &now,
                    log: &log,
                },
                stop: &StopRequest::default(),
                pause: &mut |_| {},
                replaced: None,
                ended: &|| None,
            });

            assert!(
                matches!(&ended, Err(Failure::Refused(reason)) if reason == "another PR watch is running"),
                "{ended:?}"
            );
            assert_eq!(sent.get(), 0);
            assert!(asked.borrow().is_empty());
        }
    }

    #[test]
    fn t3_worker_is_reached_through_t3_code_and_any_other_through_the_codex_app() {
        let asked = RefCell::new(Vec::new());
        let mut workers = AppWorkers {
            codex: Named {
                app: "codex",
                asked: &asked,
            },
            t3: Named {
                app: "t3",
                asked: &asked,
            },
        };

        let senders = [workers.sender_of("t3:w1"), workers.sender_of("019a-codex")];

        assert_eq!(*asked.borrow(), ["t3 t3:w1", "codex 019a-codex"]);
        assert_eq!(senders, [Ok("t3".to_owned()), Ok("codex".to_owned())]);
    }

    #[test]
    fn caller_is_the_t3_thread_first_and_must_be_a_confirmed_chat_id() {
        for (name, t3, codex, expected) in [
            ("codex", None, Some("019a"), Ok("019a")),
            (
                "t3_over_codex",
                Some("mcp:w1"),
                Some("019a"),
                Ok("t3:mcp:w1"),
            ),
            ("empty_t3", Some(""), Some("019a"), Ok("019a")),
            (
                "none",
                None,
                None,
                Err("CODEX_THREAD_ID is not set; run pr-wake from a thread"),
            ),
            (
                "space",
                None,
                Some("a b"),
                Err("CODEX_THREAD_ID must be a confirmed chat ID without whitespace"),
            ),
            (
                "control",
                Some("a\u{7}"),
                None,
                Err("T3CODE_THREAD_ID must be a confirmed chat ID without whitespace"),
            ),
            (
                "byte_order_mark",
                None,
                Some("a\u{feff}"),
                Err("CODEX_THREAD_ID must be a confirmed chat ID without whitespace"),
            ),
            (
                "unconfirmed",
                None,
                Some("client-new-thread:1"),
                Err("CODEX_THREAD_ID must be a confirmed chat ID without whitespace"),
            ),
        ] {
            let env = |variable: &str| match variable {
                T3_THREAD_ENV => t3.map(OsString::from),
                CODEX_THREAD_ENV => codex.map(OsString::from),
                _ => None,
            };

            let caller = caller_id(&env).map_err(|failure| failure.to_string());

            assert_eq!(
                caller,
                expected.map(str::to_owned).map_err(str::to_owned),
                "case: {name}"
            );
        }
    }

    #[test]
    fn cloud_session_is_refused_and_a_local_one_let_through() {
        for (name, variables, refused) in [
            (
                "explicit_local",
                vec![(EXECUTION_ENV, "local"), ("CLAUDE_CODE_REMOTE", "true")],
                None,
            ),
            (
                "explicit_cloud",
                vec![(EXECUTION_ENV, "cloud")],
                Some(CLOUD_REFUSAL),
            ),
            (
                "claude_remote",
                vec![("CLAUDE_CODE_REMOTE", "true")],
                Some(CLOUD_REFUSAL),
            ),
            (
                "codex_environment",
                vec![("CODEX_ENVIRONMENT_ID", "env")],
                Some(CLOUD_REFUSAL),
            ),
            (
                "nothing_set",
                vec![(EXECUTION_ENV, ""), ("CLAUDE_CODE_REMOTE", "false")],
                None,
            ),
            (
                "unknown_value",
                vec![(EXECUTION_ENV, "mac")],
                Some("HARNEXUS_EXECUTION_ENV must be cloud or local, not \"mac\""),
            ),
        ] {
            let env = |variable: &str| {
                variables
                    .iter()
                    .find(|(name, _)| *name == variable)
                    .map(|(_, value)| OsString::from(value))
            };

            let checked = refuse_in_cloud(&env).map_err(|failure| failure.to_string());

            assert_eq!(
                checked,
                refused.map_or(Ok(()), |reason| Err(reason.to_owned())),
                "case: {name}"
            );
        }
    }

    #[test]
    fn failure_tags_name_what_failed_as_the_typescript_watch_did() {
        let records = Records::new();
        let missing = watch_record::load_config(&records.paths.state.join("missing.json"))
            .expect_err("no config");
        fs::write(records.paths.state.join("bad.json"), "{").expect("write a broken file");
        let invalid =
            watch_record::load_config(&records.paths.state.join("bad.json")).expect_err("broken");

        assert_eq!(Failure::Config(missing).tag(), "ConfigUnreadable");
        assert_eq!(Failure::Config(invalid).tag(), "ConfigInvalid");
        assert_eq!(Failure::Replaced.tag(), "WatchReplaced");
        assert_eq!(Failure::Refused(String::new()).tag(), "WatchRefused");
    }

    // A temporary home for the config and the records.
    struct Records {
        _root: tempfile::TempDir,
        paths: WatchPaths,
    }

    #[derive(Default)]
    struct Recorded(RefCell<Vec<Entry>>);

    impl Records {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("create a directory");
            let paths = WatchPaths {
                config: root.path().join("pr-watch.json"),
                state: root.path().join("watch"),
            };
            fs::create_dir_all(&paths.state).expect("create the record directory");
            fs::write(
                &paths.config,
                json!({
                    "repositories": { "Org/Pay": { "account": "owner" } },
                    "ignore": [{ "author": "coderabbitai[bot]", "kind": "issue_comment" }],
                })
                .to_string(),
            )
            .expect("write the config");
            Self { _root: root, paths }
        }

        fn legacy() -> Self {
            let records = Self::new();
            fs::write(
                &records.paths.config,
                json!({
                    "repositories": { "org/pay": { "account": "owner" } },
                    "quietHours": null,
                    "instructions": "$task-worker",
                })
                .to_string(),
            )
            .expect("write the config");
            copy_directory(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-watch"),
                &records.paths.state,
            );
            records
        }
    }

    impl Log for Recorded {
        fn write(&self, entry: Entry) {
            self.0.borrow_mut().push(entry);
        }
    }

    impl GitHub for FakeGitHub<'_> {
        fn summaries(
            &mut self,
            _account: &str,
            prs: &[PrRef],
            _bots: &[ReviewBot],
        ) -> Result<Vec<Option<PrSummary>>, ReadError> {
            Ok(prs
                .iter()
                .map(|_| Some(self.detail.summary.clone()))
                .collect())
        }

        fn detail(
            &mut self,
            account: &str,
            pr: &PrRef,
            _bots: &[ReviewBot],
        ) -> Result<PrDetail, ReadError> {
            self.asked
                .borrow_mut()
                .push(format!("{account} {}#{}", pr.repo, pr.number));
            Ok(PrDetail {
                url: format!("https://github.com/org/pay/pull/{}", pr.number),
                comments: self
                    .detail
                    .comments
                    .iter()
                    .map(|comment| PrComment {
                        url: comment
                            .url
                            .replace("/pull/7", &format!("/pull/{}", pr.number)),
                        ..comment.clone()
                    })
                    .collect(),
                ..self.detail.clone()
            })
        }

        fn open_pulls(&mut self, account: &str, repo: &str) -> Result<Vec<PrRef>, ReadError> {
            self.asked
                .borrow_mut()
                .push(format!("open {account} {repo}"));
            Ok(vec![PrRef {
                repo: repo.to_owned(),
                number: 7,
            }])
        }
    }

    impl Workers for Named<'_> {
        fn sender_of(&mut self, worker: &str) -> Result<String, String> {
            self.asked
                .borrow_mut()
                .push(format!("{} {worker}", self.app));
            Ok(self.app.to_owned())
        }

        fn look(
            &mut self,
            _sender: &str,
            _worker: &str,
            _awaited: Option<&Delivered>,
            _now: Timestamp,
        ) -> Option<WorkerView> {
            None
        }

        fn send(&mut self, _sender: &str, _worker: &str, _prompt: &str) -> Sent {
            Sent {
                outcome: watch::SendOutcome::Done,
                reported: "done".to_owned(),
            }
        }
    }

    fn github(asked: &RefCell<Vec<String>>) -> FakeGitHub<'_> {
        let comment =
            |id: &str, kind: ItemKind, url: String, body: &str, path: Option<&str>| PrComment {
                id: id.to_owned(),
                kind,
                author: "coderabbitai[bot]".to_owned(),
                url,
                created_at: "2026-10-01T00:00:00Z".to_owned(),
                body: body.to_owned(),
                path: path.map(str::to_owned),
                line: path.map(|_| 3),
                reply: false,
                resolved: false,
                approved: false,
            };
        FakeGitHub {
            asked,
            detail: PrDetail {
                summary: PrSummary {
                    updated_at: "2026-10-23T00:00:00Z".to_owned(),
                    bots: BTreeMap::from([(
                        "coderabbit".to_owned(),
                        Some(BotStatus {
                            state: "SUCCESS".to_owned(),
                            description: "Review completed".to_owned(),
                            created_at: "2026-10-01T00:00:00Z".to_owned(),
                        }),
                    )]),
                    ..summary()
                },
                url: PR.to_owned(),
                title: "Pay".to_owned(),
                failed_required: Vec::new(),
                comments: vec![
                    comment(
                        "inline:1",
                        ItemKind::Inline,
                        format!("{PR}#discussion_r1"),
                        "**🟡 Minor**",
                        Some("a.kt"),
                    ),
                    comment(
                        "issue:2",
                        ItemKind::IssueComment,
                        format!("{PR}#issuecomment-2"),
                        "walkthrough",
                        None,
                    ),
                ],
                no_actionable: Vec::new(),
            },
        }
    }

    // Each PR shows only the comment its record already holds, read later than every record.
    fn legacy_github(asked: &RefCell<Vec<String>>) -> FakeGitHub<'_> {
        let summary = PrSummary {
            updated_at: "2026-10-23T01:00:00Z".to_owned(),
            bots: BTreeMap::from([(
                "coderabbit".to_owned(),
                Some(BotStatus {
                    state: "SUCCESS".to_owned(),
                    description: "Review completed".to_owned(),
                    created_at: "2026-10-22T00:00:00Z".to_owned(),
                }),
            )]),
            ..summary()
        };
        FakeGitHub {
            asked,
            detail: PrDetail {
                summary,
                url: PR.to_owned(),
                title: "Pay".to_owned(),
                failed_required: Vec::new(),
                comments: Vec::new(),
                no_actionable: Vec::new(),
            },
        }
    }

    // Every worker is idle and last talked to an hour ago, so nothing holds a message back.
    fn idle_thread(arguments: &Map<String, Value>) -> Map<String, Value> {
        let noon = noon().timestamp().as_second();
        let read = json!({
            "thread": { "id": arguments["threadId"], "status": { "type": "idle" } },
            "turns": [{
                "status": "completed",
                "startedAt": noon - 3600,
                "completedAt": noon - 3500,
                "items": [{ "type": "userMessage", "content": [] }],
            }],
        });
        let Value::Object(answer) = json!({
            "outcome": "done",
            "result": { "content": [{ "type": "text", "text": read.to_string() }] },
        }) else {
            unreachable!("an object literal");
        };
        answer
    }

    fn summary() -> PrSummary {
        PrSummary {
            state: "OPEN".to_owned(),
            is_draft: false,
            head: HEAD.to_owned(),
            updated_at: "2026-10-23T00:00:00Z".to_owned(),
            mergeable: "MERGEABLE".to_owned(),
            bots: BTreeMap::new(),
            rollup: Some("SUCCESS".to_owned()),
        }
    }

    fn link_of(number: u64, worker: &str) -> PrLink {
        PrLink {
            pr: pay(number),
            url: format!("https://github.com/org/pay/pull/{number}"),
            worker: worker.to_owned(),
            linked_at: "2026-09-01T00:00:00Z".to_owned(),
        }
    }

    fn pay(number: u64) -> PrRef {
        PrRef {
            repo: "org/pay".to_owned(),
            number,
        }
    }

    fn noon() -> Zoned {
        "2026-10-23T12:00:00Z"
            .parse::<Timestamp>()
            .expect("parse the time")
            .to_zoned(TimeZone::UTC)
    }

    fn copy_directory(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("create the directory");
        for entry in fs::read_dir(from).expect("list the fixture") {
            let entry = entry.expect("read an entry");
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_directory(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).expect("copy the fixture");
            }
        }
    }
}
