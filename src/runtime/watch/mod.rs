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

use jiff::{Timestamp, ZonedDateTime};
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
        alerter,
        command::{ExternalCommand, run_tool},
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Run,
    DryRun(Vec<String>),
    Release(String),
    Status { json: bool },
    Sender,
    Link { pr: String, take_over: bool },
}

#[derive(Debug)]
pub(crate) enum Failure {
    Config(RecordError),
    State(RecordError),
    Github(ReadError),
    Refused(String),
    Replaced,
}

type BridgeCall<'call> = dyn Fn(&str, &str, &Map<String, Value>) -> Map<String, Value> + 'call;

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
    // The watch logs and skips an unreadable record, so the status shows it as unreadable.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    undelivered_since: Option<String>,
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

// The watch has no command config, and macOS keeps osascript here.
const OSASCRIPT: &str = "/usr/bin/osascript";

// Well under the one second within which a wait must notice a stop request.
const STOP_CHECK: Duration = Duration::from_millis(500);

pub(crate) fn run(mode: Mode, env: Env<'_>) -> Result<Option<String>, Failure> {
    // The watch needs the Codex App on this Mac, so a Cloud session gets that reason, not a missing-socket hint.
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

pub(crate) fn install(env: Env<'_>) -> Result<(), String> {
    let config = watch_paths().map_err(|failure| failure.to_string())?.config;
    if !config.exists() {
        return Err(format!(
            "{} not found; write it as the README shows",
            config.display()
        ));
    }
    // A bad config must keep the running agent alive, so a dry run checks it before the agent stops.
    output::write_stdout("$ pr-wake watch dry-run\n");
    run(Mode::DryRun(Vec::new()), env).map_err(|failure| failure.to_string())?;

    let home = home_of(env).ok_or("the home directory is unknown")?;
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
    // A harnexus without protocol 1 is never written to, so it is checked before the watch can send.
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
            now: &ZonedDateTime::now,
            log: &log,
            notify: &|subtitle, message| {
                alerter::notify_outcome(&run_tool, Path::new(OSASCRIPT), subtitle, message, &stop)
            },
        },
        stop: &stop,
        pause: &mut |wait| pause(&stop, wait),
        replaced: replaced
            .as_ref()
            .map(|replaced| replaced as &dyn Fn() -> bool),
        ended: &|| ended_by(&unsupported),
    })
}

