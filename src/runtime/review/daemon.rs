use std::{
    collections::HashMap,
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    thread::{self, Scope},
    time::Duration,
};

use jiff::{Timestamp, tz::TimeZone};

use super::{Route, STOP_CHECK, Tools, ask_owner, github, launch};
use crate::{
    app::review::{
        batch::{Digest, Listed, NowRequest, Plan, due_slot, plan},
        config::Choice,
        inbox::threads_to_clear,
        notice::Selection,
        parse_pr_ref,
        request::{pending_requests, pr_key, request_queries},
        state::{PendingDigest, Request, ReviewState, Status},
    },
    infra::{
        alerter,
        gh::rest::GhRest,
        harnexus::{Created, NewChat},
        review_record::{self, ReviewPaths},
        stop::StopRequest,
    },
};

pub(super) struct DaemonDeps<'deps> {
    pub(super) tools: &'deps Tools<'deps>,
    pub(super) paths: &'deps ReviewPaths,
    pub(super) log: &'deps (dyn Fn(&str) + Sync),
    pub(super) now: &'deps (dyn Fn() -> Timestamp + Sync),
    pub(super) zone: &'deps TimeZone,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    SentToLinked(String),
    Started(Option<String>),
    Failed,
}

// A wake-up that is kept until the notify loop takes it, as a channel of one would be.
#[derive(Default)]
struct Wake {
    woken: Mutex<bool>,
    signal: Condvar,
}

