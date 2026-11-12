use std::{
    collections::HashMap,
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    thread::{self, Scope},
    time::Duration,
};

use jiff::Timestamp;

use super::{STOP_CHECK, Tools, launch};
use crate::{
    app::review::{
        config::Choice,
        notice::{Notice, Selection},
        request::{pending_requests, request_queries},
        state::{Request, ReviewState, Status},
    },
    infra::{
        alerter,
        gh::rest::GhRest,
        review_record::{self, ReviewPaths},
        stop::StopRequest,
    },
};

pub(super) struct DaemonDeps<'deps> {
    pub(super) tools: &'deps Tools<'deps>,
    pub(super) paths: &'deps ReviewPaths,
    pub(super) log: &'deps (dyn Fn(&str) + Sync),
    pub(super) now: &'deps (dyn Fn() -> Timestamp + Sync),
    // Tells whether the whole wait passed; a stop request cuts it short.
    pub(super) pause: &'deps (dyn Fn(Duration) -> bool + Sync),
    pub(super) stop: &'deps StopRequest,
    pub(super) replaced: Option<&'deps (dyn Fn() -> bool + Sync)>,
}

// Polling and notifying run on separate threads, so a notification awaiting the owner never delays a poll.
pub(super) struct Daemon<'deps> {
    deps: DaemonDeps<'deps>,
    // The one copy of the state; every change is saved before the lock is released.
    state: Mutex<ReviewState>,
    shown: Mutex<Shown>,
    wake: Wake,
    // Worktrees of one checkout are added one at a time.
    launching: Mutex<()>,
}

#[derive(Default)]
struct Shown {
    notices: HashMap<String, ShownNotice>,
    sequence: u64,
}

struct ShownNotice {
    sequence: u64,
    // Requested to close the notification, for a newer request or a stop.
    close: StopRequest,
    replacing: bool,
}

// A wake-up that is kept until the notify loop takes it, as a channel of one would be.
#[derive(Default)]
struct Wake {
    woken: Mutex<bool>,
    signal: Condvar,
}

const REPLACED: &str = "the review's program was replaced; exiting to be restarted";
const MAX_BACKOFF: Duration = Duration::from_mins(15);

impl<'deps> Daemon<'deps> {
    pub(super) fn new(deps: DaemonDeps<'deps>, state: ReviewState) -> Self {
        Self {
            deps,
            state: Mutex::new(state),
            shown: Mutex::new(Shown::default()),
            wake: Wake::default(),
            launching: Mutex::new(()),
        }
    }

    // Runs until a stop request, with every notification closed and every launch ended.
    pub(super) fn run(&self) -> Result<(), String> {
        let mut state = self.lock_state();
        state.recover((self.deps.now)());
        let saved = self.save(&state);
        drop(state);
        saved?;

        thread::scope(|scope| {
            scope.spawn(|| self.notify_loop(scope));
            self.poll_loop()
        })
    }

    fn poll_loop(&self) -> Result<(), String> {
        let interval = self.poll_interval();
        let mut failures = 0;
        loop {
            // Checked before each poll, so no poll runs on a program replaced while it slept.
            if self.deps.replaced.is_some_and(|replaced| replaced()) {
                self.log(REPLACED);
                self.deps.stop.request();
                return Err(REPLACED.to_owned());
            }

            let delay = match self.poll_once() {
                Ok(()) => {
                    failures = 0;
                    interval
                }
                Err(_) if self.deps.stop.is_requested() => return Ok(()),
                Err(error) => {
                    failures += 1;
                    let delay = backoff(interval, failures);
                    self.log(&format!(
                        "poll failed ({failures} in a row, next in {}s): {error}",
                        delay.as_secs()
                    ));
                    delay
                }
            };
            if !(self.deps.pause)(delay) {
                return Ok(());
            }
        }
    }

