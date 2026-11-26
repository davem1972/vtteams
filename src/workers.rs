//! Two small fixed pools of worker threads for blocking backend calls, each with a queue of its own.
//!
//! The **background** lane (8 threads) runs what nobody is waiting for: polling, name lookups, picture downloads,
//! presence, and any call that can legitimately run for minutes (an interactive sign-in). Each call there can sit on
//! the network for as long as its timeout, so all eight can be busy at once. The **interactive** lane (2 threads) runs
//! what a person has just asked for and is watching (a send, an edit, a reaction, a search, opening a chat), so that
//! such a call never waits in a queue behind the slow ones. It must only be given calls that finish in about one
//! request: a call that can run for minutes would occupy a thread for good.
//!
//! Jobs are queued on a channel and run by whichever worker of the lane is free. Dropping the pool closes both
//! channels: a worker idle in `recv()` is woken by that at once (the channel's own close signal, not a second one it
//! has to select on) and exits its loop; one in the middle of a backend call notices only once that call returns.
//! `drop` then joins every worker of both lanes, but only up to [`SHUTDOWN_BUDGET`] in all, since a call can
//! legitimately run for minutes; past that it gives up and returns rather than hang a quit on it, leaving `main`'s
//! `std::process::exit` as the real backstop. The pool itself does not catch panics; each job is wrapped by the caller
//! (see `App::spawn_call`) so that a panic still produces a result the UI can act on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use teams_client::Backend;

/// Background worker threads are named `vtteams-worker-N`.
pub const WORKER_PREFIX: &str = "vtteams-worker";

/// Interactive worker threads are named `vtteams-action-N`.
pub const ACTION_PREFIX: &str = "vtteams-action";

/// Enough that slow calls (a silent sign-in renewal can take half a minute) do not starve chat polling, with image
/// downloads separately capped below this.
const BACKGROUND_THREADS: usize = 8;

/// What a person is waiting on is one call at a time (a send, a search); two threads let a second action run while the
/// first is still on the network, and no more, since this lane is for answering quickly, not for throughput.
const INTERACTIVE_THREADS: usize = 2;

/// How long `Drop` waits for every worker to actually exit once the queues are closed. Covers the common case (idle
/// workers, which wake and exit at once) with time to spare; a worker that was mid-call when its queue closed will
/// not finish in time and is simply left behind, since nothing here can interrupt a call already in flight.
const SHUTDOWN_BUDGET: Duration = Duration::from_millis(500);

/// A unit of work. It receives the backend so the closure does not need its own handle.
pub type Job = Box<dyn FnOnce(&dyn Backend) + Send>;

/// Which queue a job waits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Something a person just asked for and is waiting on; a short call.
    Interactive,
    /// Everything else, including calls that may run for minutes.
    Background,
}

/// Counts a job from the moment it is queued until it has finished (or been dropped unrun).
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn begin(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(count))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One lane: its queue (closed on drop), how many of its threads started and how many of its jobs are unfinished.
struct Queue {
    jobs: Option<Sender<Job>>,
    threads: usize,
    in_flight: Arc<AtomicUsize>,
}

impl Queue {
    #[cfg(test)]
    fn closed() -> Self {
        Self {
            jobs: None,
            threads: 0,
            in_flight: Arc::default(),
        }
    }

    /// Start up to `count` threads named `{prefix}-N` that run this lane's jobs, appending their handles.
    fn start(
        prefix: &str,
        count: usize,
        backend: &Arc<dyn Backend>,
        handles: &mut Vec<JoinHandle<()>>,
    ) -> Self {
        let (jobs, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let mut threads = 0;
        for n in 0..count {
            let (rx, backend) = (Arc::clone(&rx), Arc::clone(backend));
            let spawned = std::thread::Builder::new()
                .name(format!("{prefix}-{n}"))
                .spawn(move || {
                    loop {
                        // The receiver is only a handle, so a poisoned lock cannot hide a half-updated state. The guard is
                        // dropped at the end of this statement, before the job runs.
                        let next = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
                        match next {
                            Ok(job) => job(backend.as_ref()),
                            Err(_) => break, // the pool was dropped
                        }
                    }
                });
            match spawned {
                Ok(handle) => {
                    handles.push(handle);
                    threads += 1;
                }
                Err(e) => log::error!("cannot start worker thread {prefix}-{n}: {e}"),
            }
        }
        Self {
            jobs: Some(jobs),
            threads,
            in_flight: Arc::default(),
        }
    }

    fn has_threads(&self) -> bool {
        self.jobs.is_some() && self.threads > 0
    }
}

pub struct Workers {
    interactive: Queue,
    background: Queue,
    handles: Vec<JoinHandle<()>>,
}

impl Workers {
    pub fn new(backend: &Arc<dyn Backend>) -> Self {
        let mut handles = Vec::with_capacity(BACKGROUND_THREADS + INTERACTIVE_THREADS);
        let background = Queue::start(WORKER_PREFIX, BACKGROUND_THREADS, backend, &mut handles);
        let interactive = Queue::start(ACTION_PREFIX, INTERACTIVE_THREADS, backend, &mut handles);
        Self {
            interactive,
            background,
            handles,
        }
    }