const REPLACED: &str = "the review's program was replaced; exiting to be restarted";
const MAX_BACKOFF: Duration = Duration::from_mins(15);
const BATCH_CHECK: Duration = Duration::from_secs(1);

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
            if self.deps.tools.config.batches.is_some() {
                scope.spawn(|| self.batch_loop());
            } else {
                scope.spawn(|| self.notify_loop(scope));
            }
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
        if self.deps.tools.config.batches.is_some() {
            self.clear_inbox(&github);
        }
        self.wake.signal();
        Ok(())
    }

    fn clear_inbox(&self, github: &GhRest<'_>) {
        let tracked = self.lock_state().tracked_prs((self.deps.now)());
        if tracked.is_empty() {
            return;
        }
        let threads = match github.unread_notifications() {
            Ok(threads) => threads,
            Err(error) => {
                self.log(&format!("GitHub notifications: {error}"));
                return;
            }
        };
        for thread in threads_to_clear(&threads, &tracked) {
            let cleared = github
                .unsubscribe_thread(&thread.id)
                .and_then(|()| github.mark_thread_done(&thread.id));
            let (repo, number) = thread.pr.clone().unwrap_or_default();
            match cleared {
                Ok(()) => self.log(&format!(
                    "{repo}#{number}: cleared GitHub notification {} ({})",
                    thread.id, thread.reason
                )),
                Err(error) => self.log(&format!(
                    "{repo}#{number}: GitHub notification {}: {error}",
                    thread.id
                )),
            }
        }
    }

    fn batch_loop(&self) {
        while !self.deps.stop.is_requested() {
            self.batch_once();
            if !(self.deps.pause)(BATCH_CHECK) {
                return;
            }
        }
    }

    pub(super) fn batch_once(&self) {
        let Some(batches) = &self.deps.tools.config.batches else {
            return;
        };
        match review_record::take_now_requests(self.deps.paths) {
            Ok(requests) => {
                for request in requests {
                    match request {
                        Ok(NowRequest { pr: Some(pr) }) => self.start_now(&pr),
                        Ok(NowRequest { pr: None }) => self.run_batch((self.deps.now)()),
                        Err(error) => self.log(&format!("review now: {error}")),
                    }
                }
            }
            Err(error) => self.log(&format!("review now: {error}")),
        }

        let now = (self.deps.now)().to_zoned(self.deps.zone.clone());
        let mut state = self.lock_state();
        if let Some(slot) = due_slot(batches, &now, state.last_batch) {
            // Saved before any review starts, so a restart within the slot does not run it again.
            let before = state.last_batch.replace(slot.start);
            let saved = self.save(&state);
            if saved.is_err() {
                state.last_batch = before;
            }
            drop(state);
            match saved {
                Ok(()) => self.run_batch(slot.ready),
                Err(error) => self.log(&format!("batch not run: save state: {error}")),
            }
        } else {
            drop(state);
        }
        self.show_due_digests();
    }

    fn run_batch(&self, ready: Timestamp) {
        let (login, queued) = {
            let state = self.lock_state();
            (
                state.login.clone().unwrap_or_default(),
                state.queued_in_order(),
            )
        };
        let mut digest = Digest::default();
        let mut pending = None;
        for request in queued {
            // What is left stays queued for the next batch after the restart.
            if self.deps.stop.is_requested() {
                break;
            }
            let Some(request) = self.claim_key(request.found.key, &[Status::Queued]) else {
                continue;
            };
            let key = &request.found.key;
            match self.plan(&request, &login) {
                Plan::Report(reason) => {
                    self.log_saved(self.update(key, |request| request.status = Status::Reported));
                    self.log(&format!("{key}: only reported ({reason:?})"));
                    digest.reported.push((Listed::of(&request.found), reason));
                }
                Plan::Start => self.start_listed(&request, &mut digest),
            }
            // Saved after each request, so a stop in the middle still shows what was done.
            self.keep_digest(&mut pending, ready, &digest);
        }
        self.log(&format!(
            "batch: {} started, {} reported, {} failed",
            digest.started.len(),
            digest.reported.len(),
            digest.failed
        ));
    }

    fn start_now(&self, pr: &str) {
        let (repo, number) = match parse_pr_ref(pr) {
            Ok(pr) => pr,
            Err(error) => {
                self.log(&format!("review now: {error}"));
                return;
            }
        };
        let key = self
            .lock_state()
            .startable(&repo, number)
            .map(|request| request.found.key.clone());
        let Some(key) = key else {
            self.log(&format!(
                "review now: {repo}#{number} has no queued or reported request"
            ));
            return;
        };
        let Some(request) = self.claim_key(key, &[Status::Queued, Status::Reported]) else {
            return;
        };
        let mut digest = Digest::default();
        self.start_listed(&request, &mut digest);
        self.keep_digest(&mut None, (self.deps.now)(), &digest);
    }

    // Only the batch thread adds or removes digests, so the index stays valid while the batch runs.
    fn keep_digest(&self, pending: &mut Option<usize>, ready: Timestamp, digest: &Digest) {
        let mut state = self.lock_state();
        let before = state.pending_digests.clone();
        let digests = &mut state.pending_digests;
        if let Some(kept) = pending.and_then(|index| digests.get_mut(index)) {
            kept.digest = digest.clone();
        } else {
            *pending = Some(digests.len());
            digests.push(PendingDigest {
                ready,
                digest: digest.clone(),
            });
        }
        let saved = self.save(&state);
        if saved.is_err() {
            state.pending_digests = before;
        }
        drop(state);
        self.log_saved(saved.map_err(|error| format!("save state: {error}")));
    }

    fn show_due_digests(&self) {
        let now = (self.deps.now)();
        loop {
            // A stop may have cut a notification short, so what is left is shown after the restart.
            if self.deps.stop.is_requested() {
                return;
            }
            let due = self
                .lock_state()
                .pending_digests
                .iter()
                .enumerate()
                .find(|(_, pending)| pending.ready <= now)
                .map(|(index, pending)| (index, pending.clone()));
            let Some((index, pending)) = due else {
                return;
            };
            if let Err(error) = self.report_digest(&pending) {
                self.log(&format!("batch notification: {error}"));
                // A stop may have kept osascript from running, so the digest stays for the restarted review.
                if self.deps.stop.is_requested() {
                    return;
                }
            }

            let mut state = self.lock_state();
            // Removed even when it failed otherwise or was not saved, so a broken osascript or disk never repeats it every second.
            state.pending_digests.remove(index);
            let saved = self.save(&state);
            drop(state);
            self.log_saved(saved.map_err(|error| format!("save state: {error}")));
        }
    }

    fn plan(&self, request: &Request, login: &str) -> Plan {
        let found = &request.found;
        match github(self.deps.tools, self.deps.stop)
            .pull_request_reviewers(&found.repo, found.number)
        {
            Ok(reviewers) => plan(&reviewers, login),
            Err(error) => {
                self.log(&format!("{}: reviewers: {error}; starting it", found.key));
                Plan::Start
            }
        }
    }

    fn start_listed(&self, request: &Request, digest: &mut Digest) {
        let choices = &self.deps.tools.config.choices;
        // No notification asks for a choice in a batch, so a linked PR keeps its earlier one and any other takes the first, which the config always has.
        let choice = self.linked_choice(request).unwrap_or(&choices[0]);
        let chat = match self.start(request, choice) {
            Outcome::Failed => {
                digest.failed += 1;
                return;
            }
            Outcome::SentToLinked(chat) => Some(chat),
            Outcome::Started(chat) => chat,
        };
        digest.started.push(Listed {
            chat,
            ..Listed::of(&request.found)
        });
    }

    fn report_digest(&self, pending: &PendingDigest) -> Result<(), String> {
        let tools = self.deps.tools;
        let ready = pending.ready.to_zoned(self.deps.zone.clone());
        let title = format!("レビュー報告｜{}", ready.strftime("%m/%d %H:%M"));
        let prompt = pending.digest.report_prompt();
        let created = (tools.create)(
            &NewChat {
                project: &tools.config.chat_home,
                title: &title,
                prompt: &prompt,
            },
            self.deps.stop,
        );
        match created {
            Created::Started(chat) => self.log(&format!("batch report: chat {chat}")),
            // The chat may exist, so the digest is not shown again in a notification.
            Created::Unknown(reason) => self.log(&format!(
                "batch report: the chat could not be confirmed; check Codex App: {reason}"
            )),
            Created::NotSent(reason) => {
                self.log(&format!("batch report: no chat, so notifying: {reason}"));
                return self.notify_digest(&pending.digest);
            }
        }
        Ok(())
    }

    fn notify_digest(&self, digest: &Digest) -> Result<(), String> {
        let tools = self.deps.tools;
        alerter::notify_outcome(
            tools.run,
            &tools.commands.osascript,
            &digest.subtitle(),
            &digest.message(),
            self.deps.stop,
        )
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

            // Taken before the notification slots are counted, as it needs none and must not close another request's notification.
            if let Some((request, choice)) = self.claim_linked() {
                scope.spawn(move || self.resend(&request, choice));
                continue;
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
        let found = &request.found;
        let size = GhRest {
            run: tools.run,
            gh: &tools.commands.gh,
            stop: close,
        }
        .pull_request_size(&found.repo, found.number)
        .inspect_err(|error| self.log(&format!("{key}: size: {error}")))
        .ok();
        let answer = ask_owner(tools, found, size.as_ref(), close, &mut |line| {
            self.log(&format!("{key}: {line}"));
        });

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
            Selection::Dismissed | Selection::Opened => Status::Dismissed,
        };
        self.log_saved(self.update(key, |request| request.status = status));
        self.log(&format!("{key}: {}", selection.name()));
        None
    }

    fn linked_choice(&self, request: &Request) -> Option<&'deps Choice> {
        let tools = self.deps.tools;
        let found = &request.found;
        match review_record::load_chat(&tools.paths, &found.repo, found.number) {
            Ok(Some(_)) => {}
            Ok(None) => return None,
            // The launch reports an unreadable link, so the owner is asked as before.
            Err(error) => {
                self.log(&format!("{}: read the linked chat: {error}", found.key));
                return None;
            }
        }

        // No notification asks for a choice here, so the latest launched review of the PR supplies it.
        let pr = pr_key(&found.repo, found.number);
        let earlier = self
            .lock_state()
            .requests
            .values()
            .filter(|earlier| {
                earlier.found.key != found.key
                    && earlier.status == Status::Launched
                    && pr_key(&earlier.found.repo, earlier.found.number) == pr
            })
            .max_by_key(|earlier| earlier.updated_at)
            .and_then(|earlier| earlier.choice.clone())?;
        tools
            .config
            .choices
            .iter()
            .find(|choice| choice.id == earlier)
    }

    fn resend(&self, request: &Request, choice: &Choice) {
        self.log(&format!(
            "{}: re-request of a linked PR, sent with {} without asking",
            request.found.key, choice.id
        ));
        if matches!(self.start(request, choice), Outcome::SentToLinked(_)) {
            self.report(request, "再レビューを紐づけ済みのチャットへ送りました");
        }
    }

    fn start(&self, request: &Request, choice: &Choice) -> Outcome {
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
            return Outcome::Failed;
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
        let head = head.unwrap_or_default();
        let worktree = worktree.unwrap_or_default();
        match (launched.result, launched.route) {
            (Ok(()), Some(Route::Linked(chat))) => {
                self.log(&format!(
                    "{key}: sent to chat {chat} for {worktree} at {head}"
                ));
                return Outcome::SentToLinked(chat);
            }
            (Ok(()), Some(Route::Started(chat))) => {
                self.log(&format!("{key}: started chat {chat} at {head}"));
                return Outcome::Started(Some(chat));
            }
            (Ok(()), Some(Route::Opened(reason))) => self.log(&format!(
                "{key}: opened {worktree} at {head}, as no chat could be started: {reason}"
            )),
            (Ok(()), None) => self.log(&format!("{key}: opened {worktree} at {head}")),
            (Err(error), _) => {
                self.log(&format!("{key}: launch failed: {error}"));
                self.report_failure(request, &error);
                return Outcome::Failed;
            }
        }
        Outcome::Started(None)
    }

    // The oldest queued request becomes notifying, saved before it is shown.
    fn claim(&self) -> Option<Request> {
        let key = self.lock_state().next_queued()?.to_owned();
        self.claim_key(key, &[Status::Queued])
    }

    fn claim_linked(&self) -> Option<(Request, &'deps Choice)> {
        let queued = self.lock_state().queued_in_order();
        queued.into_iter().find_map(|request| {
            let choice = self.linked_choice(&request)?;
            let claimed = self.claim_key(request.found.key, &[Status::Queued])?;
            Some((claimed, choice))
        })
    }

    fn claim_key(&self, key: String, from: &[Status]) -> Option<Request> {
        let mut state = self.lock_state();
        let now = (self.deps.now)();
        let before = state
            .requests
            .get(&key)
            .filter(|request| from.contains(&request.status))?
            .clone();
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
        self.report(request, &format!("レビューを開始できませんでした: {error}"));
    }

    fn report(&self, request: &Request, message: &str) {
        let tools = self.deps.tools;
        let found = &request.found;
        let reported = alerter::notify_outcome(
            tools.run,
            &tools.commands.osascript,
            &format!("{} #{}", found.repo, found.number),
            message,
            self.deps.stop,
        );
        if let Err(failure) = reported {
            self.log(&format!("{}: outcome notification: {failure}", found.key));
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
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Instant,
    };

    use jiff::{SignedDuration, civil::Time};
    use serde::Deserialize as _;
    use serde_json::json;

    use super::*;
    use crate::{
        app::review::{
            PullRequest,
            config::{Batches, ConfigFile, ReviewConfig},
            state::Found,
            with_worktree,
        },
        infra::{
            codex_app::new_chat_link,
            command::{RunTool, ToolCall},
            review_record::ReviewChat,
        },
        runtime::review::{Commands, CreateChat, Delivery, SendToChat, pause},
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
        head: Mutex<String>,
        delivery: Mutex<Delivery>,
        // Each prompt sent to a linked chat, after the chat.
        sent: Mutex<Vec<(String, String)>>,
        creation: Mutex<Created>,
        // Each prompt a new chat was started with, after its title.
        started: Mutex<Vec<(String, String)>>,
        inbox: Mutex<String>,
    }

    #[derive(Default)]
    struct Setup<'setup> {
        // Answered by the test itself, which may wait for the notification to close.
        alerter: Option<Alerter<'setup>>,
        pause: Pause<'setup>,
        now: Option<&'setup (dyn Fn() -> Timestamp + Sync)>,
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

        let worktree = fixture.worktree_at(HEAD);
        assert_eq!(
            fixture.started(),
            [fixture.review_chat_at(HEAD)],
            "a chat is tried first"
        );
        assert_eq!(fixture.opened(), [fixture.new_chat_at(HEAD)]);
        assert_eq!(fixture.sent(), []);
        let launched = fixture.state().requests["acme/app#12@30"].clone();
        assert_eq!(launched.choice.as_deref(), Some("code"));
        assert_eq!(launched.head_sha.as_deref(), Some(HEAD));
        assert_eq!(launched.worktree, Some(worktree.display().to_string()));
    }

    #[test]
    fn started_chat_runs_the_review_at_the_head_and_takes_the_prs_re_requests() {
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);
        fixture.set_creation(Created::Started("new-chat".to_owned()));

        fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("baseline poll");
            notifying(daemon, stop, || {
                fixture.request("acme/app", 12, &[requested(30, "sato")]);
                daemon.poll_once().expect("poll the request");
                fixture.wait_status("acme/app#12@30", Status::Launched);

                fixture.request(
                    "acme/app",
                    12,
                    &[requested(30, "sato"), requested(31, "sato")],
                );
                daemon.poll_once().expect("poll the re-request");
                fixture.wait_status("acme/app#12@31", Status::Launched);
            });
        });

        assert_eq!(fixture.started(), [fixture.review_chat_at(HEAD)]);
        assert_eq!(fixture.opened(), Vec::<String>::new());
        assert_eq!(
            fixture.state().requests["acme/app#12@30"].worktree,
            Some(fixture.worktree_at(HEAD).display().to_string())
        );
        assert_eq!(
            fixture.sent(),
            [("new-chat".to_owned(), fixture.worktree_prompt_at(HEAD))]
        );
        assert_eq!(fixture.count_calls("alerter "), 1);
    }

    #[test]
    fn chat_that_may_have_started_is_reported_without_opening_another() {
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);
        fixture.set_creation(Created::Unknown("unknown: no answer".to_owned()));

        fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("baseline poll");
            notifying(daemon, stop, || {
                fixture.request("acme/app", 12, &[requested(30, "sato")]);
                daemon.poll_once().expect("poll the request");
                fixture.wait_status("acme/app#12@30", Status::Failed);
            });
        });

        assert_eq!(fixture.started().len(), 1);
        assert_eq!(fixture.opened(), Vec::<String>::new());
        assert_eq!(
            fixture.count_calls(
                "レビューを開始できませんでした: the new review chat could not be confirmed"
            ),
            1
        );
        assert_eq!(
            review_record::load_chat(&fixture.paths, "acme/app", 12).expect("read the link"),
            None
        );
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
    fn re_request_of_a_linked_pr_goes_to_its_chat_at_the_head_without_asking_and_is_reported() {
        const LATER: &str = "fedcba9876543210fedcba9876543210fedcba98";
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);
        let first = [requested(30, "sato")];
        let second = [requested(30, "sato"), requested(31, "sato")];
        let third = [
            requested(30, "sato"),
            requested(31, "sato"),
            requested(32, "sato"),
        ];

        fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("baseline poll");
            notifying(daemon, stop, || {
                fixture.request("acme/app", 12, &first);
                daemon.poll_once().expect("poll the request");
                fixture.wait_status("acme/app#12@30", Status::Launched);
                fixture.link_chat("acme/app", 12, "review-chat");

                fixture.set_head(LATER);
                fixture.request("acme/app", 12, &second);
                daemon.poll_once().expect("poll the re-request");
                fixture.wait_status("acme/app#12@31", Status::Launched);

                fixture.request("acme/app", 12, &third);
                daemon
                    .poll_once()
                    .expect("poll the re-request of the same head");
                fixture.wait_status("acme/app#12@32", Status::Launched);
            });
        });

        assert_eq!(fixture.opened(), [fixture.new_chat_at(HEAD)]);
        let to_chat = ("review-chat".to_owned(), fixture.worktree_prompt_at(LATER));
        assert_eq!(fixture.sent(), [to_chat.clone(), to_chat]);
        let sent = fixture.state().requests["acme/app#12@31"].clone();
        assert_eq!(sent.head_sha.as_deref(), Some(LATER));
        assert_eq!(sent.choice.as_deref(), Some("code"));
        assert_eq!(
            fixture.count_calls("alerter "),
            1,
            "only the first request is asked"
        );
        assert_eq!(
            fixture.count_calls("再レビューを紐づけ済みのチャットへ送りました"),
            2
        );
    }

    #[test]
    fn automatic_send_leaves_an_unanswered_notification_of_another_pr_open() {
        let mut fixture = Fixture::new();
        fixture.config.max_notifications = 1;
        let closed = Mutex::new(Vec::new());
        let alerter = |call: &ToolCall<'_>| {
            let key = group_of(call);
            if key.starts_with("acme/app#12@") {
                return Ok(CODE_REVIEW.as_bytes().to_vec());
            }
            wait_for(|| call.stop.is_requested());
            closed.lock().expect("note the close").push(key);
            Err("alerter was stopped".to_owned())
        };

        let closed_before_the_send = fixture.with_daemon(
            &Setup {
                alerter: Some(&alerter),
                ..Setup::default()
            },
            |daemon, stop| {
                daemon.poll_once().expect("baseline poll");
                notifying(daemon, stop, || {
                    fixture.request("acme/app", 12, &[requested(30, "sato")]);
                    daemon.poll_once().expect("poll the request");
                    fixture.wait_status("acme/app#12@30", Status::Launched);
                    fixture.link_chat("acme/app", 12, "review-chat");

                    fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
                    daemon.poll_once().expect("poll the other PR");
                    fixture.wait_status("acme/app#1@10", Status::Notifying);

                    fixture.request(
                        "acme/app",
                        12,
                        &[requested(30, "sato"), requested(31, "sato")],
                    );
                    daemon.poll_once().expect("poll the re-request");
                    fixture.wait_status("acme/app#12@31", Status::Launched);
                    closed.lock().expect("read the closes").clone()
                })
            },
        );

        assert!(
            closed_before_the_send.is_empty(),
            "{closed_before_the_send:?}"
        );
        assert_eq!(fixture.sent().len(), 1);
    }

    #[test]
    fn re_request_of_a_linked_pr_without_an_earlier_review_asks_the_owner() {
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);
        fixture.link_chat("acme/app", 12, "review-chat");

        fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("baseline poll");
            notifying(daemon, stop, || {
                fixture.request("acme/app", 12, &[requested(30, "sato")]);
                daemon.poll_once().expect("poll");
                fixture.wait_status("acme/app#12@30", Status::Launched);
            });
        });

        assert_eq!(fixture.count_calls("alerter "), 1);
        assert_eq!(
            fixture.sent(),
            [("review-chat".to_owned(), fixture.worktree_prompt_at(HEAD))]
        );
        assert_eq!(
            fixture.count_calls("再レビューを紐づけ済みのチャットへ送りました"),
            0,
            "a review the owner chose is not reported again"
        );
    }

    #[test]
    fn failed_automatic_send_is_reported_without_a_new_chat_or_a_resend() {
        let fixture = Fixture::new();
        fixture.answer(CODE_REVIEW);

        let queued = fixture.with_daemon(&Setup::default(), |daemon, stop| {
            daemon.poll_once().expect("baseline poll");
            notifying(daemon, stop, || {
                fixture.request("acme/app", 12, &[requested(30, "sato")]);
                daemon.poll_once().expect("poll the request");
                fixture.wait_status("acme/app#12@30", Status::Launched);
                fixture.link_chat("acme/app", 12, "review-chat");
                fixture.set_delivery(Delivery::NotSent("not_sent: no bridge".to_owned()));

                fixture.request(
                    "acme/app",
                    12,
                    &[requested(30, "sato"), requested(31, "sato")],
                );
                daemon.poll_once().expect("poll the re-request");
                fixture.wait_status("acme/app#12@31", Status::Failed);
            });
            daemon.poll_once().expect("poll again");
            daemon.queued_count()
        });

        assert_eq!(queued, 0);
        assert_eq!(fixture.count_calls("alerter "), 1);
        assert_eq!(fixture.opened(), [fixture.new_chat_at(HEAD)]);
        assert_eq!(fixture.sent().len(), 1);
        assert_eq!(
            fixture.count_calls("再レビューを紐づけ済みのチャットへ送りました"),
            0
        );
        assert_eq!(
            fixture
                .count_calls("レビューを開始できませんでした: could not send to chat review-chat"),
            1
        );
    }

    #[test]
    fn failed_send_to_the_linked_chat_is_reported_without_a_new_chat_or_a_resend() {
        for (name, delivery, expected) in [
            (
                "not_sent",
                Delivery::NotSent("not_sent: no bridge".to_owned()),
                "could not send to chat review-chat: not_sent",
            ),
            (
                "unknown",
                Delivery::Unknown("unknown: no answer".to_owned()),
                "chat review-chat could not be confirmed",
            ),
        ] {
            let fixture = Fixture::new();
            fixture.answer(CODE_REVIEW);
            fixture.link_chat("acme/app", 12, "review-chat");
            fixture.set_delivery(delivery);

            let queued = fixture.with_daemon(&Setup::default(), |daemon, stop| {
                daemon.poll_once().expect("baseline poll");
                notifying(daemon, stop, || {
                    fixture.request("acme/app", 12, &[requested(30, "sato")]);
                    daemon.poll_once().expect("poll");
                    fixture.wait_status("acme/app#12@30", Status::Failed);
                    wait_for(|| fixture.ran("osascript"));
                });
                daemon.poll_once().expect("poll again");
                daemon.queued_count()
            });

            assert_eq!(queued, 0, "case: {name}");
            assert_eq!(fixture.sent().len(), 1, "case: {name}");
            assert!(fixture.opened().is_empty(), "case: {name}");
            let failed = fixture.state().requests["acme/app#12@30"].clone();
            assert!(
                failed
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected)),
                "case: {name}: {failed:?}"
            );
        }
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
                "reply",
                r#"{"activationType":"replied"}"#,
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
    fn clicked_notification_opens_the_pr_and_asks_again() {
        let fixture = Fixture::new();
        let shown = AtomicUsize::new(0);
        let alerter = |_: &ToolCall<'_>| {
            let answer = match shown.fetch_add(1, Ordering::SeqCst) {
                0 => r#"{"activationType":"contentsClicked"}"#,
                _ => CODE_REVIEW,
            };
            Ok(answer.as_bytes().to_vec())
        };

        fixture.with_daemon(
            &Setup {
                alerter: Some(&alerter),
                ..Setup::default()
            },
            |daemon, stop| {
                daemon.poll_once().expect("baseline poll");
                notifying(daemon, stop, || {
                    fixture.request("acme/app", 12, &[requested(30, "sato")]);
                    daemon.poll_once().expect("poll");
                    fixture.wait_status("acme/app#12@30", Status::Launched);
                });
            },
        );

        assert_eq!(shown.load(Ordering::SeqCst), 2);
        assert_eq!(
            fixture.opened(),
            [
                "https://github.com/acme/app/pull/12".to_owned(),
                fixture.new_chat_at(HEAD),
            ]
        );
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
        assert_eq!(fixture.opened(), Vec::<String>::new());
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
        assert!(paused.contains(&Duration::from_secs(30)), "{paused:?}");
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
            [60, 120, 240].map(Duration::from_secs)
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
        fixture.save_queued(&[(1, 10)]);
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
                ..Setup::default()
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
        assert_eq!(fixture.opened(), Vec::<String>::new());
    }

    #[test]
    fn replaced_program_ends_the_daemon_before_the_next_poll() {
        let fixture = Fixture::new();
        let run = |call: &ToolCall<'_>| fixture.run(call);
        let send = |thread: &str, prompt: &str, _: &StopRequest| fixture.deliver(thread, prompt);
        let create = |chat: &NewChat<'_>, _: &StopRequest| fixture.create(chat);
        let tools = fixture.tools(&run, &send, &create);
        let stop = StopRequest::default();
        let zone = TimeZone::UTC;
        let daemon = Daemon::new(
            DaemonDeps {
                tools: &tools,
                paths: &fixture.paths,
                log: &|_| {},
                now: &Timestamp::now,
                zone: &zone,
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

    #[test]
    fn batch_mode_queues_new_requests_without_notifying_them() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(1, 10)]);
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
        let now = || tokyo("2026-11-18T09:00:00");

        let ended = fixture.with_daemon(
            &Setup {
                pause: Pause::Real,
                now: Some(&now),
                ..Setup::default()
            },
            |daemon, stop| {
                thread::scope(|scope| {
                    let run = scope.spawn(|| daemon.run());
                    wait_for(|| fixture.ran("gh api --paginate -X GET notifications"));
                    thread::sleep(Duration::from_millis(200));
                    stop.request();
                    run.join().expect("join the daemon")
                })
            },
        );

        assert_eq!(ended, Ok(()));
        assert_eq!(fixture.count_calls("alerter "), 0);
        assert_eq!(fixture.status("acme/app#1@10"), Status::Queued);
    }

    #[test]
    fn scheduled_batch_starts_my_reviews_and_reports_the_others_at_the_ready_time() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(1, 10), (2, 20), (3, 30), (4, 40)]);
        fixture.answer_with("gh api --paginate repos/acme/app/pulls/2/reviews", |_| {
            Ok(br#"[{"user":{"login":"sato","type":"User"},"state":"APPROVED"}]"#.to_vec())
        });
        fixture.answer_with("gh api repos/acme/app/pulls/3/requested_reviewers", |_| {
            Ok(
                br#"{"users":[{"login":"sato","type":"User"},{"login":"suzuki","type":"User"}]}"#
                    .to_vec(),
            )
        });
        fixture.answer_with("gh api repos/acme/app/pulls/4/requested_reviewers", |_| {
            Err("HTTP 502".to_owned())
        });
        let clock = Mutex::new(tokyo("2026-11-18T10:51:00"));
        let now = || *clock.lock().expect("read the clock");

        let shown_before_the_ready_time = fixture.with_daemon(
            &Setup {
                now: Some(&now),
                ..Setup::default()
            },
            |daemon, _| {
                daemon.batch_once();
                let shown = fixture.count_calls("osascript ");
                *clock.lock().expect("move the clock") = tokyo("2026-11-18T11:00:00");
                daemon.batch_once();
                daemon.batch_once();
                shown
            },
        );

        let statuses = [
            "acme/app#1@10",
            "acme/app#2@20",
            "acme/app#3@30",
            "acme/app#4@40",
        ]
        .map(|key| fixture.status(key));
        assert_eq!(
            statuses,
            [
                Status::Launched,
                Status::Reported,
                Status::Reported,
                Status::Launched
            ]
        );
        assert_eq!(
            fixture.state().last_batch,
            Some(tokyo("2026-11-18T10:50:00"))
        );
        assert_eq!(shown_before_the_ready_time, 0);
        assert_eq!(fixture.count_calls("alerter "), 0);
        assert_eq!(fixture.count_calls("osascript "), 1, "the slot runs once");
        assert_eq!(
            fixture.count_calls(
                "#1 PR 1\n#4 PR 4\n報告のみ: #2 PR 2（他の人が承認済み）\n報告のみ: #3 PR 3（3人以上を指名） pr-wake レビュー 4件"
            ),
            1
        );
        assert_eq!(fixture.state().pending_digests, []);
    }

    #[test]
    fn batch_is_reported_in_a_chat_that_links_each_review_chat() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(1, 10)]);
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
        fixture.set_creation(Created::Started("review-chat".to_owned()));
        let now = || tokyo("2026-11-18T11:30:00");

        fixture.with_daemon(
            &Setup {
                now: Some(&now),
                ..Setup::default()
            },
            |daemon, _| daemon.batch_once(),
        );

        let started = fixture.started();
        let titles = started
            .iter()
            .map(|(title, _)| title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles, ["Review｜T PR#1", "レビュー報告｜11/18 11:00"]);
        let (_, report) = &started[1];
        assert!(
            report
                .contains("- [#1 PR 1](https://github.com/acme/app/pull/1) スレッドID review-chat"),
            "{report}"
        );
        assert_eq!(fixture.count_calls("osascript "), 0);
        assert_eq!(fixture.state().pending_digests, []);
    }

    #[test]
    fn batch_notification_cut_short_by_a_stop_is_shown_once_after_the_restart() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(1, 10)]);
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
        let now = || tokyo("2026-11-18T11:30:00");
        let setup = Setup {
            now: Some(&now),
            ..Setup::default()
        };
        fixture.answer_with("osascript", |call| {
            call.stop.request();
            Err("stopped before osascript started".to_owned())
        });

        fixture.with_daemon(&setup, |daemon, _| daemon.batch_once());
        let kept = fixture.state().pending_digests.len();
        fixture.overrides.lock().expect("drop the stop").clear();
        fixture.with_daemon(&setup, |daemon, _| {
            daemon.batch_once();
            daemon.batch_once();
        });

        assert_eq!(kept, 1);
        assert_eq!(fixture.count_calls("#1 PR 1 pr-wake レビュー 1件"), 2);
        assert_eq!(fixture.state().pending_digests, []);
        assert_eq!(fixture.opened().len(), 1, "launched once");
    }

    #[test]
    fn batch_notification_survives_a_restart_before_its_ready_time() {
        for (name, resumed, shown_on_resume) in [
            ("resumed_before_ready", "2026-11-18T10:55:00", 0),
            ("resumed_after_ready", "2026-11-18T11:30:00", 1),
        ] {
            let fixture = Fixture::batched();
            fixture.save_queued(&[(1, 10), (2, 20)]);
            fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
            fixture.request("acme/app", 2, &[requested(20, "tanaka")]);
            fixture.answer_with("gh api --paginate repos/acme/app/pulls/2/reviews", |_| {
                Ok(br#"[{"user":{"login":"sato","type":"User"},"state":"APPROVED"}]"#.to_vec())
            });
            let clock = Mutex::new(tokyo("2026-11-18T10:51:00"));
            let now = || *clock.lock().expect("read the clock");
            let setup = Setup {
                now: Some(&now),
                ..Setup::default()
            };

            fixture.with_daemon(&setup, |daemon, _| daemon.batch_once());
            *clock.lock().expect("move the clock") = tokyo(resumed);
            let shown = fixture.with_daemon(&setup, |daemon, _| {
                daemon.poll_once().expect("poll after the restart");
                daemon.batch_once();
                let shown = fixture.count_calls("osascript ");
                *clock.lock().expect("move the clock") = tokyo("2026-11-18T11:45:00");
                daemon.batch_once();
                shown
            });

            assert_eq!(shown, shown_on_resume, "case: {name}");
            assert_eq!(fixture.count_calls("osascript "), 1, "case: {name}");
            assert_eq!(
                fixture.count_calls(
                    "#1 PR 1\n報告のみ: #2 PR 2（他の人が承認済み） pr-wake レビュー 2件"
                ),
                1,
                "case: {name}"
            );
            assert_eq!(fixture.opened().len(), 1, "case: {name}: launched once");
            assert_eq!(
                [
                    fixture.status("acme/app#1@10"),
                    fixture.status("acme/app#2@20")
                ],
                [Status::Launched, Status::Reported],
                "case: {name}"
            );
            assert_eq!(fixture.state().pending_digests, [], "case: {name}");
        }
    }

    #[test]
    fn missed_slot_runs_once_and_not_again_after_a_restart() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(1, 10)]);
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
        let now = || tokyo("2026-11-18T13:00:00");
        let setup = Setup {
            now: Some(&now),
            ..Setup::default()
        };

        fixture.with_daemon(&setup, |daemon, _| {
            daemon.batch_once();
        });
        fixture.request("acme/app", 5, &[requested(50, "sato")]);
        fixture.with_daemon(&setup, |daemon, _| {
            daemon.poll_once().expect("poll the new request");
            daemon.batch_once();
        });

        assert_eq!(fixture.status("acme/app#1@10"), Status::Launched);
        assert_eq!(fixture.status("acme/app#5@50"), Status::Queued);
        assert_eq!(fixture.count_calls("osascript "), 1);
    }

    #[test]
    fn review_now_starts_the_queued_requests_or_one_pr_whatever_its_reviewers() {
        for (name, pr, expected, notified) in [
            ("queued", None, [Status::Launched, Status::Reported], 1),
            (
                "one_reported_pr",
                Some("acme/app#2"),
                [Status::Queued, Status::Launched],
                1,
            ),
            (
                "unknown_pr",
                Some("acme/app#9"),
                [Status::Queued, Status::Reported],
                0,
            ),
        ] {
            let fixture = Fixture::batched();
            fixture.save_queued(&[(1, 10), (2, 20)]);
            let mut state = fixture.state();
            state
                .requests
                .get_mut("acme/app#2@20")
                .expect("request")
                .status = Status::Reported;
            review_record::save_state(&fixture.paths, &state).expect("save the state");
            fixture.answer_with("gh api repos/acme/app/pulls/2/requested_reviewers", |_| {
                Ok(br#"{"users":[{"login":"sato"},{"login":"suzuki"}]}"#.to_vec())
            });
            // A Saturday, so no scheduled batch runs beside the request.
            let now = || tokyo("2026-11-21T12:00:00");
            review_record::request_now(
                &fixture.paths,
                &NowRequest {
                    pr: pr.map(str::to_owned),
                },
                Timestamp::now(),
            )
            .expect("ask for the review");

            fixture.with_daemon(
                &Setup {
                    now: Some(&now),
                    ..Setup::default()
                },
                |daemon, _| daemon.batch_once(),
            );

            let statuses = ["acme/app#1@10", "acme/app#2@20"].map(|key| fixture.status(key));
            assert_eq!(statuses, expected, "case: {name}");
            assert_eq!(fixture.count_calls("osascript "), notified, "case: {name}");
            assert_eq!(
                review_record::take_now_requests(&fixture.paths)
                    .expect("read the requests")
                    .len(),
                0,
                "case: {name}"
            );
        }
    }

    #[test]
    fn poll_in_batch_mode_clears_github_notifications_of_requested_prs_but_mentions() {
        for (name, batched, failing_unsubscribe, expected) in [
            (
                "batched",
                true,
                false,
                &[
                    "gh api -X DELETE notifications/threads/1/subscription",
                    "gh api -X DELETE notifications/threads/1",
                    "gh api -X DELETE notifications/threads/5/subscription",
                    "gh api -X DELETE notifications/threads/5",
                ][..],
            ),
            (
                "failing_unsubscribe",
                true,
                true,
                &[
                    "gh api -X DELETE notifications/threads/1/subscription",
                    "gh api -X DELETE notifications/threads/5/subscription",
                ][..],
            ),
            ("without_batches", false, false, &[][..]),
        ] {
            let fixture = if batched {
                Fixture::batched()
            } else {
                Fixture::new()
            };
            fixture.save_queued(&[(12, 30)]);
            fixture.request("acme/app", 12, &[requested(30, "sato")]);
            let thread = |id: u64, reason: &str, kind: &str, path: &str| {
                json!({
                    "id": id.to_string(),
                    "reason": reason,
                    "subject": {
                        "type": kind,
                        "url": format!("https://api.github.com/repos/{path}"),
                    },
                })
            };
            *fixture.inbox.lock().expect("set the inbox") = json!([
                thread(1, "review_requested", "PullRequest", "acme/app/pulls/12"),
                thread(2, "mention", "PullRequest", "acme/app/pulls/12"),
                thread(3, "review_requested", "PullRequest", "acme/app/pulls/13"),
                thread(4, "team_mention", "PullRequest", "acme/app/pulls/12"),
                thread(5, "comment", "PullRequest", "Acme/App/pulls/12"),
                thread(6, "subscribed", "Issue", "acme/app/issues/12"),
            ])
            .to_string();
            if failing_unsubscribe {
                fixture.answer_with("gh api -X DELETE", |_| Err("HTTP 404".to_owned()));
            }

            let polled = fixture.with_daemon(&Setup::default(), |daemon, _| daemon.poll_once());

            assert_eq!(polled, Ok(()), "case: {name}");
            assert_eq!(
                fixture.calls_starting("gh api -X DELETE"),
                expected,
                "case: {name}"
            );
        }
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
                    "prompt": "[${{.Skill}}]({{.SkillPath}}) {{.PR.URL}} {{.PR.HeadSHA}}",
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
                head: Mutex::new(HEAD.to_owned()),
                delivery: Mutex::new(Delivery::Sent),
                sent: Mutex::new(Vec::new()),
                creation: Mutex::new(Created::NotSent("no harnexus in the test".to_owned())),
                started: Mutex::new(Vec::new()),
                inbox: Mutex::new("[]".to_owned()),
            }
        }

        fn batched() -> Self {
            let mut fixture = Self::new();
            fixture.config.batches = Some(Batches {
                ready_at: vec![Time::constant(11, 0, 0, 0), Time::constant(16, 0, 0, 0)],
                lead: SignedDuration::from_mins(10),
            });
            fixture
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
            let send = |thread: &str, prompt: &str, _: &StopRequest| self.deliver(thread, prompt);
            let create = |chat: &NewChat<'_>, _: &StopRequest| self.create(chat);
            let tools = self.tools(&run, &send, &create);
            let stop = StopRequest::default();
            let real = |wait| pause(&stop, wait);
            let state = review_record::load_state(&self.paths)
                .expect("read the state")
                .unwrap_or_default();
            let zone = TimeZone::fixed(jiff::tz::offset(9));
            let daemon = Daemon::new(
                DaemonDeps {
                    tools: &tools,
                    paths: &self.paths,
                    log: &|_| {},
                    now: setup.now.unwrap_or(&Timestamp::now),
                    zone: &zone,
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

        fn tools<'run>(
            &self,
            run: &'run RunTool<'run>,
            send: &'run SendToChat<'run>,
            create: &'run CreateChat<'run>,
        ) -> Tools<'run> {
            Tools {
                run,
                send,
                create,
                home: self.home.path().to_owned(),
                paths: self.paths.clone(),
                commands: Commands {
                    gh: PathBuf::from("gh"),
                    git: PathBuf::from("git"),
                    alerter: PathBuf::from("alerter"),
                    open: PathBuf::from("open"),
                    osascript: PathBuf::from("osascript"),
                    plutil: PathBuf::from("plutil"),
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
            if line.starts_with("gh api --paginate -X GET notifications") {
                return Ok(self
                    .inbox
                    .lock()
                    .expect("read the inbox")
                    .clone()
                    .into_bytes());
            }
            if line.ends_with("/requested_reviewers") {
                return Ok(br#"{"users":[]}"#.to_vec());
            }
            if line.ends_with("/reviews?per_page=100") {
                return Ok(b"[]".to_vec());
            }
            if let Some(path) = line.strip_prefix("gh api --paginate ") {
                return self.events_of(path);
            }
            if let Some(path) = line.strip_prefix("gh api repos/") {
                let parts = path.split('/').collect::<Vec<_>>();
                let head = self.head.lock().expect("read the head").clone();
                return Ok(
                    pull_json(&format!("{}/{}", parts[0], parts[1]), parts[3], &head).into_bytes(),
                );
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
                "gh api -X DELETE ".to_owned(),
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

        fn save_queued(&self, requests: &[(u64, u64)]) {
            let now = Timestamp::now();
            let mut state = ReviewState::default();
            state.apply_poll(
                requests
                    .iter()
                    .map(|&(number, event)| Found {
                        key: format!("acme/app#{number}@{event}"),
                        repo: "acme/app".to_owned(),
                        number,
                        title: format!("PR {number}"),
                        requester: Some("tanaka".to_owned()),
                        team: None,
                    })
                    .collect(),
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

        fn calls_starting(&self, prefix: &str) -> Vec<String> {
            self.calls
                .lock()
                .expect("read the calls")
                .iter()
                .filter(|line| line.starts_with(prefix))
                .cloned()
                .collect()
        }

        fn count_calls(&self, text: &str) -> usize {
            self.calls
                .lock()
                .expect("read the calls")
                .iter()
                .filter(|line| line.contains(text))
                .count()
        }

        fn opened(&self) -> Vec<String> {
            self.opened.lock().expect("read the links").clone()
        }

        fn deliver(&self, thread: &str, prompt: &str) -> Delivery {
            self.sent
                .lock()
                .expect("note the send")
                .push((thread.to_owned(), prompt.to_owned()));
            self.delivery.lock().expect("read the delivery").clone()
        }

        fn sent(&self) -> Vec<(String, String)> {
            self.sent.lock().expect("read the sends").clone()
        }

        fn create(&self, chat: &NewChat<'_>) -> Created {
            self.started
                .lock()
                .expect("note the new chat")
                .push((chat.title.to_owned(), chat.prompt.to_owned()));
            self.creation.lock().expect("read the creation").clone()
        }

        fn started(&self) -> Vec<(String, String)> {
            self.started.lock().expect("read the new chats").clone()
        }

        fn set_creation(&self, creation: Created) {
            *self.creation.lock().expect("set the creation") = creation;
        }

        fn link_chat(&self, repo: &str, number: u64, thread: &str) {
            review_record::record_chat(
                &self.paths,
                &ReviewChat {
                    repo: repo.to_owned(),
                    number,
                    thread: thread.to_owned(),
                    linked_at: "2026-11-13T00:00:00.000Z".to_owned(),
                },
            )
            .expect("link the chat");
        }

        fn set_head(&self, head: &str) {
            head.clone_into(&mut self.head.lock().expect("set the head"));
        }

        fn set_delivery(&self, delivery: Delivery) {
            *self.delivery.lock().expect("set the delivery") = delivery;
        }

        // The prompt of the code choice for PR 12 at `head`.
        fn prompt_at(&self, head: &str) -> String {
            let skill = self.home.path().join(".codex/skills/code-review/SKILL.md");
            format!(
                "[$code-review]({}) https://github.com/acme/app/pull/12 {head}",
                skill.display()
            )
        }

        fn worktree_at(&self, head: &str) -> PathBuf {
            self.home.path().join(format!(
                ".local/share/pr-wake/worktrees/acme/app/pr-12-{}",
                head.get(..12).unwrap_or_default()
            ))
        }

        fn worktree_prompt_at(&self, head: &str) -> String {
            with_worktree(
                &self.prompt_at(head),
                &self.worktree_at(head),
                &PullRequest {
                    repo: "acme/app".to_owned(),
                    number: 12,
                    title: String::new(),
                    url: String::new(),
                    head_sha: head.to_owned(),
                    base_ref: String::new(),
                },
            )
        }

        // The title and the prompt of a new review chat for PR 12 at `head`.
        fn review_chat_at(&self, head: &str) -> (String, String) {
            ("Review｜T PR#12".to_owned(), self.worktree_prompt_at(head))
        }

        fn new_chat_at(&self, head: &str) -> String {
            new_chat_link(
                &self.worktree_prompt_at(head),
                &self.home.path().join("review-home"),
            )
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

    fn tokyo(local: &str) -> Timestamp {
        format!("{local}+09:00").parse().expect("parse the time")
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

    fn pull_json(repo: &str, number: &str, head: &str) -> String {
        json!({
            "number": number.parse::<u64>().expect("a number"),
            "title": "T",
            "html_url": format!("https://github.com/{repo}/pull/{number}"),
            "additions": 12,
            "deletions": 3,
            "changed_files": 2,
            "head": {"sha": head},
            "base": {"ref": "main", "repo": {"full_name": repo}},
        })
        .to_string()
    }
}