    pub(super) fn poll_once(&self) -> Result<(), String> {
        let (login, cache) = {
            let state = self.lock_state();
            (state.login.clone(), state.prs.clone())
        };
        let github = GhRest {
            run: self.deps.tools.run,
            gh: &self.deps.tools.commands.gh,
            stop: self.deps.stop,
        };
        let config = &self.deps.tools.config;

        let login = match login {
            Some(login) => login,
            None => github
                .current_login()
                .map_err(|error| self.record_error(error))?,
        };
        let mut hits = Vec::new();
        for query in request_queries(&config.owners(), &config.teams) {
            hits.extend(
                github
                    .search_pull_requests(&query)
                    .map_err(|error| self.record_error(error))?,
            );
        }
        let pending = pending_requests(hits, &cache, &login, &config.teams, |hit| {
            github.review_requests(&hit.repo, hit.number)
        })
        .map_err(|error| self.record_error(error))?;

        let now = (self.deps.now)();
        let pending_count = pending.found.len();
        let mut state = self.lock_state();
        let baseline = state.is_new();
        let added = state.apply_poll(pending.found, now, baseline);
        state.login = Some(login);
        state.prs = pending.prs;
        state.last_poll = Some(now);
        state.last_error = None;
        let saved = self.save(&state);
        drop(state);
        saved?;

        if baseline {
            self.log(&format!(
                "recorded {pending_count} pending requests as the baseline; only later requests are notified"
            ));
        }
        for key in added {
            self.log(&format!("new request {key}"));
        }
        self.wake.signal();
        Ok(())
    }