    /// A pool with no threads: every job runs on the thread that submits it, to completion, before `submit` returns.
    /// The app degrades to the same behaviour if no worker thread can be started; tests use it to be deterministic.
    #[cfg(test)]
    pub fn inline() -> Self {
        Self {
            interactive: Queue::closed(),
            background: Queue::closed(),
            handles: Vec::new(),
        }
    }

    /// Whether jobs run on the thread that submits them (no worker threads exist).
    pub fn is_inline(&self) -> bool {
        !self.interactive.has_threads() && !self.background.has_threads()
    }

    /// Queue `job` in `lane`. A lane none of whose threads could be started hands its jobs to the other lane, so a
    /// machine that is short of threads still runs everything; if no worker could be started at all (or all have
    /// gone), the job comes back to the caller.
    pub fn submit(&self, lane: Lane, job: Job) -> Result<(), Job> {
        let (own, other) = match lane {
            Lane::Interactive => (&self.interactive, &self.background),
            Lane::Background => (&self.background, &self.interactive),
        };
        let queue = if own.has_threads() { own } else { other };
        match &queue.jobs {
            Some(tx) if queue.has_threads() => {
                let counted = InFlight::begin(&queue.in_flight);
                let job: Job = Box::new(move |backend| {
                    let _counted = counted;
                    job(backend);
                });
                // A send fails only when every thread of the lane is gone; the caller gets the job back and runs it,
                // and it is counted until it has.
                tx.send(job).map_err(|e| e.0)
            }
            _ => Err(job),
        }
    }

    /// How many jobs, in both lanes, have been queued and not yet finished.
    pub fn in_flight(&self) -> usize {
        self.in_flight_in(Lane::Interactive) + self.in_flight_in(Lane::Background)
    }

    /// How many of `lane`'s own jobs have been queued and not yet finished.
    pub fn in_flight_in(&self, lane: Lane) -> usize {
        match lane {
            Lane::Interactive => &self.interactive,
            Lane::Background => &self.background,
        }
        .in_flight
        .load(Ordering::SeqCst)
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        // Closing the queues is the wake-up: a worker blocked in `recv()` gets `Err` the instant the last `Sender`
        // goes away, with no polling and no separate shutdown channel for it to select on.
        self.interactive.jobs = None;
        self.background.jobs = None;
        let handles = std::mem::take(&mut self.handles);
        if handles.is_empty() {
            return;
        }
        // Joined on another thread so this can still enforce an overall deadline (one for both lanes together):
        // `JoinHandle::join` itself has no timeout, and a worker that was mid-call when its queue closed may not exit
        // for as long as that call's own timeout allows (minutes, for an interactive sign-in).
        let (done, arrived) = channel::<()>();
        let joined = std::thread::Builder::new()
            .name(format!("{WORKER_PREFIX}-join"))
            .spawn(move || {
                for handle in handles {
                    let _ = handle.join();
                }
                let _ = done.send(());
            });
        if joined.is_ok() {
            let _ = arrived.recv_timeout(SHUTDOWN_BUDGET);
        }
    }
}

/// What tests that fill a lane with blocked jobs share.
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::Condvar;

    /// Long enough that a healthy machine never reaches it; only a test that is about to fail waits this long.
    pub const PATIENCE: Duration = Duration::from_secs(20);

    /// A door that jobs wait at until the test opens it.
    #[derive(Default)]
    pub struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        pub fn wait(&self) {
            let open = self.open.lock().unwrap();
            let (_open, _) = self
                .changed
                .wait_timeout_while(open, PATIENCE, |open| !*open)
                .unwrap();
        }

        pub fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    /// Submit `n` jobs to `lane` that report in and then wait at `gate`; returns once every one of them is running, so
    /// the lane's threads are all occupied.
    pub fn occupy(workers: &Workers, lane: Lane, n: usize, gate: &Arc<Gate>) {
        let (started, running) = channel::<()>();
        for _ in 0..n {
            let (started, gate) = (started.clone(), Arc::clone(gate));
            let queued = workers.submit(
                lane,
                Box::new(move |_| {
                    let _ = started.send(());
                    gate.wait();
                }),
            );
            assert!(queued.is_ok());
        }
        for _ in 0..n {
            running
                .recv_timeout(PATIENCE)
                .expect("a blocking job never started");
        }
    }

