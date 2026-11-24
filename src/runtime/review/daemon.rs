use std::{
    collections::HashMap,
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    thread::{self, Scope},
    time::{Duration, SystemTime},
};

use jiff::{Timestamp, tz::TimeZone};

use super::{Launched, Route, STOP_CHECK, Tools, ask_owner, github, launch};
use crate::{
    app::review::{
        PullRequest,
        batch::{Digest, Listed, NowRequest, Plan, ReportReason, due_slot, plan},
        config::Choice,
        inbox::threads_to_clear,
        notice::Selection,
        parse_pr_ref,
        request::{
            SearchHit, current_request, pending_requests, pr_key, request_queries, still_requested,
        },
        selection::{SelectedOutcome, SelectionRecord, is_selection_id},
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
    // Names this run of the review, which a selection must be handed to.
    pub(super) run: &'deps str,
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
    // The latest record of each selection whose save failed, written again on later ticks so its known outcomes are not lost.
    unsaved: Mutex<HashMap<String, SelectionRecord>>,
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
    Started(String),
    // The reason no chat could be started, so the prompt waits in a new chat's composer.
    Opened(Option<String>),
    // The chat is kept when the review went to one, as a started chat exists even if its link failed.
    Failed { error: String, chat: Option<String> },
    AlreadyReviewed,
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
// Long enough for a lost answer to be read again, short enough that the records stay few.
const SELECTION_RETENTION: Duration = Duration::from_hours(7 * 24);

impl<'deps> Daemon<'deps> {
    pub(super) fn new(deps: DaemonDeps<'deps>, state: ReviewState) -> Self {
        Self {
            deps,
            state: Mutex::new(state),
            shown: Mutex::new(Shown::default()),
            wake: Wake::default(),
            launching: Mutex::new(()),
            unsaved: Mutex::new(HashMap::new()),
        }
    }

    // Runs until a stop request, with every notification closed and every launch ended.
    pub(super) fn run(&self) -> Result<(), String> {
        self.recover()?;
        thread::scope(|scope| {
            if self.deps.tools.config.batches.is_some() {
                scope.spawn(|| self.batch_loop());
            } else {
                scope.spawn(|| self.notify_loop(scope));
            }
            self.poll_loop()
        })
    }

    pub(super) fn recover(&self) -> Result<(), String> {
        let mut state = self.lock_state();
        state.recover((self.deps.now)());
        // The cache names each PR's request as the previous config resolved it, so the first poll reads them all again.
        state.prs.clear();
        let saved = self.save(&state);
        drop(state);
        saved?;
        self.close_interrupted_selections();
        Ok(())
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
        // Only a batch queues requests for a later choice, so only then is each one named after the reviewers GitHub asks now.
        let live_targets = config.batches.is_some();
        let pending = pending_requests(hits, &cache, &login, &config.teams, |hit| {
            let requests = github.review_requests(&hit.repo, hit.number)?;
            let live = live_targets
                .then(|| github.live_request(&hit.repo, hit.number))
                .transpose()?;
            Ok((requests, live))
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
        self.take_selections();

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
            self.plan_and_start(&request, &login, &mut digest);
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
        // `review now --pr` starts the PR whatever was reviewed before.
        self.start_listed(&request, &mut digest, false);
        self.keep_digest(&mut None, (self.deps.now)(), &digest);
    }

    // A selection's record is its own handoff, so the caller's one write is all it takes and a caller that dies after it loses nothing.
    fn take_selections(&self) {
        self.save_unsaved_selections();
        let ids = match review_record::selection_ids(self.deps.paths) {
            Ok(ids) => ids,
            Err(error) => {
                self.log(&format!("selections: {error}"));
                return;
            }
        };
        for (id, _) in ids {
            if self.deps.stop.is_requested() {
                return;
            }
            // An unreadable record is reported to whoever reads it, so it is not logged here every second.
            if let Ok(Some(record)) = review_record::load_selection(self.deps.paths, &id)
                && !record.accepted
                && !record.complete
                && is_selection_id(&id)
                && !self
                    .unsaved
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains_key(&id)
            {
                self.start_selected(&id, record);
            }
        }
    }

    // Only the chosen requests still queued go through the batch's decision, so a stale choice never starts a second review.
    fn start_selected(&self, id: &str, mut record: SelectionRecord) {
        // A selection made for an earlier run waited while no review ran, so it is closed with nothing started.
        if record.run != self.deps.run {
            record.close(&SelectedOutcome::Stopped);
            self.log(&format!(
                "selection {id}: made for the earlier run {}, so nothing is started",
                record.run
            ));
            self.save_selection(id, &record);
            return;
        }
        record.accepted = true;
        // Nothing starts untracked, so a selection whose acceptance cannot be written stays waiting.
        if let Err(error) = review_record::save_selection(self.deps.paths, id, &record) {
            self.log(&format!("selection {id}: not started: {error}"));
            return;
        }
        let login = self.lock_state().login.clone().unwrap_or_default();
        for key in record.keys.clone() {
            if self.deps.stop.is_requested() {
                break;
            }
            let outcome = match self.queued_request(&key) {
                Err(outcome) => outcome,
                Ok(request) => {
                    // The plan reads GitHub while the request stays queued, so a poll meanwhile can still withdraw or replace it.
                    let plan = self.plan(&request, &login);
                    if let Err(outcome) = self.verify_selected(&key, &login) {
                        outcome
                    } else {
                        record.in_flight = Some(key.clone());
                        if let Err(error) =
                            review_record::save_selection(self.deps.paths, id, &record)
                        {
                            self.log(&format!("selection {id}: {key} not started: {error}"));
                            record.in_flight = None;
                            break;
                        }
                        match self.claim_selected(&key) {
                            // The Skill returns each outcome in the conversation it was chosen from, so no report chat is kept for a selection.
                            Ok(request) => self.apply_plan(&request, plan, &mut Digest::default()),
                            Err(outcome) => outcome,
                        }
                    }
                }
            };
            self.log(&format!("selection {id}: {key}: {outcome:?}"));
            record.in_flight = None;
            record.outcomes.insert(key, outcome);
            self.save_selection(id, &record);
        }
        record.close(&SelectedOutcome::Stopped);
        self.save_selection(id, &record);
    }

    fn queued_request(&self, key: &str) -> Result<Request, SelectedOutcome> {
        let state = self.lock_state();
        let newer = state.newer_request(key).map(str::to_owned);
        match state.requests.get(key) {
            Some(request) if request.status == Status::Queued && newer.is_none() => {
                Ok(request.clone())
            }
            request => Err(SelectedOutcome::NotQueued {
                status: request.map(|request| request.status),
                newer,
            }),
        }
    }

    // Reads GitHub without the poll's cache, as the queue the owner chose from may predate a withdrawal or a re-request.
    fn verify_selected(&self, key: &str, login: &str) -> Result<(), SelectedOutcome> {
        let found = self.queued_request(key)?.found;
        let unverified = |error: String| SelectedOutcome::Failed {
            error: format!("could not check the request on GitHub, so it stays queued: {error}"),
            thread: None,
        };
        if login.is_empty() {
            return Err(unverified("the login is unknown".to_owned()));
        }
        let github = github(self.deps.tools, self.deps.stop);
        let teams = &self.deps.tools.config.teams;
        let live = github
            .live_request(&found.repo, found.number)
            .map_err(unverified)?;
        let events = github
            .review_requests(&found.repo, found.number)
            .map_err(unverified)?;
        let hit = SearchHit {
            repo: found.repo.clone(),
            number: found.number,
            title: found.title.clone(),
            updated_at: String::new(),
        };
        let current = current_request(&events, Some(&live), &hit, login, teams);
        if current.key != found.key {
            return Err(SelectedOutcome::NotQueued {
                status: None,
                newer: Some(current.key),
            });
        }
        if !still_requested(&live, &current, login, teams) {
            return Err(SelectedOutcome::NotQueued {
                status: Some(Status::Withdrawn),
                newer: None,
            });
        }
        Ok(())
    }

    fn claim_selected(&self, key: &str) -> Result<Request, SelectedOutcome> {
        // A poll that supersedes or withdraws the request takes it out of the queue under the same lock, so the claim refuses it.
        if let Some(request) = self.claim_key(key.to_owned(), &[Status::Queued]) {
            return Ok(request);
        }
        let status = self
            .lock_state()
            .requests
            .get(key)
            .map(|request| request.status);
        if status == Some(Status::Queued) {
            return Err(SelectedOutcome::Failed {
                error: "the state could not be saved, so it stays queued".to_owned(),
                thread: None,
            });
        }
        Err(SelectedOutcome::NotQueued {
            status,
            newer: None,
        })
    }

    // A selection an earlier run left open is closed once and never resumed, as its launch may already have happened.
    fn close_interrupted_selections(&self) {
        let ids = match review_record::selection_ids(self.deps.paths) {
            Ok(ids) => ids,
            Err(error) => {
                self.log(&format!("selections: {error}"));
                return;
            }
        };
        let now = SystemTime::now();
        for (id, written) in ids {
            let mut record = match review_record::load_selection(self.deps.paths, &id) {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(error) => {
                    self.log(&format!("selection {id}: {error}"));
                    continue;
                }
            };
            if record.complete {
                let old = written
                    .and_then(|written| now.duration_since(written).ok())
                    .is_some_and(|age| age > SELECTION_RETENTION);
                if old && let Err(error) = review_record::remove_selection(self.deps.paths, &id) {
                    self.log(&format!("selection {id}: {error}"));
                }
                continue;
            }
            // One made for this run is still to be taken.
            if record.run == self.deps.run {
                continue;
            }
            // A request the recovery queued again never reached its launch, so only another may have started.
            let in_flight = record.in_flight.as_ref().and_then(|key| {
                self.lock_state()
                    .requests
                    .get(key)
                    .map(|request| request.status)
            });
            record.close(if in_flight == Some(Status::Queued) {
                &SelectedOutcome::Stopped
            } else {
                &SelectedOutcome::Unconfirmed
            });
            self.log(&format!("selection {id}: closed after the review ended"));
            self.save_selection(&id, &record);
        }
    }

    // Only the batch thread writes a selection after it is taken, so the record kept here is never older than the file.
    fn save_selection(&self, id: &str, record: &SelectionRecord) {
        let mut unsaved = self.unsaved.lock().unwrap_or_else(PoisonError::into_inner);
        match review_record::save_selection(self.deps.paths, id, record) {
            Ok(()) => {
                unsaved.remove(id);
            }
            Err(error) => {
                self.log(&format!("selection {id}: {error}"));
                unsaved.insert(id.to_owned(), record.clone());
            }
        }
    }

    fn save_unsaved_selections(&self) {
        let unsaved = self
            .unsaved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for (id, record) in unsaved {
            self.save_selection(&id, &record);
        }
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

    fn plan_and_start(
        &self,
        request: &Request,
        login: &str,
        digest: &mut Digest,
    ) -> SelectedOutcome {
        self.apply_plan(request, self.plan(request, login), digest)
    }

    fn apply_plan(&self, request: &Request, plan: Plan, digest: &mut Digest) -> SelectedOutcome {
        let key = &request.found.key;
        match plan {
            Plan::Report(reason) => {
                self.log_saved(self.update(key, |request| request.status = Status::Reported));
                self.log(&format!("{key}: only reported ({reason:?})"));
                digest.reported.push((Listed::of(&request.found), reason));
                SelectedOutcome::Reported { reason }
            }
            Plan::Start => match self.start_listed(request, digest, true) {
                Outcome::Started(thread) => SelectedOutcome::Started { thread },
                Outcome::SentToLinked(thread) => SelectedOutcome::SentToLinked { thread },
                Outcome::Opened(reason) => SelectedOutcome::Opened { reason },
                Outcome::AlreadyReviewed => SelectedOutcome::Reported {
                    reason: ReportReason::AlreadyReviewed,
                },
                Outcome::Failed { error, chat } => SelectedOutcome::Failed {
                    error,
                    thread: chat,
                },
            },
        }
    }

    fn start_listed(&self, request: &Request, digest: &mut Digest, skip_reviewed: bool) -> Outcome {
        let choices = &self.deps.tools.config.choices;
        // No notification asks for a choice in a batch, so a linked PR keeps its earlier one and any other takes the first, which the config always has.
        let choice = self.linked_choice(request).unwrap_or(&choices[0]);
        let outcome = self.start(request, choice, skip_reviewed);
        match &outcome {
            Outcome::Failed { .. } => digest.failed += 1,
            Outcome::AlreadyReviewed => digest
                .reported
                .push((Listed::of(&request.found), ReportReason::AlreadyReviewed)),
            Outcome::Opened(_) => digest.opened.push(Listed::of(&request.found)),
            Outcome::SentToLinked(chat) | Outcome::Started(chat) => {
                digest.started.push(Listed {
                    chat: Some(chat.clone()),
                    request: Some(request.found.key.clone()),
                    ..Listed::of(&request.found)
                });
            }
        }
        outcome
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
            self.start(request, choice, false);
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
        if matches!(self.start(request, choice, false), Outcome::SentToLinked(_)) {
            self.report(request, "再レビューを紐づけ済みのチャットへ送りました");
        }
    }

    // `skip_reviewed` reports a head a newer launched request already reviewed instead of starting it again.
    fn start(&self, request: &Request, choice: &Choice, skip_reviewed: bool) -> Outcome {
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
            return Outcome::Failed { error, chat: None };
        }

        self.log(&format!("{key}: chose {}", choice.id));
        let launched = {
            let _one_at_a_time = self
                .launching
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let reviewed =
                |pr: &PullRequest| skip_reviewed && self.reviewed_at(request, &pr.head_sha);
            let launched = launch(self.deps.tools, choice, found, &reviewed, self.deps.stop);
            // Saved before the lock is released, so the next launch in this process already sees its proof.
            self.record_launched(key, &launched);
            launched
        };

        let head = launched.pr.map_or_default(|pr| pr.head_sha);
        let worktree = launched
            .worktree
            .map_or_default(|worktree| worktree.to_string_lossy().into_owned());
        match (launched.result, launched.route) {
            (Ok(()), Some(Route::Linked(chat))) => {
                self.log(&format!(
                    "{key}: sent to chat {chat} for {worktree} at {head}"
                ));
                Outcome::SentToLinked(chat)
            }
            (Ok(()), Some(Route::Started(chat))) => {
                self.log(&format!("{key}: started chat {chat} at {head}"));
                Outcome::Started(chat)
            }
            (Ok(()), Some(Route::Opened(reason))) => {
                self.log(&format!(
                    "{key}: opened {worktree} at {head}, as no chat could be started: {reason}"
                ));
                Outcome::Opened(Some(reason))
            }
            (Ok(()), Some(Route::AlreadyReviewed)) => {
                self.log(&format!(
                    "{key}: only reported, as a newer request's review already started at {head}"
                ));
                Outcome::AlreadyReviewed
            }
            (Ok(()), None) => {
                self.log(&format!("{key}: opened {worktree} at {head}"));
                Outcome::Opened(None)
            }
            (Err(error), route) => {
                self.log(&format!("{key}: launch failed: {error}"));
                self.report_failure(request, &error);
                let chat = match route {
                    Some(Route::Linked(chat) | Route::Started(chat)) => Some(chat),
                    Some(Route::Opened(_) | Route::AlreadyReviewed) | None => None,
                };
                Outcome::Failed { error, chat }
            }
        }
    }

    fn record_launched(&self, key: &str, launched: &Launched) {
        let head = launched.pr.as_ref().map(|pr| pr.head_sha.clone());
        let worktree = launched
            .worktree
            .as_ref()
            .map(|worktree| worktree.to_string_lossy().into_owned());
        let already_reviewed = launched.route == Some(Route::AlreadyReviewed);
        // A confirmed new chat has begun even if its link failed afterwards, while a linked chat has it only once the send is confirmed.
        let review_started = match &launched.route {
            Some(Route::Started(_)) => true,
            Some(Route::Linked(_)) => launched.result.is_ok(),
            Some(Route::Opened(_) | Route::AlreadyReviewed) | None => false,
        };
        self.log_saved(self.update(key, |request| {
            request.head_sha.clone_from(&head);
            request.worktree.clone_from(&worktree);
            request.review_started = Some(review_started);
            match &launched.result {
                // Handled rather than left queued, so the next batch does not find it again.
                Ok(()) if already_reviewed => request.status = Status::Reported,
                Ok(()) => request.status = Status::Launched,
                Err(error) => {
                    request.status = Status::Failed;
                    request.error = Some(error.clone());
                }
            }
        }));
    }

    // A strictly newer request of the PR whose review began at this head already covers what the request asks for.
    // Newer means a larger GitHub event id, as an older event can be first seen late; an unknown event has no order and is never suppressed.
    fn reviewed_at(&self, request: &Request, head: &str) -> bool {
        let Some(event) = event_id(&request.found.key) else {
            return false;
        };
        let found = &request.found;
        let pr = pr_key(&found.repo, found.number);
        let newer = self
            .lock_state()
            .requests
            .values()
            .filter(|other| {
                other.head_sha.as_deref() == Some(head)
                    && pr_key(&other.found.repo, other.found.number) == pr
                    && event_id(&other.found.key).is_some_and(|other| other > event)
            })
            .cloned()
            .collect::<Vec<_>>();
        newer.iter().any(|other| {
            other.review_started.unwrap_or_else(|| {
                // The version before the proof saved a started new chat as launched with no worktree of its own and the PR linked to that chat.
                // A draft saved its worktree, as does any review since chats start in review-home, so such a record proves nothing.
                other.status == Status::Launched
                    && other.worktree.is_none()
                    && review_record::load_chat(self.deps.paths, &found.repo, found.number)
                        .is_ok_and(|chat| chat.is_some())
            })
        })
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

// The review_requested event a key names, which an unknown request lacks.
fn event_id(key: &str) -> Option<u64> {
    key.rsplit_once('@')?.1.parse().ok()
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
            config::{Batches, ConfigFile, ReviewConfig},
            selection::{queue, report},
            state::Found,
            with_request, with_worktree,
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
        // Run as a chat is created, after the selection recorded the request in flight.
        on_create: Mutex<Option<Box<dyn Fn() + Send>>>,
        // Answers the report chat in place of `creation`.
        report_creation: Mutex<Option<Created>>,
        // Each prompt a new chat was started with, after its title.
        started: Mutex<Vec<(String, String)>>,
        inbox: Mutex<String>,
        // Whether every PR is open and whom it asks to review, as GitHub answers now.
        live: Mutex<serde_json::Value>,
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
            [fixture.review_chat_at(HEAD, "acme/app#12@30")],
            "a chat is tried first"
        );
        assert_eq!(
            fixture.opened(),
            [fixture.new_chat_at(HEAD, "acme/app#12@30")]
        );
        assert_eq!(fixture.sent(), []);
        let launched = fixture.state().requests["acme/app#12@30"].clone();
        assert_eq!(launched.choice.as_deref(), Some("code"));
        assert_eq!(launched.head_sha.as_deref(), Some(HEAD));
        assert_eq!(launched.worktree, Some(worktree.display().to_string()));
        assert_eq!(
            launched.review_started,
            Some(false),
            "a draft in a composer has not started"
        );
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

        assert_eq!(
            fixture.started(),
            [fixture.review_chat_at(HEAD, "acme/app#12@30")]
        );
        assert_eq!(fixture.opened(), Vec::<String>::new());
        assert_eq!(
            fixture.state().requests["acme/app#12@30"].worktree,
            Some(fixture.worktree_at(HEAD).display().to_string())
        );
        assert_eq!(
            fixture.state().requests["acme/app#12@30"].review_started,
            Some(true)
        );
        assert_eq!(
            fixture.sent(),
            [(
                "new-chat".to_owned(),
                fixture.worktree_prompt_at(HEAD, "acme/app#12@31")
            )]
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

        assert_eq!(
            fixture.opened(),
            [fixture.new_chat_at(HEAD, "acme/app#12@30")]
        );
        let to_chat = |key| {
            (
                "review-chat".to_owned(),
                fixture.worktree_prompt_at(LATER, key),
            )
        };
        assert_eq!(
            fixture.sent(),
            [to_chat("acme/app#12@31"), to_chat("acme/app#12@32")],
            "each re-request of one head is told apart by its key"
        );
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
            [(
                "review-chat".to_owned(),
                fixture.worktree_prompt_at(HEAD, "acme/app#12@30")
            )]
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
        assert_eq!(
            fixture.opened(),
            [fixture.new_chat_at(HEAD, "acme/app#12@30")]
        );
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
                fixture.new_chat_at(HEAD, "acme/app#12@30"),
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
                run: "run-1",
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
                "送信待ち: #1 PR 1\n送信待ち: #4 PR 4\n報告のみ: #2 PR 2（他の人が承認済み）\n報告のみ: #3 PR 3（3人以上を指名） pr-wake レビュー 4件"
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
            report.contains("- [#1 PR 1](https://github.com/acme/app/pull/1) スレッドID review-chat、依頼ID acme/app#1@10"),
            "{report}"
        );
        assert_eq!(fixture.count_calls("osascript "), 0);
        assert_eq!(fixture.state().pending_digests, []);
    }

    #[test]
    fn batch_report_tells_a_review_opened_for_the_owner_to_send_from_a_started_one() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(1, 10)]);
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
        *fixture
            .report_creation
            .lock()
            .expect("set the report creation") = Some(Created::Started("report-chat".to_owned()));
        let now = || tokyo("2026-11-18T11:30:00");

        fixture.with_daemon(
            &Setup {
                now: Some(&now),
                ..Setup::default()
            },
            |daemon, _| daemon.batch_once(),
        );

        let started = fixture.started();
        let (_, report) = started.last().expect("a report chat");
        assert!(
            report.contains(
                "Enterで送るまでレビューは始まらないと書く）:\n- [#1 PR 1](https://github.com/acme/app/pull/1)"
            ),
            "{report}"
        );
        assert!(!report.contains("レビューしたPR:"), "{report}");
        assert_eq!(
            fixture.opened().len(),
            1,
            "the review was opened for the owner"
        );
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
    fn selection_starts_only_its_queued_requests_by_the_batch_rules_and_records_each_outcome() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(1, 10), (2, 20), (3, 30), (3, 31), (4, 40), (5, 50)]);
        let mut state = fixture.state();
        state
            .requests
            .get_mut("acme/app#2@20")
            .expect("request")
            .status = Status::Launched;
        review_record::save_state(&fixture.paths, &state).expect("save the state");
        fixture.answer_with("gh api repos/acme/app/pulls/4/requested_reviewers", |_| {
            Ok(br#"{"users":[{"login":"sato"},{"login":"suzuki"}]}"#.to_vec())
        });
        fixture.set_creation(Created::Started("chat-1".to_owned()));
        fixture.request("acme/app", 1, &[requested(10, "tanaka")]);
        fixture.request("acme/app", 4, &[requested(40, "tanaka")]);
        let keys = [
            "acme/app#1@10",
            "acme/app#1@10",
            "acme/app#2@20",
            "acme/app#3@30",
            "acme/app#4@40",
            "acme/app#9@90",
        ];
        // A Saturday, so no scheduled batch runs beside the selection.
        let now = || tokyo("2026-11-21T12:00:00");
        let setup = Setup {
            now: Some(&now),
            ..Setup::default()
        };

        fixture.select("first", &keys);
        fixture.with_daemon(&setup, |daemon, _| daemon.batch_once());
        fixture.select("again", &keys[..1]);
        fixture.with_daemon(&setup, |daemon, _| daemon.batch_once());

        let first = review_record::load_selection(&fixture.paths, "first")
            .expect("read the selection")
            .expect("a recorded selection");
        assert!(first.complete);
        assert_eq!(first.keys.len(), 5, "a key chosen twice is handled once");
        assert_eq!(
            first.outcomes,
            BTreeMap::from([
                (
                    "acme/app#1@10".to_owned(),
                    SelectedOutcome::Started {
                        thread: "chat-1".to_owned()
                    }
                ),
                (
                    "acme/app#2@20".to_owned(),
                    SelectedOutcome::NotQueued {
                        status: Some(Status::Launched),
                        newer: None
                    }
                ),
                (
                    "acme/app#3@30".to_owned(),
                    SelectedOutcome::NotQueued {
                        status: Some(Status::Queued),
                        newer: Some("acme/app#3@31".to_owned())
                    }
                ),
                (
                    "acme/app#4@40".to_owned(),
                    SelectedOutcome::Reported {
                        reason: ReportReason::ManyReviewers
                    }
                ),
                (
                    "acme/app#9@90".to_owned(),
                    SelectedOutcome::NotQueued {
                        status: None,
                        newer: None
                    }
                ),
            ])
        );
        let again = review_record::load_selection(&fixture.paths, "again")
            .expect("read the selection")
            .expect("a recorded selection");
        assert_eq!(
            again.outcomes.get("acme/app#1@10"),
            Some(&SelectedOutcome::NotQueued {
                status: Some(Status::Launched),
                newer: None
            })
        );
        assert_eq!(fixture.started().len(), 1, "one review of the chosen PR");
        assert_eq!(
            ["acme/app#3@31", "acme/app#5@50"].map(|key| fixture.status(key)),
            [Status::Queued, Status::Queued],
            "requests not chosen stay for the scheduled batch"
        );
        assert_eq!(
            fixture.count_calls("osascript "),
            0,
            "no batch notification"
        );
        assert_eq!(fixture.state().pending_digests, [], "no batch report");
    }

    #[test]
    fn selection_records_the_chat_a_review_went_to_and_none_for_a_draft() {
        for (name, linked, creation, expected) in [
            (
                "linked_re_request",
                true,
                Created::NotSent("unused".to_owned()),
                SelectedOutcome::SentToLinked {
                    thread: "review-chat".to_owned(),
                },
            ),
            (
                "draft_only",
                false,
                Created::NotSent("no harnexus".to_owned()),
                SelectedOutcome::Opened {
                    reason: Some("no harnexus".to_owned()),
                },
            ),
            (
                "unconfirmed_chat",
                false,
                Created::Unknown("unknown: no answer".to_owned()),
                SelectedOutcome::Failed {
                    error: "the new review chat could not be confirmed; check Codex App before choosing again: unknown: no answer".to_owned(),
                    thread: None,
                },
            ),
        ] {
            let fixture = Fixture::batched();
            fixture.save_queued(&[(12, 30)]);
            fixture.request("acme/app", 12, &[requested(30, "sato")]);
            if linked {
                fixture.link_chat("acme/app", 12, "review-chat");
            }
            fixture.set_creation(creation);
            let now = || tokyo("2026-11-21T12:00:00");

            fixture.select("one", &["acme/app#12@30"]);
            fixture.with_daemon(
                &Setup {
                    now: Some(&now),
                    ..Setup::default()
                },
                |daemon, _| daemon.batch_once(),
            );

            let record = review_record::load_selection(&fixture.paths, "one")
                .expect("read the selection")
                .expect("a recorded selection");
            assert_eq!(
                record.outcomes.get("acme/app#12@30"),
                Some(&expected),
                "case: {name}"
            );
        }
    }

    #[test]
    fn selection_checks_github_again_and_starts_only_a_request_still_asked_of_me() {
        let mine = live("open", &["me"], &[]);
        let nobody = live("open", &[], &[]);
        let team = live("open", &[], &["backend"]);
        let closed = live("closed", &["me"], &[]);
        let team_event =
            json!({"id": 30, "event": "review_requested", "requested_team": {"slug": "backend"}})
                .to_string();
        let withdrawn = Some(SelectedOutcome::NotQueued {
            status: Some(Status::Withdrawn),
            newer: None,
        });
        let opened = Some(SelectedOutcome::Opened {
            reason: Some("no harnexus in the test".to_owned()),
        });
        let replaced_by = |newer: &str| {
            Some(SelectedOutcome::NotQueued {
                status: None,
                newer: Some(newer.to_owned()),
            })
        };
        let first = requested(30, "sato");
        let second = requested(31, "sato");
        for (name, key, live, events, expected) in [
            (
                "withdrawn",
                "acme/app#12@30",
                &nobody,
                Some(vec![first.clone()]),
                withdrawn.clone(),
            ),
            (
                "closed",
                "acme/app#12@30",
                &closed,
                Some(vec![first.clone()]),
                withdrawn,
            ),
            (
                "re_requested",
                "acme/app#12@30",
                &mine,
                Some(vec![first.clone(), second]),
                replaced_by("acme/app#12@31"),
            ),
            // `None` expects a failure to check, which leaves the request queued.
            ("events_unreadable", "acme/app#12@30", &mine, None, None),
            (
                "team_request",
                "acme/app#12@30",
                &team,
                Some(vec![team_event]),
                opened.clone(),
            ),
            (
                "unknown_still_asked",
                "acme/app#12@unknown",
                &mine,
                Some(Vec::new()),
                opened,
            ),
            (
                "unknown_replaced_by_an_event",
                "acme/app#12@unknown",
                &mine,
                Some(vec![first]),
                replaced_by("acme/app#12@30"),
            ),
        ] {
            let mut fixture = Fixture::batched();
            fixture.config.teams = vec!["acme/backend".to_owned()];
            fixture.save_queued(&[(12, 30)]);
            fixture.rekey("acme/app#12@30", key);
            fixture.set_live(live.clone());
            if let Some(events) = &events {
                fixture.request("acme/app", 12, events);
            }

            fixture.select("one", &[key]);
            fixture.take_selections();

            let outcome = fixture.selection("one").outcomes.get(key).cloned();
            match &expected {
                Some(expected) => assert_eq!(outcome.as_ref(), Some(expected), "case: {name}"),
                None => assert!(
                    matches!(&outcome, Some(SelectedOutcome::Failed { error, thread: None })
                        if error.starts_with("could not check the request on GitHub, so it stays queued")),
                    "case: {name}: {outcome:?}"
                ),
            }
            let launched = matches!(expected, Some(SelectedOutcome::Opened { .. }));
            let status = [Status::Queued, Status::Launched][usize::from(launched)];
            assert_eq!(fixture.status(key), status, "case: {name}");
            assert_eq!(
                fixture.opened().len(),
                usize::from(launched),
                "case: {name}"
            );
        }
    }

    #[test]
    fn restart_closes_a_selection_its_run_left_open_without_starting_it_again() {
        for (name, in_flight_status, accepted, expected_in_flight) in [
            (
                "launch_may_have_happened",
                Status::Launching,
                true,
                SelectedOutcome::Unconfirmed,
            ),
            (
                "claimed_before_the_launch",
                Status::Notifying,
                true,
                SelectedOutcome::Stopped,
            ),
            (
                "taken_but_not_accepted",
                Status::Queued,
                false,
                SelectedOutcome::Stopped,
            ),
        ] {
            let fixture = Fixture::batched();
            fixture.save_queued(&[(1, 10), (2, 20), (3, 30)]);
            let mut state = fixture.state();
            state
                .requests
                .get_mut("acme/app#2@20")
                .expect("request")
                .status = in_flight_status;
            review_record::save_state(&fixture.paths, &state).expect("save the state");
            let keys = ["acme/app#1@10", "acme/app#2@20", "acme/app#3@30"].map(str::to_owned);
            let mut record = SelectionRecord {
                run: "run-0".to_owned(),
                accepted,
                ..SelectionRecord::of(&keys)
            };
            if accepted {
                record.outcomes.insert(
                    keys[0].clone(),
                    SelectedOutcome::Started {
                        thread: "chat-1".to_owned(),
                    },
                );
                record.in_flight = Some(keys[1].clone());
            }
            review_record::save_selection(&fixture.paths, "old", &record).expect("save it");

            fixture.with_daemon(&Setup::default(), |daemon, _| {
                daemon.recover().expect("recover");
                daemon.recover().expect("recover again");
            });

            let closed = fixture.selection("old");
            assert!(
                closed.complete && closed.in_flight.is_none(),
                "case: {name}"
            );
            let started = SelectedOutcome::Started {
                thread: "chat-1".to_owned(),
            };
            let first = if accepted {
                &started
            } else {
                &SelectedOutcome::Stopped
            };
            assert_eq!(closed.outcomes.get(&keys[0]), Some(first), "case: {name}");
            assert_eq!(
                closed.outcomes.get(&keys[1]),
                Some(&expected_in_flight),
                "case: {name}"
            );
            assert_eq!(
                closed.outcomes.get(&keys[2]),
                Some(&SelectedOutcome::Stopped),
                "case: {name}"
            );
            assert_eq!(fixture.status(&keys[2]), Status::Queued, "case: {name}");
            assert!(
                fixture.opened().is_empty() && fixture.started().is_empty(),
                "case: {name}"
            );
        }
    }

    #[test]
    fn poll_during_the_reviewer_lookup_still_withdraws_or_replaces_a_selected_request() {
        for (name, events_after, newer) in [
            ("withdrawn", None, None),
            (
                "replaced",
                Some(vec![requested(30, "sato"), requested(31, "sato")]),
                Some("acme/app#12@31"),
            ),
        ] {
            let fixture = Fixture::batched();
            fixture.save_queued(&[(12, 30)]);
            // Seen before the test's clock, so a request the poll adds is the later one.
            let mut state = fixture.state();
            state
                .requests
                .get_mut("acme/app#12@30")
                .expect("request")
                .first_seen = tokyo("2026-11-20T12:00:00");
            review_record::save_state(&fixture.paths, &state).expect("save the state");
            fixture.request("acme/app", 12, &[requested(30, "sato")]);
            let looking = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let (looked, release) = (Arc::clone(&looking), Arc::clone(&released));
            fixture.answer_with(
                "gh api repos/acme/app/pulls/12/requested_reviewers",
                move |_| {
                    looked.store(true, Ordering::SeqCst);
                    wait_for(|| release.load(Ordering::SeqCst));
                    Ok(br#"{"users":[]}"#.to_vec())
                },
            );
            fixture.select("one", &["acme/app#12@30"]);
            let now = || tokyo("2026-11-21T12:00:00");

            fixture.with_daemon(
                &Setup {
                    now: Some(&now),
                    ..Setup::default()
                },
                |daemon, _| {
                    thread::scope(|scope| {
                        let batch = scope.spawn(|| daemon.batch_once());
                        wait_for(|| looking.load(Ordering::SeqCst));
                        match &events_after {
                            Some(events) => fixture.request("acme/app", 12, events),
                            None => {
                                fixture.events.lock().expect("drop the PR").clear();
                            }
                        }
                        daemon.poll_once().expect("poll during the lookup");
                        released.store(true, Ordering::SeqCst);
                        batch.join().expect("join the batch");
                    });
                },
            );

            assert_eq!(
                fixture.selection("one").outcomes.get("acme/app#12@30"),
                Some(&SelectedOutcome::NotQueued {
                    status: Some(Status::Withdrawn),
                    newer: newer.map(str::to_owned),
                }),
                "case: {name}"
            );
            assert!(
                fixture.opened().is_empty() && fixture.started().is_empty(),
                "case: {name}"
            );
        }
    }

    #[test]
    fn removal_of_one_of_simultaneous_direct_and_team_requests_lists_and_starts_the_one_left() {
        let mut fixture = Fixture::batched();
        fixture.config.teams = vec!["acme/backend".to_owned()];
        let direct = requested(10, "tanaka");
        let team =
            json!({"id": 20, "event": "review_requested", "requested_team": {"slug": "backend"}})
                .to_string();
        let removed =
            json!({"id": 21, "event": "review_request_removed", "requested_team": {"slug": "backend"}})
                .to_string();
        let now = || tokyo("2026-11-21T12:00:00");
        let setup = Setup {
            now: Some(&now),
            ..Setup::default()
        };
        let queued = |fixture: &Fixture| {
            queue(&fixture.state())
                .into_iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>()
        };

        let (both, one_left) = fixture.with_daemon(&setup, |daemon, _| {
            daemon.poll_once().expect("baseline poll");
            fixture.set_live(live("open", &["me"], &["backend"]));
            fixture.request("acme/app", 12, &[direct.clone(), team.clone()]);
            daemon.poll_once().expect("poll both requests");
            let both = queued(&fixture);
            fixture.set_live(live("open", &["me"], &[]));
            fixture.request("acme/app", 12, &[direct, team, removed]);
            daemon.poll_once().expect("poll after the team's removal");
            (both, queued(&fixture))
        });
        fixture.select("chosen", &["acme/app#12@20", "acme/app#12@10"]);
        fixture.take_selections();

        assert_eq!(both, ["acme/app#12@20"]);
        assert_eq!(one_left, ["acme/app#12@10"]);
        let record = fixture.selection("chosen");
        assert_eq!(
            record.outcomes.get("acme/app#12@20"),
            Some(&SelectedOutcome::NotQueued {
                status: Some(Status::Withdrawn),
                newer: None
            }),
            "the key shown for the team does not start the direct request"
        );
        assert!(matches!(
            record.outcomes.get("acme/app#12@10"),
            Some(SelectedOutcome::Opened { .. })
        ));
        assert_eq!(fixture.opened().len(), 1);
    }

    #[test]
    fn older_request_seen_again_is_only_reported_when_a_newer_one_was_reviewed_at_its_head() {
        const LATER: &str = "fedcba9876543210fedcba9876543210fedcba98";
        for (name, selected, newer_status, started, newer_head, starts) in [
            (
                "selected_same_head",
                true,
                Status::Launched,
                true,
                HEAD,
                false,
            ),
            (
                "scheduled_same_head",
                false,
                Status::Launched,
                true,
                HEAD,
                false,
            ),
            (
                "selected_other_head",
                true,
                Status::Launched,
                true,
                LATER,
                true,
            ),
            (
                "scheduled_newer_draft_only",
                false,
                Status::Launched,
                false,
                HEAD,
                true,
            ),
            (
                "scheduled_newer_withdrawn",
                false,
                Status::Withdrawn,
                false,
                HEAD,
                true,
            ),
        ] {
            let fixture = Fixture::batched();
            fixture.save_queued(&[(12, 10), (12, 20)]);
            let mut state = fixture.state();
            let newer = state.requests.get_mut("acme/app#12@20").expect("request");
            newer.status = newer_status;
            newer.review_started = Some(started);
            newer.head_sha = Some(newer_head.to_owned());
            // Event 10 is first seen only after event 20's team was removed, so its order comes from the event ids.
            let older = state.requests.get_mut("acme/app#12@10").expect("request");
            older.first_seen = older
                .first_seen
                .checked_add(SignedDuration::from_hours(1))
                .expect("a later time");
            review_record::save_state(&fixture.paths, &state).expect("save the state");
            fixture.request("acme/app", 12, &[requested(10, "tanaka")]);
            if selected {
                fixture.select("one", &["acme/app#12@10"]);
            } else {
                review_record::request_now(
                    &fixture.paths,
                    &NowRequest { pr: None },
                    Timestamp::now(),
                )
                .expect("ask for the batch");
            }

            fixture.take_selections();

            assert_eq!(!fixture.opened().is_empty(), starts, "case: {name}");
            let status = [Status::Reported, Status::Launched][usize::from(starts)];
            assert_eq!(fixture.status("acme/app#12@10"), status, "case: {name}");
            if selected && !starts {
                assert_eq!(
                    fixture.selection("one").outcomes.get("acme/app#12@10"),
                    Some(&SelectedOutcome::Reported {
                        reason: ReportReason::AlreadyReviewed
                    }),
                    "case: {name}"
                );
            }
        }
    }

    #[test]
    fn record_of_the_version_before_the_proof_suppresses_only_a_confirmed_new_chat() {
        for (name, worktree, sends) in [
            ("new_chat", None, false),
            ("draft_with_its_worktree", Some("/w/acme/app/pr-12"), true),
        ] {
            let fixture = Fixture::batched();
            fixture.save_queued(&[(12, 10), (12, 20)]);
            let mut state = fixture.state();
            let newer = state.requests.get_mut("acme/app#12@20").expect("request");
            newer.status = Status::Launched;
            newer.head_sha = Some(HEAD.to_owned());
            newer.worktree = worktree.map(str::to_owned);
            review_record::save_state(&fixture.paths, &state).expect("save the state");
            let saved = std::fs::read_to_string(fixture.paths.state_file()).expect("read it");
            fixture.link_chat("acme/app", 12, "review-chat");
            review_record::request_now(&fixture.paths, &NowRequest { pr: None }, Timestamp::now())
                .expect("ask for the batch");

            fixture.take_selections();

            assert!(
                !saved.contains("reviewStarted"),
                "case: {name}: as the old version wrote it"
            );
            assert_eq!(fixture.sent().len(), usize::from(sends), "case: {name}");
            let status = [Status::Reported, Status::Launched][usize::from(sends)];
            assert_eq!(fixture.status("acme/app#12@10"), status, "case: {name}");
        }
    }

    #[test]
    fn confirmed_new_chat_whose_link_failed_still_keeps_the_older_request_from_a_second_chat() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::batched();
        fixture.save_queued(&[(12, 10), (12, 20)]);
        fixture.set_creation(Created::Started("chat-1".to_owned()));
        let mut state = fixture.state();
        state
            .requests
            .get_mut("acme/app#12@10")
            .expect("request")
            .status = Status::Withdrawn;
        review_record::save_state(&fixture.paths, &state).expect("save the state");
        let chats = fixture.paths.state.join("chats");
        std::fs::create_dir_all(&chats).expect("create the links");
        // The link of the started chat to the PR cannot be written.
        std::fs::set_permissions(&chats, std::fs::Permissions::from_mode(0o500)).expect("block it");
        let batch = || {
            review_record::request_now(&fixture.paths, &NowRequest { pr: None }, Timestamp::now())
                .expect("ask for the batch");
            fixture.take_selections();
        };

        batch();
        let mut state = fixture.state();
        state
            .requests
            .get_mut("acme/app#12@10")
            .expect("request")
            .status = Status::Queued;
        review_record::save_state(&fixture.paths, &state).expect("save the state");
        batch();

        std::fs::set_permissions(&chats, std::fs::Permissions::from_mode(0o700)).expect("restore");
        let failed = fixture.state().requests["acme/app#12@20"].clone();
        assert_eq!(failed.status, Status::Failed);
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("started in chat chat-1")),
            "{failed:?}"
        );
        assert_eq!(failed.review_started, Some(true));
        assert_eq!(fixture.status("acme/app#12@10"), Status::Reported);
        let reviews = fixture
            .started()
            .into_iter()
            .filter(|(title, _)| title.starts_with("Review｜"))
            .count();
        assert_eq!(reviews, 1, "no second chat");
    }

    #[test]
    fn poll_without_batches_keeps_naming_the_latest_event_without_reading_the_reviewers() {
        let mut fixture = Fixture::new();
        fixture.config.teams = vec!["acme/backend".to_owned()];
        fixture.set_live(live("open", &["me"], &[]));
        let direct = requested(10, "tanaka");
        let team =
            json!({"id": 20, "event": "review_requested", "requested_team": {"slug": "backend"}})
                .to_string();
        let removed =
            json!({"id": 21, "event": "review_request_removed", "requested_team": {"slug": "backend"}})
                .to_string();

        fixture.with_daemon(&Setup::default(), |daemon, _| {
            daemon.poll_once().expect("baseline poll");
            fixture.request("acme/app", 12, &[direct.clone(), team.clone()]);
            daemon.poll_once().expect("poll both requests");
            fixture.request("acme/app", 12, &[direct, team, removed]);
            daemon.poll_once().expect("poll after the team's removal");
        });

        let keys = fixture.state().requests.into_keys().collect::<Vec<_>>();
        assert_eq!(keys, ["acme/app#12@20"]);
        assert_eq!(
            fixture.calls_starting("gh api repos/acme/app/pulls/12"),
            Vec::<String>::new()
        );

        // Restarted with batches while the PR is unchanged, so only the restart makes the cached event 20 be read again.
        fixture.config.batches = Fixture::batched().config.batches;
        fixture.with_daemon(&Setup::default(), |daemon, _| {
            daemon.recover().expect("recover on the restart");
            daemon.poll_once().expect("poll after the restart");
        });
        let queued = queue(&fixture.state())
            .into_iter()
            .map(|entry| entry.key)
            .collect::<Vec<_>>();
        fixture.select("chosen", &["acme/app#12@10"]);
        fixture.take_selections();

        assert_eq!(queued, ["acme/app#12@10"]);
        assert_eq!(fixture.status("acme/app#12@20"), Status::Withdrawn);
        assert!(matches!(
            fixture.selection("chosen").outcomes.get("acme/app#12@10"),
            Some(SelectedOutcome::Opened { .. })
        ));
    }

    #[test]
    fn selection_result_lost_to_failed_writes_is_saved_later_without_a_second_launch() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::batched();
        fixture.save_queued(&[(12, 30)]);
        fixture.request("acme/app", 12, &[requested(30, "sato")]);
        fixture.set_creation(Created::Started("chat-1".to_owned()));
        let selections = fixture.paths.state.join("selections");
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("set the mode");
        };
        let blocked = selections.clone();
        // Every write of the selection after the chat starts fails until the directory is writable again.
        *fixture.on_create.lock().expect("set the hook") = Some(Box::new(move || {
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500))
                .expect("block the selection");
        }));
        fixture.select("one", &["acme/app#12@30"]);
        let now = || tokyo("2026-11-21T12:00:00");

        let (lost, saved) = fixture.with_daemon(
            &Setup {
                now: Some(&now),
                ..Setup::default()
            },
            |daemon, _| {
                daemon.batch_once();
                let lost = fixture.selection("one");
                mode(&selections, 0o700);
                daemon.batch_once();
                (lost, fixture.selection("one"))
            },
        );

        assert!(lost.accepted && !lost.complete, "{lost:?}");
        assert_eq!(lost.in_flight.as_deref(), Some("acme/app#12@30"));
        assert!(saved.complete && saved.in_flight.is_none(), "{saved:?}");
        let report = report("one", &saved, &fixture.state());
        assert_eq!(
            report.requests[0].link.as_deref(),
            Some("codex://threads/chat-1")
        );
        assert_eq!(fixture.started().len(), 1, "launched once");
    }

    #[test]
    fn selection_made_for_an_earlier_run_is_closed_unstarted() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(12, 30)]);
        fixture.request("acme/app", 12, &[requested(30, "sato")]);
        fixture.select_for("run-0", "yesterday", &["acme/app#12@30"]);

        fixture.with_daemon(&Setup::default(), |daemon, _| {
            daemon.recover().expect("recover");
        });
        fixture.take_selections();

        let closed = fixture.selection("yesterday");
        assert!(closed.complete && !closed.accepted);
        assert_eq!(
            closed.outcomes.get("acme/app#12@30"),
            Some(&SelectedOutcome::Stopped)
        );
        assert_eq!(fixture.status("acme/app#12@30"), Status::Queued);
        assert_eq!(fixture.opened(), Vec::<String>::new());
    }

    #[test]
    fn selection_of_a_caller_that_died_after_submitting_starts_once_whatever_repeats_its_id() {
        let fixture = Fixture::batched();
        fixture.save_queued(&[(12, 30)]);
        fixture.request("acme/app", 12, &[requested(30, "sato")]);

        let first = fixture.select("one", &["acme/app#12@30"]);
        fixture.take_selections();
        let repeated = fixture.select("one", &["acme/app#12@30"]);
        fixture.take_selections();

        assert!(
            first && !repeated,
            "only the first write of an id is the handoff"
        );
        let record = fixture.selection("one");
        assert!(record.complete && record.accepted);
        assert_eq!(
            record.outcomes.get("acme/app#12@30"),
            Some(&SelectedOutcome::Opened {
                reason: Some("no harnexus in the test".to_owned())
            })
        );
        assert_eq!(fixture.opened().len(), 1, "started once");
    }

    #[test]
    fn selection_whose_acceptance_cannot_be_written_starts_nothing_and_stays_waiting() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::batched();
        fixture.save_queued(&[(12, 30)]);
        fixture.request("acme/app", 12, &[requested(30, "sato")]);
        fixture.select("one", &["acme/app#12@30"]);
        let selections = fixture.paths.state.join("selections");
        let permissions = |mode| {
            std::fs::set_permissions(&selections, std::fs::Permissions::from_mode(mode))
                .expect("set the mode");
        };
        // A directory that cannot be written to makes the acceptance's atomic replacement fail.
        permissions(0o500);

        fixture.take_selections();

        permissions(0o700);
        let waiting = fixture.selection("one");
        assert!(!waiting.accepted && !waiting.complete);
        assert_eq!(fixture.status("acme/app#12@30"), Status::Queued);
        assert!(fixture.opened().is_empty() && fixture.started().is_empty());
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
                on_create: Mutex::new(None),
                report_creation: Mutex::new(None),
                started: Mutex::new(Vec::new()),
                inbox: Mutex::new("[]".to_owned()),
                live: Mutex::new(json!({
                    "state": "open",
                    "requested_reviewers": [{"login": "me"}],
                    "requested_teams": [],
                })),
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
                    run: "run-1",
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
                let mut pull = serde_json::from_str::<serde_json::Value>(&pull_json(
                    &format!("{}/{}", parts[0], parts[1]),
                    parts[3],
                    &head,
                ))
                .expect("a pull request");
                let requests_now = self.live.lock().expect("read the live requests").clone();
                for (field, value) in requests_now.as_object().expect("an object") {
                    pull[field] = value.clone();
                }
                return Ok(pull.to_string().into_bytes());
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

        fn select(&self, id: &str, keys: &[&str]) -> bool {
            self.select_for("run-1", id, keys)
        }

        // As the caller hands a selection over, with its one write; `false` when the id is taken.
        fn select_for(&self, run: &str, id: &str, keys: &[&str]) -> bool {
            let keys = keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
            let record = SelectionRecord {
                run: run.to_owned(),
                ..SelectionRecord::of(&keys)
            };
            review_record::create_selection(&self.paths, id, &record).expect("write the selection")
        }

        fn selection(&self, id: &str) -> SelectionRecord {
            review_record::load_selection(&self.paths, id)
                .expect("read the selection")
                .expect("a recorded selection")
        }

        fn set_live(&self, live: serde_json::Value) {
            *self.live.lock().expect("set the live requests") = live;
        }

        fn rekey(&self, from: &str, to: &str) {
            let mut state = self.state();
            let mut request = state.requests.remove(from).expect("request");
            to.clone_into(&mut request.found.key);
            state.requests.insert(to.to_owned(), request);
            review_record::save_state(&self.paths, &state).expect("save the state");
        }

        // One pass of the batch thread on a Saturday, so no scheduled batch runs beside what was handed over.
        fn take_selections(&self) {
            let now = || tokyo("2026-11-21T12:00:00");
            self.with_daemon(
                &Setup {
                    now: Some(&now),
                    ..Setup::default()
                },
                |daemon, _| daemon.batch_once(),
            );
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
            if let Some(hook) = &*self.on_create.lock().expect("read the hook") {
                hook();
            }
            let report = self
                .report_creation
                .lock()
                .expect("read the report creation")
                .clone()
                .filter(|_| chat.title.starts_with("レビュー報告"));
            report.unwrap_or_else(|| self.creation.lock().expect("read the creation").clone())
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

        fn worktree_prompt_at(&self, head: &str, key: &str) -> String {
            let prompt = with_worktree(
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
            );
            with_request(&prompt, key)
        }

        // The title and the prompt of a new review chat for PR 12 at `head`.
        fn review_chat_at(&self, head: &str, key: &str) -> (String, String) {
            (
                "Review｜T PR#12".to_owned(),
                self.worktree_prompt_at(head, key),
            )
        }

        fn new_chat_at(&self, head: &str, key: &str) -> String {
            new_chat_link(
                &self.worktree_prompt_at(head, key),
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

    fn live(state: &str, users: &[&str], teams: &[&str]) -> serde_json::Value {
        json!({
            "state": state,
            "requested_reviewers": users.iter().map(|login| json!({"login": login})).collect::<Vec<_>>(),
            "requested_teams": teams.iter().map(|slug| json!({"slug": slug})).collect::<Vec<_>>(),
        })
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