    // Bounds the alerter processes without letting unanswered notifications block new requests.
    pub(super) fn notify_loop<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>) {
        loop {
            if self.deps.stop.is_requested() {
                // The requests stay notifying and are shown again after the restart.
                for notice in self.lock_shown().notices.values() {
                    notice.close.request();
                }
                return;
            }

            let mut shown = self.lock_shown();
            let full = shown.notices.len() >= self.deps.tools.config.max_notifications;
            // One notification is closed per waiting request; a closing one already makes room.
            let replacing = shown
                .notices
                .values()
                .filter(|notice| notice.replacing)
                .count();
            if full && replacing < self.queued_count() {
                self.close_oldest(&mut shown);
            }
            drop(shown);

            if !full && let Some(request) = self.claim() {
                let close = StopRequest::default();
                {
                    let mut shown = self.lock_shown();
                    shown.sequence += 1;
                    let sequence = shown.sequence;
                    shown.notices.insert(
                        request.found.key.clone(),
                        ShownNotice {
                            sequence,
                            close: close.clone(),
                            replacing: false,
                        },
                    );
                }
                scope.spawn(move || self.handle(&request, &close));
                continue;
            }
            self.wake.wait(self.deps.stop);
        }
    }

    pub(super) fn queued_count(&self) -> usize {
        self.lock_state().queued_count()
    }

    fn handle(&self, request: &Request, close: &StopRequest) {
        let chosen = self.ask(request, close);
        self.lock_shown().notices.remove(&request.found.key);
        self.wake.signal();
        if let Some(choice) = chosen {
            self.start(request, choice);
        }
    }

    fn ask(&self, request: &Request, close: &StopRequest) -> Option<&'deps Choice> {
        let tools = self.deps.tools;
        let key = &request.found.key;
        let answer = alerter::notify(
            tools.run,
            &tools.commands.alerter,
            &Notice::of(&request.found, &tools.config.choices),
            tools.config.notification_timeout_seconds,
            close,
        );

        // A stop leaves the request notifying, so it is shown again after the restart.
        if self.deps.stop.is_requested() {
            return None;
        }
        // A choice made just before the notification was closed still counts.
        if close.is_requested() && !matches!(answer, Ok(Selection::Chosen(_))) {
            self.log_saved(self.update(key, |request| request.status = Status::Replaced));
            self.log(&format!("{key}: replaced by a newer request"));
            return None;
        }
        let selection = match answer {
            Ok(selection) => selection,
            Err(error) => {
                self.log(&format!("notify {key}: {error}"));
                // Queued again after a poll interval rather than retried in a tight loop.
                if (self.deps.pause)(self.poll_interval()) {
                    self.log_saved(self.update(key, |request| {
                        request.status = Status::Queued;
                        request.error = Some(error);
                    }));
                }
                return None;
            }
        };
        let status = match selection {
            Selection::Chosen(index) => return tools.config.choices.get(index),
            Selection::Declined => Status::Declined,
            Selection::TimedOut => Status::Timeout,
            Selection::Dismissed => Status::Dismissed,
        };
        self.log_saved(self.update(key, |request| request.status = status));
        self.log(&format!("{key}: {}", selection.name()));
        None
    }

    fn start(&self, request: &Request, choice: &Choice) {
        let found = &request.found;
        let key = &found.key;
        // Saved before the App opens, so a restart never opens a second chat.
        let launching = self.update(key, |request| {
            request.status = Status::Launching;
            request.choice = Some(choice.id.clone());
            request.error = None;
        });
        if let Err(error) = launching {
            self.log(&format!("{key}: not launched: {error}"));
            self.report_failure(request, &error);
            return;
        }

        self.log(&format!("{key}: chose {}", choice.id));
        let launched = {
            let _one_at_a_time = self
                .launching
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            launch(
                self.deps.tools,
                choice,
                &found.repo,
                found.number,
                found.requester.as_deref(),
                self.deps.stop,
            )
        };

        let head = launched.pr.map(|pr| pr.head_sha);
        let worktree = launched
            .worktree
            .map(|worktree| worktree.to_string_lossy().into_owned());
        self.log_saved(self.update(key, |request| {
            request.head_sha.clone_from(&head);
            request.worktree.clone_from(&worktree);
            match &launched.result {
                Ok(()) => request.status = Status::Launched,
                Err(error) => {
                    request.status = Status::Failed;
                    request.error = Some(error.clone());
                }
            }
        }));
        match launched.result {
            Ok(()) => self.log(&format!(
                "{key}: opened {} at {}",
                worktree.unwrap_or_default(),
                head.unwrap_or_default()
            )),
            Err(error) => {
                self.log(&format!("{key}: launch failed: {error}"));
                self.report_failure(request, &error);
            }
        }
    }

    // The oldest queued request becomes notifying, saved before it is shown.
    fn claim(&self) -> Option<Request> {
        let mut state = self.lock_state();
        let key = state.next_queued()?.to_owned();
        let now = (self.deps.now)();
        let before = state.requests.get(&key)?.clone();
        let claimed = Request {
            status: Status::Notifying,
            updated_at: now,
            ..before.clone()
        };
        state.requests.insert(key.clone(), claimed.clone());
        let saved = self.save(&state);
        if saved.is_err() {
            // Left queued, so the next wake-up tries again.
            state.requests.insert(key, before);
        }
        drop(state);
        match saved {
            Ok(()) => Some(claimed),
            Err(error) => {
                self.log(&format!("save state: {error}"));
                None
            }
        }
    }

    // A change that cannot be saved is undone, so the state in memory stays the one on disk.
    fn update(&self, key: &str, change: impl FnOnce(&mut Request)) -> Result<(), String> {
        let mut state = self.lock_state();
        let now = (self.deps.now)();
        let Some(before) = state.requests.get(key).cloned() else {
            return Err("request is not in the state".to_owned());
        };
        let mut changed = before.clone();
        change(&mut changed);
        changed.updated_at = now;
        state.requests.insert(key.to_owned(), changed);
        let saved = self.save(&state);
        if saved.is_err() {
            state.requests.insert(key.to_owned(), before);
        }
        drop(state);
        saved.map_err(|error| format!("save state: {error}"))
    }

    fn record_error(&self, error: String) -> String {
        let mut state = self.lock_state();
        state.last_error = Some(error.clone());
        let saved = self.save(&state);
        drop(state);
        match saved {
            Ok(()) => error,
            Err(saved) => format!("{error}\n{saved}"),
        }
    }

    fn close_oldest(&self, shown: &mut Shown) {
        let oldest = shown
            .notices
            .iter_mut()
            .filter(|(_, notice)| !notice.replacing)
            .min_by_key(|(_, notice)| notice.sequence);
        if let Some((key, notice)) = oldest {
            notice.replacing = true;
            self.log(&format!("{key}: closing to show a newer request"));
            notice.close.request();
        }
    }

    fn report_failure(&self, request: &Request, error: &str) {
        let tools = self.deps.tools;
        let found = &request.found;
        let reported = alerter::notify_failure(
            tools.run,
            &tools.commands.osascript,
            &format!("{} #{}", found.repo, found.number),
            &format!("レビューを開始できませんでした: {error}"),
            self.deps.stop,
        );
        if let Err(failure) = reported {
            self.log(&format!("{}: failure notification: {failure}", found.key));
        }
    }

    fn save(&self, state: &ReviewState) -> Result<(), String> {
        review_record::save_state(self.deps.paths, state).map_err(|error| error.to_string())
    }

    fn log_saved(&self, saved: Result<(), String>) {
        if let Err(error) = saved {
            self.log(&error);
        }
    }

    fn log(&self, message: &str) {
        (self.deps.log)(message);
    }

    const fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.deps.tools.config.poll_interval_seconds)
    }

    fn lock_state(&self) -> MutexGuard<'_, ReviewState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_shown(&self) -> MutexGuard<'_, Shown> {
        self.shown.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Wake {
    fn signal(&self) {
        *self.woken.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.signal.notify_one();
    }

    // Returns once woken or on a stop request, which it notices within the stop check.
    fn wait(&self, stop: &StopRequest) {
        let mut woken = self.woken.lock().unwrap_or_else(PoisonError::into_inner);
        while !*woken && !stop.is_requested() {
            woken = self
                .signal
                .wait_timeout(woken, STOP_CHECK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *woken = false;
    }
}

// Rate limits and outages are waited out with a growing delay instead of stopping the review.
fn backoff(interval: Duration, failures: u32) -> Duration {
    let mut delay = interval;
    for _ in 0..failures {
        if delay >= MAX_BACKOFF {
            break;
        }
        delay = delay.saturating_mul(2);
    }
    delay.min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        iter,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };

    use serde::Deserialize as _;
    use serde_json::json;

    use super::*;
    use crate::{
        app::review::{
            config::{ConfigFile, ReviewConfig},
            state::Found,
        },
        infra::{
            codex_app::new_chat_link,
            command::{RunTool, ToolCall},
        },
        runtime::review::{Commands, pause},
    };

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const CODE_REVIEW: &str =
        r#"{"activationType":"actionClicked","activationValue":"コードレビュー"}"#;

    type Answer = Arc<dyn Fn(&ToolCall<'_>) -> Result<Vec<u8>, String> + Send + Sync>;
    type Alerter<'alerter> = &'alerter (dyn Fn(&ToolCall<'_>) -> Result<Vec<u8>, String> + Sync);

    // GitHub, git, alerter and the App as the daemon sees them through its tools.
    struct Fixture {
        home: tempfile::TempDir,
        paths: ReviewPaths,
        config: ReviewConfig,
        // "owner/repo#number" to the JSON of its review_requested events.
        events: Mutex<BTreeMap<String, String>>,
        answer: Mutex<String>,
        // Taken before the default answers, the latest first.
        overrides: Mutex<Vec<(&'static str, Answer)>>,
        calls: Mutex<Vec<String>>,
        opened: Mutex<Vec<String>>,
    }

    #[derive(Default)]
    struct Setup<'setup> {
        // Answered by the test itself, which may wait for the notification to close.
        alerter: Option<Alerter<'setup>>,
        pause: Pause<'setup>,
    }

    #[derive(Default)]
    enum Pause<'pause> {
        #[default]
        Immediate,
        // As the agent waits, so a stop ends the poll loop.
        Real,
        With(&'pause (dyn Fn(Duration) -> bool + Sync)),
    }

    #[test]
    fn each_request_is_notified_once_and_the_choice_opened() {
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);

        fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("poll");
            assert_eq!(
                daemon.queued_count(),
                0,
                "the first poll is only a baseline"
            );

            notifying(daemon, stop, || {
                fixture.request("acme/app", 12, &[requested(30, "sato")]);
                daemon.poll_once().expect("poll");
                fixture.wait_status("acme/app#12@30", Status::Launched);
            });
        });

        let worktree = fixture.home.path().join(format!(
            ".local/share/pr-wake/worktrees/acme/app/pr-12-{}",
            HEAD.get(..12).unwrap_or_default()
        ));
        let skill = fixture
            .home
            .path()
            .join(".codex/skills/code-review/SKILL.md");
        let prompt = format!(
            "[$code-review]({}) https://github.com/acme/app/pull/12 {HEAD} {}",
            skill.display(),
            worktree.display()
        );
        assert_eq!(
            fixture.opened(),
            [new_chat_link(
                &prompt,
                &fixture.home.path().join("review-home")
            )]
        );
        let launched = fixture.state().requests["acme/app#12@30"].clone();
        assert_eq!(launched.choice.as_deref(), Some("code"));
        assert_eq!(launched.head_sha.as_deref(), Some(HEAD));
        assert_eq!(launched.worktree, Some(worktree.display().to_string()));
    }

    #[test]
    fn restart_brings_back_nothing_handled_but_a_re_request_of_the_same_pr() {
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);
        fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("baseline poll");
            notifying(daemon, stop, || {
                fixture.request("acme/app", 12, &[requested(30, "sato")]);
                daemon.poll_once().expect("poll");
                fixture.wait_status("acme/app#12@30", Status::Launched);
            });
        });

        let queued_after_restart = fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("poll after the restart");
            let queued = daemon.queued_count();
            notifying(daemon, stop, || {
                fixture.request(
                    "acme/app",
                    12,
                    &[requested(30, "sato"), requested(31, "sato")],
                );
                daemon.poll_once().expect("poll");
                fixture.wait_status("acme/app#12@31", Status::Launched);
            });
            queued
        });

        assert_eq!(
            queued_after_restart, 0,
            "a launched request must not come back"
        );
        assert_eq!(fixture.opened().len(), 2);
    }

    #[test]
    fn answer_other_than_a_choice_starts_nothing_and_does_not_come_back() {
        for (name, answer, expected) in [
            (
                "decline",
                r#"{"activationType":"closed","activationValue":"見送る"}"#,
                Status::Declined,
            ),
            (
                "timeout",
                r#"{"activationType":"timeout"}"#,
                Status::Timeout,
            ),
            (
                "body_click",
                r#"{"activationType":"contentsClicked"}"#,
                Status::Dismissed,
            ),
        ] {
            let fixture = Fixture::new();
            fixture.answer(answer);

            let queued = fixture.with_daemon(&Setup::default(), |daemon, stop| {
                daemon.poll_once().expect("baseline poll");
                notifying(daemon, stop, || {
                    fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
                    daemon.poll_once().expect("poll");
                    fixture.wait_status("acme/app#1@10", expected);
                });
                daemon.poll_once().expect("poll again");
                daemon.queued_count()
            });

            assert_eq!(queued, 0, "case: {name}");
            assert!(!fixture.ran("git"), "case: {name}");
            assert!(fixture.opened().is_empty(), "case: {name}");
        }
    }

    #[test]
    fn launch_failure_is_notified_without_opening_anything() {
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);

        fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("baseline poll");
            notifying(daemon, stop, || {
                fixture.request("acme/unknown", 5, &[requested(50, "tanaka")]);
                daemon.poll_once().expect("poll");
                fixture.wait_status("acme/unknown#5@50", Status::Failed);
                wait_for(|| fixture.ran("osascript"));
            });
        });

        let failed = fixture.state().requests["acme/unknown#5@50"].clone();
        assert!(fixture.opened().is_empty());
        assert_eq!(failed.head_sha.as_deref(), Some(HEAD));
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no local checkout")),
            "{failed:?}"
        );
    }

    #[test]
    fn failed_notification_is_queued_again_after_a_poll_interval() {
        let fixture = Fixture::new();
        fixture.answer_with("alerter", |_| {
            Err("alerter: exit status 1: no permission".to_owned())
        });
        let paused = Mutex::new(Vec::new());
        let pause = |wait| {
            paused.lock().expect("note the wait").push(wait);
            true
        };

        fixture.with_daemon(
            &Setup {
                pause: Pause::With(&pause),
                ..Setup::default()
            },
            |daemon, stop| {
                daemon.poll_once().expect("baseline poll");
                notifying(daemon, stop, || {
                    fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
                    daemon.poll_once().expect("poll");
                    wait_for(|| {
                        fixture.state().requests["acme/app#1@10"]
                            .error
                            .as_deref()
                            .is_some_and(|error| error.contains("no permission"))
                    });
                });
            },
        );

        let paused = paused.into_inner().expect("read the waits");
        assert!(paused.contains(&Duration::from_mins(1)), "{paused:?}");
    }

    #[test]
    fn api_failure_keeps_polling_with_a_growing_delay() {
        let fixture = Fixture::new();
        fixture.answer_with("gh api --paginate -X GET search/issues", |_| {
            Err("HTTP 403: API rate limit exceeded".to_owned())
        });
        let delays = Mutex::new(Vec::new());
        let pause = |delay| {
            let mut delays = delays.lock().expect("note the delay");
            delays.push(delay);
            delays.len() < 3
        };

        let ended = fixture.with_daemon(
            &Setup {
                pause: Pause::With(&pause),
                ..Setup::default()
            },
            |daemon, _| daemon.poll_loop(),
        );

        assert_eq!(ended, Ok(()));
        assert_eq!(
            delays.into_inner().expect("read the delays"),
            [120, 240, 480].map(Duration::from_secs)
        );
        assert!(
            fixture
                .state()
                .last_error
                .is_some_and(|error| error.contains("rate limit"))
        );
    }

    #[test]
    fn backoff_doubles_up_to_fifteen_minutes() {
        let interval = Duration::from_mins(1);

        let delays = [1, 2, 3, 4, 10].map(|failures| backoff(interval, failures).as_secs());

        assert_eq!(delays, [120, 240, 480, 900, 900]);
    }

    #[test]
    fn oldest_notification_is_closed_for_a_new_request() {
        for (name, late, slow_close) in [
            ("new_request_while_three_are_shown", true, false),
            // alerter takes a while to close, and wake-ups meanwhile must not close another one.
            ("four_queued_and_wake_ups_while_closing", false, true),
        ] {
            let fixture = Fixture::new();
            let closed = Mutex::new(Vec::new());
            let closing = AtomicBool::new(false);
            let released = AtomicBool::new(false);
            let alerter = |call: &ToolCall<'_>| {
                let key = group_of(call);
                if key.starts_with("acme/app#4@") {
                    return Ok(br#"{"activationType":"closed"}"#.to_vec());
                }
                wait_for(|| call.stop.is_requested());
                closed.lock().expect("note the close").push(key);
                if slow_close {
                    closing.store(true, Ordering::SeqCst);
                    wait_for(|| released.load(Ordering::SeqCst));
                }
                Err("alerter was stopped".to_owned())
            };

            let closed_before_the_fourth = fixture.with_daemon(
                &Setup {
                    alerter: Some(&alerter),
                    ..Setup::default()
                },
                |daemon, stop| {
                    daemon.poll_once().expect("baseline poll");
                    let queued = if late { 3 } else { 4 };
                    for number in 1..=queued {
                        fixture.request("acme/app", number, &[requested(number * 10, "tanaka")]);
                    }
                    daemon.poll_once().expect("poll");
                    notifying(daemon, stop, || {
                        if late {
                            fixture.wait_status("acme/app#3@30", Status::Notifying);
                            fixture.request("acme/app", 4, &[requested(40, "tanaka")]);
                            daemon.poll_once().expect("poll the new request");
                        }
                        if slow_close {
                            wait_for(|| closing.load(Ordering::SeqCst));
                            for _ in 0..5 {
                                daemon.poll_once().expect("poll while closing");
                                thread::sleep(Duration::from_millis(10));
                            }
                            released.store(true, Ordering::SeqCst);
                        }
                        // The fourth is shown only after the closed one exits, so any extra close happened before this.
                        fixture.wait_status("acme/app#4@40", Status::Declined);
                        closed.lock().expect("read the closes").clone()
                    })
                },
            );

            assert_eq!(closed_before_the_fourth, ["acme/app#1@10"], "case: {name}");
            assert_eq!(
                fixture.status("acme/app#1@10"),
                Status::Replaced,
                "case: {name}"
            );
        }
    }

    #[test]
    fn stop_closes_a_shown_notification_and_leaves_it_to_be_shown_again() {
        let fixture = Fixture::new();
        fixture.save_queued("acme/app", 1, 10);
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
        let shown = AtomicBool::new(false);
        let alerter = |call: &ToolCall<'_>| {
            shown.store(true, Ordering::SeqCst);
            wait_for(|| call.stop.is_requested());
            Err("alerter was stopped".to_owned())
        };

        let (ended, waited) = fixture.with_daemon(
            &Setup {
                alerter: Some(&alerter),
                pause: Pause::Real,
            },
            |daemon, stop| {
                thread::scope(|scope| {
                    let run = scope.spawn(|| daemon.run());
                    wait_for(|| shown.load(Ordering::SeqCst));
                    let requested_at = Instant::now();
                    stop.request();
                    let ended = run.join().expect("join the daemon");
                    (ended, requested_at.elapsed())
                })
            },
        );

        assert_eq!(ended, Ok(()));
        assert!(
            waited < Duration::from_secs(2),
            "stopped {waited:?} after the request"
        );
        assert_eq!(fixture.status("acme/app#1@10"), Status::Notifying);
        assert!(fixture.opened().is_empty());
    }

    #[test]
    fn replaced_program_ends_the_daemon_before_the_next_poll() {
        let fixture = Fixture::new();
        let run = |call: &ToolCall<'_>| fixture.run(call);
        let tools = fixture.tools(&run);
        let stop = StopRequest::default();
        let daemon = Daemon::new(
            DaemonDeps {
                tools: &tools,
                paths: &fixture.paths,
                log: &|_| {},
                now: &Timestamp::now,
                pause: &|_| true,
                stop: &stop,
                replaced: Some(&|| true),
            },
            ReviewState::default(),
        );

        let ended = daemon.run();

        assert_eq!(ended, Err(REPLACED.to_owned()));
        assert!(stop.is_requested(), "the notify loop must end too");
        assert!(!fixture.ran("gh"), "no poll may run on a replaced program");
    }

    impl Fixture {
        fn new() -> Self {
            let home = tempfile::tempdir().expect("create a home");
            let config = ConfigFile::deserialize(json!({
                "repositories": {"acme/app": "~/src/app"},
                "choices": [{
                    "id": "code",
                    "label": "コードレビュー",
                    "skill": "code-review",
                    "prompt": "[${{.Skill}}]({{.SkillPath}}) {{.PR.URL}} {{.PR.HeadSHA}} {{.Worktree}}",
                }],
            }))
            .expect("read the config")
            .resolve(home.path(), &home.path().join(".codex"))
            .expect("a valid config");
            let paths = ReviewPaths {
                config: home.path().join("review.json"),
                state: home.path().join("state"),
            };
            Self {
                home,
                paths,
                config,
                events: Mutex::new(BTreeMap::new()),
                answer: Mutex::new(String::new()),
                overrides: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                opened: Mutex::new(Vec::new()),
            }
        }

        // A daemon over the saved state, as after a restart.
        fn with_daemon<T>(
            &self,
            setup: &Setup<'_>,
            body: impl FnOnce(&Daemon<'_>, &StopRequest) -> T,
        ) -> T {
            let run = |call: &ToolCall<'_>| match setup.alerter {
                Some(alerter) if call.program == Path::new("alerter") => {
                    self.note(call);
                    alerter(call)
                }
                _ => self.run(call),
            };
            let tools = self.tools(&run);
            let stop = StopRequest::default();
            let real = |wait| pause(&stop, wait);
            let state = review_record::load_state(&self.paths)
                .expect("read the state")
                .unwrap_or_default();
            let daemon = Daemon::new(
                DaemonDeps {
                    tools: &tools,
                    paths: &self.paths,
                    log: &|_| {},
                    now: &Timestamp::now,
                    pause: match &setup.pause {
                        Pause::Immediate => &|_| true,
                        Pause::Real => &real,
                        Pause::With(pause) => *pause,
                    },
                    stop: &stop,
                    replaced: None,
                },
                state,
            );
            body(&daemon, &stop)
        }

        fn tools<'run>(&self, run: &'run RunTool<'run>) -> Tools<'run> {
            Tools {
                run,
                commands: Commands {
                    gh: PathBuf::from("gh"),
                    git: PathBuf::from("git"),
                    alerter: PathBuf::from("alerter"),
                    open: PathBuf::from("open"),
                    osascript: PathBuf::from("osascript"),
                },
                config: self.config.clone(),
            }
        }

        fn run(&self, call: &ToolCall<'_>) -> Result<Vec<u8>, String> {
            let line = self.note(call);
            let overridden = self
                .overrides
                .lock()
                .expect("read the overrides")
                .iter()
                .rev()
                .find(|(prefix, _)| line.starts_with(prefix))
                .map(|(_, answer)| Arc::clone(answer));
            if let Some(answer) = overridden {
                return answer(call);
            }
            let checkout = self.home.path().join("src/app").display().to_string();
            if line == "gh api user" {
                return Ok(br#"{"login":"me"}"#.to_vec());
            }
            if line.starts_with("gh api --paginate -X GET search/issues") {
                return Ok(self.search().into_bytes());
            }
            if let Some(path) = line.strip_prefix("gh api --paginate ") {
                return self.events_of(path);
            }
            if let Some(path) = line.strip_prefix("gh api repos/") {
                let parts = path.split('/').collect::<Vec<_>>();
                return Ok(pull_json(&format!("{}/{}", parts[0], parts[1]), parts[3]).into_bytes());
            }
            if line.starts_with("alerter ") {
                return Ok(self
                    .answer
                    .lock()
                    .expect("read the answer")
                    .clone()
                    .into_bytes());
            }
            if line == format!("git -C {checkout} config --get remote.origin.url") {
                return Ok(b"git@github.com:acme/app.git\n".to_vec());
            }
            if let Some(link) = line.strip_prefix("open ") {
                self.opened
                    .lock()
                    .expect("note the link")
                    .push(link.to_owned());
                return Ok(Vec::new());
            }
            let quiet = [
                format!("git -C {checkout} cat-file "),
                format!("git -C {checkout} worktree add "),
                "osascript ".to_owned(),
            ];
            if quiet.iter().any(|prefix| line.starts_with(prefix)) {
                return Ok(Vec::new());
            }
            Err(format!("unexpected command: {line}"))
        }

        fn note(&self, call: &ToolCall<'_>) -> String {
            let line = iter::once(call.program.as_os_str())
                .chain(call.arguments.iter().map(OsString::as_os_str))
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().expect("note the call").push(line.clone());
            line
        }

        // Each PR's updated_at follows its events, so a new event reads as an update.
        fn search(&self) -> String {
            let items = self
                .events
                .lock()
                .expect("read the events")
                .iter()
                .map(|(pr, events)| {
                    let (repo, number) = pr.split_once('#').expect("repo#number");
                    json!({
                        "number": number.parse::<u64>().expect("a number"),
                        "title": format!("PR {number}"),
                        "updated_at": events.len().to_string(),
                        "repository_url": format!("https://api.github.com/repos/{repo}"),
                    })
                })
                .collect::<Vec<_>>();
            json!({"incomplete_results": false, "items": items}).to_string()
        }

        fn events_of(&self, path: &str) -> Result<Vec<u8>, String> {
            self.events
                .lock()
                .expect("read the events")
                .iter()
                .find(|(pr, _)| {
                    let (repo, number) = pr.split_once('#').expect("repo#number");
                    path.starts_with(&format!("repos/{repo}/issues/{number}/"))
                })
                .map(|(_, events)| events.clone().into_bytes())
                .ok_or_else(|| format!("no events for {path}"))
        }

        fn request(&self, repo: &str, number: u64, events: &[String]) {
            self.events.lock().expect("note the request").insert(
                format!("{repo}#{number}"),
                format!("[{}]", events.join(",")),
            );
        }

        fn answer(&self, answer: &str) {
            answer.clone_into(&mut self.answer.lock().expect("set the answer"));
        }

        fn answer_with(
            &self,
            prefix: &'static str,
            answer: impl Fn(&ToolCall<'_>) -> Result<Vec<u8>, String> + Send + Sync + 'static,
        ) {
            self.overrides
                .lock()
                .expect("add the override")
                .push((prefix, Arc::new(answer)));
        }

        // A state that has polled once and holds one queued request.
        fn save_queued(&self, repo: &str, number: u64, event: u64) {
            let now = Timestamp::now();
            let mut state = ReviewState::default();
            state.apply_poll(
                vec![Found {
                    key: format!("{repo}#{number}@{event}"),
                    repo: repo.to_owned(),
                    number,
                    title: format!("PR {number}"),
                    requester: Some("tanaka".to_owned()),
                    team: None,
                }],
                now,
                false,
            );
            state.login = Some("me".to_owned());
            state.last_poll = Some(now);
            review_record::save_state(&self.paths, &state).expect("save the state");
        }

        fn state(&self) -> ReviewState {
            review_record::load_state(&self.paths)
                .expect("read the state")
                .expect("a saved state")
        }

        fn status(&self, key: &str) -> Status {
            self.state()
                .requests
                .get(key)
                .unwrap_or_else(|| panic!("{key} is not in the state"))
                .status
        }

        fn wait_status(&self, key: &str, expected: Status) {
            wait_for(|| {
                review_record::load_state(&self.paths)
                    .ok()
                    .flatten()
                    .and_then(|state| state.requests.get(key).map(|request| request.status))
                    == Some(expected)
            });
        }

        fn ran(&self, prefix: &str) -> bool {
            self.calls
                .lock()
                .expect("read the calls")
                .iter()
                .any(|line| line.starts_with(prefix))
        }

        fn opened(&self) -> Vec<String> {
            self.opened.lock().expect("read the links").clone()
        }
    }

    // Runs the notify loop beside `body`, and stops it however `body` ends.
    fn notifying<T>(daemon: &Daemon<'_>, stop: &StopRequest, body: impl FnOnce() -> T) -> T {
        struct StopOnDrop<'stop>(&'stop StopRequest);

        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.request();
            }
        }

        thread::scope(|scope| {
            let _stop = StopOnDrop(stop);
            scope.spawn(|| daemon.notify_loop(scope));
            body()
        })
    }

    fn wait_for(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn requested(id: u64, requester: &str) -> String {
        json!({
            "id": id,
            "event": "review_requested",
            "review_requester": {"login": requester},
            "requested_reviewer": {"login": "me"},
        })
        .to_string()
    }

    fn group_of(call: &ToolCall<'_>) -> String {
        let arguments = call
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let group = arguments
            .iter()
            .position(|argument| argument == "--group")
            .and_then(|index| arguments.get(index + 1))
            .expect("a --group argument");
        group.strip_prefix("pr-wake:").unwrap_or(group).to_owned()
    }

    fn pull_json(repo: &str, number: &str) -> String {
        json!({
            "number": number.parse::<u64>().expect("a number"),
            "title": "T",
            "html_url": format!("https://github.com/{repo}/pull/{number}"),
            "head": {"sha": HEAD},
            "base": {"ref": "main", "repo": {"full_name": repo}},
        })
        .to_string()
    }
}