    /// How many threads the background lane has.
    pub const BACKGROUND_LANE: usize = BACKGROUND_THREADS;
}

#[cfg(test)]
mod tests {
    use super::testing::{Gate, PATIENCE, occupy};
    use super::*;
    use crate::demo::DemoBackend;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Receiver;
    use std::time::Instant;

    #[test]
    fn a_pool_with_no_threads_says_it_runs_jobs_inline() {
        assert!(Workers::inline().is_inline());
        assert!(!Workers::new(&backend()).is_inline());
    }

    fn backend() -> Arc<dyn Backend> {
        Arc::new(DemoBackend::instant())
    }

    #[test]
    fn every_queued_job_of_both_lanes_is_counted_until_it_has_finished() {
        let workers = Workers::new(&backend());
        assert_eq!(workers.in_flight(), 0);
        let gate = Arc::new(Gate::default());
        occupy(&workers, Lane::Interactive, 1, &gate);
        assert_eq!(workers.in_flight(), 1, "one lane alone");
        occupy(&workers, Lane::Background, 2, &gate);
        assert_eq!(workers.in_flight(), 3, "both lanes together");
        gate.open();
        let until = Instant::now() + PATIENCE;
        while workers.in_flight() > 0 {
            assert!(Instant::now() < until, "the jobs never finished");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_job_that_panics_is_no_longer_counted() {
        let workers = Workers::new(&backend());
        let queued = workers.submit(Lane::Interactive, Box::new(|_| panic!("a job that fails")));
        assert!(queued.is_ok());
        let until = Instant::now() + PATIENCE;
        while workers.in_flight() > 0 {
            assert!(Instant::now() < until, "the count never came back down");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Submit a job that reports in on the returned receiver.
    fn probe(workers: &Workers, lane: Lane) -> Receiver<()> {
        let (ran, heard) = channel::<()>();
        let queued = workers.submit(
            lane,
            Box::new(move |_| {
                let _ = ran.send(());
            }),
        );
        assert!(queued.is_ok());
        heard
    }

    #[test]
    fn an_interactive_job_runs_at_once_while_every_background_thread_is_occupied() {
        let workers = Workers::new(&backend());
        let gate = Arc::new(Gate::default());
        occupy(&workers, Lane::Background, BACKGROUND_THREADS, &gate);
        // One more background job is queued behind the eight.
        let behind = probe(&workers, Lane::Background);
        let asked = Instant::now();
        let action = probe(&workers, Lane::Interactive);
        assert!(
            action.recv_timeout(PATIENCE).is_ok(),
            "the interactive job waited behind background work"
        );
        assert!(asked.elapsed() < PATIENCE);
        assert!(
            behind.recv_timeout(Duration::from_millis(100)).is_err(),
            "the queued background job has no thread to run on yet"
        );
        gate.open();
        assert!(
            behind.recv_timeout(PATIENCE).is_ok(),
            "it runs once a thread is free"
        );
    }

    #[test]
    fn a_background_job_runs_while_the_interactive_lane_is_occupied() {
        let workers = Workers::new(&backend());
        let gate = Arc::new(Gate::default());
        occupy(&workers, Lane::Interactive, INTERACTIVE_THREADS, &gate);
        let behind = probe(&workers, Lane::Interactive);
        assert!(
            probe(&workers, Lane::Background)
                .recv_timeout(PATIENCE)
                .is_ok(),
            "the background job waited behind interactive work"
        );
        assert!(behind.recv_timeout(Duration::from_millis(100)).is_err());
        gate.open();
        assert!(behind.recv_timeout(PATIENCE).is_ok());
    }

    #[test]
    fn each_lane_has_its_own_fixed_number_of_named_threads() {
        let workers = Workers::new(&backend());
        let (names, heard) = channel::<String>();
        for lane in [Lane::Interactive, Lane::Background] {
            for _ in 0..40 {
                let names = names.clone();
                let queued = workers.submit(
                    lane,
                    Box::new(move |_| {
                        let name = std::thread::current().name().unwrap_or("").to_string();
                        let _ = names.send(name);
                        std::thread::sleep(Duration::from_millis(1));
                    }),
                );
                assert!(queued.is_ok());
            }
        }
        drop(names);
        let mut seen = std::collections::BTreeSet::new();
        while let Ok(name) = heard.recv_timeout(PATIENCE) {
            seen.insert(name);
        }
        let count = |prefix: &str| seen.iter().filter(|n| n.starts_with(prefix)).count();
        assert!(count(ACTION_PREFIX) >= 1 && count(ACTION_PREFIX) <= INTERACTIVE_THREADS);
        assert!(count(WORKER_PREFIX) >= 1 && count(WORKER_PREFIX) <= BACKGROUND_THREADS);
        assert_eq!(seen.len(), count(ACTION_PREFIX) + count(WORKER_PREFIX));
    }

    #[test]
    fn a_lane_with_no_threads_hands_its_jobs_to_the_other_lane() {
        let mut workers = Workers::new(&backend());
        // As if no interactive thread could be started.
        workers.interactive.threads = 0;
        assert!(
            probe(&workers, Lane::Interactive)
                .recv_timeout(PATIENCE)
                .is_ok()
        );
        let mut workers = Workers::new(&backend());
        workers.background.threads = 0;
        assert!(
            probe(&workers, Lane::Background)
                .recv_timeout(PATIENCE)
                .is_ok()
        );
    }

    #[test]
    fn an_inline_pool_gives_every_job_back_whatever_the_lane() {
        let workers = Workers::inline();
        for lane in [Lane::Interactive, Lane::Background] {
            assert!(workers.submit(lane, Box::new(|_| {})).is_err());
        }
    }

    #[test]
    fn dropping_waits_for_a_short_job_in_either_lane() {
        // One job, in one lane at a time and with the other lane idle, so a drop that joined only the other lane would
        // return at once.
        for lane in [Lane::Interactive, Lane::Background] {
            let workers = Workers::new(&backend());
            let done = Arc::new(AtomicBool::new(false));
            let (flag, (started, running)) = (Arc::clone(&done), channel::<()>());
            let queued = workers.submit(
                lane,
                Box::new(move |_| {
                    let _ = started.send(());
                    std::thread::sleep(SHUTDOWN_BUDGET / 2);
                    flag.store(true, Ordering::SeqCst);
                }),
            );
            assert!(queued.is_ok());
            running
                .recv_timeout(PATIENCE)
                .expect("the job never started");
            drop(workers);
            assert!(
                done.load(Ordering::SeqCst),
                "drop returned before the {lane:?} lane's job finished"
            );
        }
    }

    #[test]
    fn a_slow_job_in_the_interactive_lane_does_not_make_drop_wait_for_it_either() {
        let workers = Workers::new(&backend());
        let gate = Arc::new(Gate::default());
        occupy(&workers, Lane::Interactive, 1, &gate);
        let started = Instant::now();
        drop(workers);
        assert!(
            started.elapsed() < SHUTDOWN_BUDGET * 3,
            "drop gave up rather than wait out the blocked job: {:?}",
            started.elapsed()
        );
        gate.open();
    }

    #[test]
    fn the_budget_is_spent_once_for_both_lanes_not_once_each() {
        let workers = Workers::new(&backend());
        let gate = Arc::new(Gate::default());
        occupy(&workers, Lane::Interactive, INTERACTIVE_THREADS, &gate);
        occupy(&workers, Lane::Background, BACKGROUND_THREADS, &gate);
        let started = Instant::now();
        drop(workers);
        assert!(
            started.elapsed() < SHUTDOWN_BUDGET * 2,
            "both lanes were blocked and drop waited longer than one budget: {:?}",
            started.elapsed()
        );
        gate.open();
    }

    #[test]
    fn idle_workers_wake_and_are_joined_well_inside_the_budget() {
        let workers = Workers::new(&backend());
        let started = Instant::now();
        drop(workers);
        assert!(
            started.elapsed() < SHUTDOWN_BUDGET,
            "idle workers should wake on the queue closing, not be waited out: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_job_already_handed_to_a_worker_still_runs_after_the_pool_is_dropped() {
        let workers = Workers::new(&backend());
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        assert!(
            workers
                .submit(
                    Lane::Background,
                    Box::new(move |_| flag.store(true, Ordering::SeqCst))
                )
                .is_ok()
        );
        drop(workers);
        for _ in 0..50 {
            if ran.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ran.load(Ordering::SeqCst), "the job ran to completion");
    }

    #[test]
    fn a_job_still_running_past_the_budget_does_not_make_drop_wait_for_it() {
        let workers = Workers::new(&backend());
        assert!(
            workers
                .submit(
                    Lane::Background,
                    Box::new(|_| std::thread::sleep(SHUTDOWN_BUDGET * 10))
                )
                .is_ok()
        );
        // Give a worker a moment to actually pick the job up, so this exercises dropping while mid-call rather
        // than while still idle.
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(workers);
        assert!(
            started.elapsed() < SHUTDOWN_BUDGET * 3,
            "drop gave up rather than wait out the slow job: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_inline_pool_has_nothing_to_join() {
        let workers = Workers::inline();
        let started = Instant::now();
        drop(workers);
        // A pool that waited for threads it does not have would take the whole of the budget; this is half of it, so a
        // slow, shared machine does not fail the test.
        assert!(
            started.elapsed() < SHUTDOWN_BUDGET / 2,
            "{:?}",
            started.elapsed()
        );
    }
}