fn watched_call<'call>(
    call: &'call BridgeCall<'call>,
    unsupported: &'call RefCell<Option<String>>,
    log: &'call dyn Log,
) -> CallApp<'call> {
    Box::new(move |caller, tool, arguments| {
        let answer = call(caller, tool, &arguments);
        // A bridge replaced by an older one ends the watch between cycles, as at start; a closed app only waits.
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

pub(crate) fn caller_id(env: Env<'_>) -> Result<String, Failure> {
    // T3 Code runs Codex with a CODEX_THREAD_ID of Codex's own, so the T3 thread variable decides first.
    let (variable, value) = present(env(T3_THREAD_ENV)).map_or_else(
        || (CODEX_THREAD_ENV, present(env(CODEX_THREAD_ENV))),
        |t3| (T3_THREAD_ENV, Some(t3)),
    );
    let Some(value) = value else {
        return Err(Failure::Refused(format!(
            "{CODEX_THREAD_ENV} is not set; run pr-wake from a thread"
        )));
    };

    // A chat ID is pasted into socket requests and t3 arguments, so it may carry no whitespace or control characters.
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

    let current = watch_record::load_link(root, &named.pr).map_err(Failure::State)?;
    // Only the link is read on a take-over, so an unreadable state never stops a chat from taking its PR over.
    if !take_over && let Some(current) = &current {
        let state = watch_record::load_watch_state(root, &named.pr).map_err(Failure::State)?;
        if state.status.holds_chat() && current.worker != worker {
            return Err(Failure::Refused(format!(
                "{} is already watched for chat {}; link it with --take-over from the chat that takes it over",
                named.url, current.worker
            )));
        }
    }

    // Linking the same PR again keeps its start, so comments made since are still reported.
    let linked_at = match current {
        Some(current) if current.worker == worker => current.linked_at,
        _ => now,
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
    let link = watch_record::load_link(root, &named.pr)
        .map_err(Failure::State)?
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
                    undelivered_since: state.lost_since.map(time::format_timestamp),
                    checked_at: state.checked_at.map(time::format_timestamp),
                }),
            },
        )
        .collect();

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
                let undelivered = read
                    .undelivered_since
                    .as_ref()
                    .map_or_else(String::new, |since| format!(" undelivered since {since}"));
                format!(
                    "{} {}{reason} pending={}{awaiting}{undelivered}",
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
        // A PR no chat has linked has no state, so its first read takes everything as seen and no link time is compared.
        let linked_at = link.map_or(Timestamp::UNIX_EPOCH, |link| link.linked_at);
        reports.push(report_of(&detail, &state, linked_at, account, &config));
    }
    Ok(DryRunOutput {
        dry_run: true,
        prs: reports,
    })
}

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
    linked_at: Timestamp,
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
    #[must_use]
    pub(crate) const fn tag(&self) -> &'static str {
        // The names the TypeScript watch logged, so a search over old and new logs finds both.
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

    use jiff::{SignedDuration, tz::TimeZone};
    use serde_json::json;

    use super::*;
    use crate::app::watch::{
        pr::PrComment,
        reviewers::ReviewBot,
        state::{PrSummary, WatchEvent},
    };

    const PR: &str = "https://github.com/org/pay/pull/7";
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct FakeGitHub<'asked> {
        asked: &'asked RefCell<Vec<String>>,
        detail: PrDetail,
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
                checked_at: Some(noon().timestamp()),
                pending: vec![WatchEvent::Conflict {
                    head: "a".to_owned(),
                }],
                worker: Some("old-chat".to_owned()),
                lost_since: Some(noon().timestamp() + SignedDuration::from_mins(10)),
                ..WatchState::default()
            };
            watch_record::save_watch_state(root, &pay(7), &state).expect("save the state");

            let shown = status(root).expect("status");

            // Other programs parse this line, so its keys, their order and the time format stay as they are.
            assert_eq!(
                serde_json::to_string(&shown).expect("write it"),
                r#"{"sender":null,"prs":[{"pr":"https://github.com/org/pay/pull/7","worker":"worker","status":"halted","reason":"x","pending":1,"followUp":0,"awaitingAnswer":false,"undeliveredSince":"2026-10-23T12:10:00.000Z","handingOver":true,"checkedAt":"2026-10-23T12:00:00.000Z"}]}"#
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
                    "pr_whose_worker_keeps_missing_messages",
                    StatusOutput {
                        sender: Some("chat-1".to_owned()),
                        sender_error: None,
                        prs: vec![PrStatus::Read(ReadStatus {
                            undelivered_since: Some("2026-10-23T12:10:00.000Z".to_owned()),
                            ..read_status(7, WatchStatus::Watching, None, 0, true)
                        })],
                    },
                    vec![
                        "https://github.com/org/pay/pull/7 watching pending=0 awaiting answer undelivered since 2026-10-23T12:10:00.000Z",
                        "sender: chat-1",
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
            PrStatus::Read(read_status(
                number,
                status,
                reason,
                pending,
                awaiting_answer,
            ))
        }

        fn read_status(
            number: u64,
            status: WatchStatus,
            reason: Option<&str>,
            pending: usize,
            awaiting_answer: bool,
        ) -> ReadStatus {
            ReadStatus {
                pr: format!("https://github.com/org/pay/pull/{number}"),
                worker: format!("w{number}"),
                status,
                reason: reason.map(str::to_owned),
                pending,
                follow_up: 0,
                awaiting_answer,
                undelivered_since: None,
                handing_over: false,
                checked_at: None,
            }
        }
    }

    mod commands {
        use super::*;

        #[test]
        fn same_chat_keeps_its_link_time_and_another_chat_must_take_over() {
            let records = Records::new();
            let root = &records.paths.state;
            let (first, later) = (
                noon().timestamp(),
                noon().timestamp() + SignedDuration::from_hours(1),
            );
            link(&from("w7"), root, PR, false, first).expect("link the PR");

            let again = link(&from("w7"), root, PR, false, later);
            let kept = watch_record::load_link(root, &pay(7)).expect("load the link");
            let refused = link(&from("other"), root, PR, false, later);
            let taken = link(&from("other"), root, PR, true, later);
            let moved = watch_record::load_link(root, &pay(7)).expect("load the link");

            assert!(again.is_ok());
            assert_eq!(
                kept.map(|link| (link.worker, link.linked_at)),
                Some(("w7".to_owned(), first))
            );
            assert!(refused.is_err_and(|failure| {
                failure.to_string().contains("already watched for chat w7")
            }));
            assert!(taken.is_ok());
            assert_eq!(
                moved.map(|link| (link.worker, link.linked_at)),
                Some(("other".to_owned(), later))
            );
        }

        #[test]
        fn halted_pr_is_released_with_its_message_taken_as_delivered() {
            let records = Records::new();
            let root = &records.paths.state;
            watch_record::record_link(root, &link_of(7, "worker")).expect("record the link");
            let halted = WatchState {
                status: WatchStatus::Halted,
                pending: vec![WatchEvent::Conflict {
                    head: HEAD.to_owned(),
                }],
                halted: Some(Map::new()),
                ..WatchState::default()
            };
            watch_record::save_watch_state(root, &pay(7), &halted).expect("save the state");

            let released = release(root, "https://github.com/Org/Pay/pull/7/files");
            let again = release(root, PR);
            let unlinked = release(root, "https://github.com/org/pay/pull/8");

            assert_eq!(
                serde_json::to_value(released.expect("release")).expect("write it"),
                json!({ "released": "https://github.com/Org/Pay/pull/7", "worker": "worker" })
            );
            assert_eq!(
                watch_record::load_watch_state(root, &pay(7)).expect("load the state"),
                watch::release(&halted).expect("the PR is halted")
            );
            assert!(again.is_err_and(|failure| failure.to_string().contains("is not halted")));
            assert!(unlinked.is_err_and(|failure| {
                failure.to_string().contains("is not linked to a worker")
            }));
        }

        fn from(chat: &'static str) -> impl Fn(&str) -> Option<OsString> {
            move |variable| (variable == CODEX_THREAD_ENV).then(|| OsString::from(chat))
        }
    }

    #[test]
    fn watch_ends_before_its_next_cycle_once_the_bridge_stops_offering_protocol_1() {
        let records = Records::new();
        let root = &records.paths.state;
        let read = WatchState {
            summary: Some(summary()),
            seen: vec!["bot:coderabbit".to_owned()],
            ..WatchState::default()
        };
        for number in [7, 9] {
            watch_record::record_link(root, &link_of(number, "worker")).expect("record the link");
            watch_record::save_watch_state(root, &pay(number), &read).expect("save the state");
        }
        let refusals = Cell::new(0);
        let unsupported = RefCell::new(None);
        let log = Recorded::default();
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
        let mut github = github(&asked);
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
                notify: &|_, _| Ok(()),
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
        let queued = watch_record::load_watch_state(root, &pay(9)).expect("load PR 9");
        assert_eq!(
            (queued.status, queued.pending.len()),
            (WatchStatus::Watching, 2)
        );
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
            linked_at: "2026-09-01T00:00:00Z".parse().expect("parse the time"),
        }
    }

    fn pay(number: u64) -> PrRef {
        PrRef {
            repo: "org/pay".to_owned(),
            number,
        }
    }

    fn noon() -> ZonedDateTime {
        "2026-10-23T12:00:00Z"
            .parse::<Timestamp>()
            .expect("parse the time")
            .to_zoned_datetime(TimeZone::UTC)
    }
}
