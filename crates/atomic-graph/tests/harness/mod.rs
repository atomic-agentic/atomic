// SPDX-License-Identifier: Apache-2.0

//! How the conformance suite runs its checks, and what it runs them against.
//!
//! A check is one scenario written once, generic over [`GraphStore`], that
//! reports each expectation that did not hold instead of panicking: an empty
//! `Vec` means the scenario passed. The same body then serves two very
//! different questions, which is why it is written that way:
//!
//! ```text
//! check(&real_store).await            -> nothing, so the backend conforms
//! check(&missing_store).await         -> the operation it stopped at, named
//! ```
//!
//! The missing-store row remains a negative control: it proves incomplete
//! backends report the first missing operation instead of returning an empty
//! success. Production SQLite is run through the full scenario table.
//!
//! The harness uses a small standard-library executor so each scenario can
//! run on a dedicated thread without a Tokio runtime. A separate smoke test
//! verifies production SQLite calls made from an existing Tokio runtime.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::task::Wake;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::store_contract::Actor;
use crate::store_contract::Draft;
use crate::store_contract::EdgeKind;
use crate::store_contract::Evidence;
use crate::store_contract::Finish;
use crate::store_contract::GraphStore;
use crate::store_contract::Holder;
use crate::store_contract::Item;
use crate::store_contract::Lease;
use crate::store_contract::Memory;
use crate::store_contract::Report;
use crate::store_contract::Show;
use crate::store_contract::Status;
use crate::store_contract::StoreError;
use crate::store_contract::Trust;

/// The clock a conformance store reads, and the handle a check moves it with.
///
/// Lease expiry has to be a deterministic assertion rather than a sleep, and
/// the repo's own rule is that a step's recorded clock is the one its store
/// sees — so the suite owns the clock, hands it to the store it constructs, and
/// advances it between operations. A backend whose constructor takes no clock
/// cannot be run through these checks; that is a missing implementation, not a
/// reason to sleep in a test.
#[derive(Debug, Clone)]
pub struct Clock {
    at: Arc<Mutex<SystemTime>>,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    /// A clock starting at a fixed point, so a run never depends on the wall
    /// clock and a `created_at` assertion has an exact answer.
    pub fn new() -> Self {
        Self {
            at: Arc::new(Mutex::new(
                UNIX_EPOCH + Duration::from_secs(1_800_000_000) + Duration::from_millis(500),
            )),
        }
    }

    /// Where the clock is now, for the store to read.
    pub fn now(&self) -> SystemTime {
        *self.lock()
    }

    /// Move the clock forward, which is how a check makes a lease expire or a
    /// `due_at` arrive.
    pub fn advance(&self, by: Duration) {
        let mut at = self.lock();
        *at = at
            .checked_add(by)
            .expect("the suite's clock only moves forward within range");
    }

    fn lock(&self) -> MutexGuard<'_, SystemTime> {
        self.at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Run `future` to completion on this thread.
///
/// The stand-in for a test runtime, explained at the top of this module. A
/// future that parks needs a real I/O driver behind its waker; this one wakes
/// the thread that polls, which is enough for an in-process store and is what a
/// store built on a connection pool will need `#[tokio::test]` for.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = std::task::Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(ready) = future.as_mut().poll(&mut cx) {
            return ready;
        }
        std::thread::park();
    }
}

/// A future boxed so that every racer's future has one type.
pub type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How many racers a race check releases at once. Errai raced forty claims and
/// forty `claim_next`s against a pool of eight; here a racer is a thread, so all
/// forty are real. How many a backend can serve at once is the backend's
/// business — what the check requires is that they are issued together.
pub const RACERS: usize = 40;

/// What a concurrent drive came back with.
#[derive(Debug)]
pub struct Drives<T> {
    /// What each racer returned, in racer order. `None` only where the racer
    /// never answered — it panicked, or it refused to issue its call — and every
    /// such racer is also a `problem`.
    pub answers: Vec<Option<T>>,
    /// What went wrong, one line each. A check that has any of these reports
    /// them and stops: with a racer missing, the counts below it would be
    /// comparing a race that did not happen.
    pub problems: Vec<String>,
    /// How many racers the start gate released — how many were told to issue their
    /// call.
    ///
    /// This is the abort rule measured directly, and it is deliberately not the same
    /// thing as the number of calls issued: a racer the gate released may still be
    /// refused further on, so `answers` and a store's own counters can read zero
    /// while a drive that should have released nobody released some. A drive that
    /// was called off before it started must have `passed` zero.
    pub passed: usize,
}

/// How long one racer waits for the others to reach their call before it gives up
/// and says so.
///
/// A wall-clock bound rather than a spin count, for two reasons. A count is a
/// guess about the machine: this suite runs inside a container under a CPU quota
/// while other lanes share the box, and forty threads yielding while they wait for
/// a fortieth to be scheduled is exactly the case where a count could run out on a
/// drive that was fine. A healthy drive arrives within microseconds and pays
/// nothing here. And a drive that cannot overlap — one that had become a sequential
/// loop — must report itself rather than hang, which is what the bound is for: it
/// is the only reason the wait ends without everyone arriving.
///
/// Sixty seconds rather than ten, because of one measurement. Under load from
/// other lanes, one run of this target's 22 tests as
/// `cargo test -p atomic-agentic-graph` reported `finished in 10.01s` where every
/// other run of it — before and after, including three reruns right after —
/// reports `0.00s`. That run printed no per-test timing, so it does not prove
/// *which* wait it spent; what it shows is that the suite's healthy cost is
/// microseconds and something in that run took ten seconds, in a target that does
/// no I/O and whose driver waits in exactly two places, both bounded here. A bound
/// a healthy run can approach is a bound that will fire on a busier minute, and
/// what it would report is "this was not a race" about a drive that did race. The
/// bound is a report deadline for a drive that is already broken, never a budget a
/// healthy drive spends, so raising it buys the margin at no cost.
const ISSUED_WAIT: Duration = Duration::from_secs(60);

/// How many yields a waiting racer takes before it sleeps instead, so that a drive
/// which is genuinely broken burns no CPU while it times out.
const WAIT_YIELDS: usize = 1024;

/// How long a racer waits at the start gate for the others to arrive.
///
/// Bounded by the clock, for the same reason and to the same measured bound as
/// [`ISSUED_WAIT`], with a second purpose: a racer that is still waiting when this
/// runs out reports it and leaves, so nothing in a drive can hang the test even if
/// a racer is lost for a reason nobody predicted.
///
/// Running out is not that racer's problem alone, and it used to be treated as
/// one: the racer timed out, returned its own report and left the others waiting,
/// which let a racer that arrived afterwards open the gate and release calls into
/// a drive that had already refused. Now the first wait to run out calls the whole
/// drive off — see [`StartGate::arrive`] — and
/// `a_gate_that_times_out_calls_the_drive_off_before_its_racers_issue` proves it.
///
/// The value a gate uses is given to it at construction ([`StartGate::new`])
/// rather than read from here, for the same reason: that test injects a bound of
/// milliseconds so it can watch a timeout happen without spending a minute. Every
/// drive a check runs gets this value.
const START_WAIT: Duration = Duration::from_secs(60);

/// The gate every racer passes through before issuing its call, which opens when
/// all `expected` racers have arrived or when the drive is called off.
///
/// This replaces a `Barrier`, and the reason is a deadlock: `Barrier::new(racers)`
/// releases nobody until `racers` threads reach it, so a drive whose fortieth
/// thread could not be started left the other thirty-nine waiting forever, the
/// driver blocked in `join`, and the "racer could not start" report unreachable —
/// the test hung instead of failing. A gate that can be called off from the
/// spawning side is the minimum machinery that makes that path a reported problem.
///
/// `Mutex`+`Condvar` rather than another `Barrier` because the decision has to be
/// readable *after* release (was this release a call-off, and why), and because a
/// lost racer has to be able to time out and leave, which a barrier never lets a
/// waiter do.
///
/// Two things release it: every racer arriving, and a call-off. A call-off is
/// caused from the spawning side when a thread could not be started, and by a
/// racer whose own wait ran out — the second because a drive that has reported a
/// refusal must not go on issuing calls behind it.
#[derive(Debug)]
struct StartGate {
    state: Mutex<GateState>,
    released: Condvar,
    /// How long one racer waits here, which is [`START_WAIT`] for every drive a
    /// check runs and shorter only for the test that has to watch a timeout.
    wait: Duration,
}

#[derive(Debug)]
struct GateState {
    /// Racers that have arrived and are waiting, or have been released.
    arrived: usize,
    /// How many arrivals open the gate.
    expected: usize,
    /// Released: every arrived racer may act or leave.
    open: bool,
    /// Why the drive was called off, which is the reason a racer reports.
    called_off: Option<String>,
}

impl StartGate {
    /// A gate that opens when `expected` racers arrive, giving up on a wait after
    /// `wait`.
    fn new(expected: usize, wait: Duration) -> Self {
        Self {
            state: Mutex::new(GateState {
                arrived: 0,
                expected,
                open: false,
                called_off: None,
            }),
            released: Condvar::new(),
            wait,
        }
    }

    /// Arrive, wait for release, and say whether this racer may issue its call.
    ///
    /// `Ok(())` means every racer is here and nobody called the drive off. `Err`
    /// carries the reason the call was not issued — a call-off, or a wait that ran
    /// out — and a racer that gets one has issued nothing, so a drive that never
    /// started leaves no partial state behind.
    ///
    /// The wait running out is treated as a call-off rather than as this racer's
    /// own report, which is the difference between a drive that stops and one that
    /// stops *and then acts*. Returning alone left the other racers asleep on the
    /// condvar with the gate neither open nor called off, and a racer that arrived
    /// afterwards — the forty-first millisecond version of a thread that was merely
    /// descheduled — completed the count, opened the gate and released them into
    /// their calls, one of which had already been reported as refused. So the first
    /// racer to run out of patience calls the whole drive off *while still holding
    /// the lock it waited on*: the reason is recorded and everybody is notified
    /// before that racer returns, and there is no window in which a late arrival
    /// can open the gate instead. Arrivals after a call-off are refused at the top
    /// of this method, whatever their count reaches.
    fn arrive(&self) -> Result<(), String> {
        let mut state = take(&self.state);
        let expected = state.expected;
        // A drive that has been called off stays called off. Checked before the
        // arrival is counted, so a late racer cannot be the arrival that opens a
        // gate whose drive has already been refused.
        if state.called_off.is_some() {
            return Self::released_as(&state);
        }
        if state.open {
            return Self::released_as(&state);
        }
        state.arrived += 1;
        if state.arrived >= expected {
            state.open = true;
            self.released.notify_all();
            return Self::released_as(&state);
        }
        let (mut state, _waited) =
            match self
                .released
                .wait_timeout_while(state, self.wait, |state| !state.open)
            {
                Ok(released) => released,
                Err(poisoned) => poisoned.into_inner(),
            };
        if state.open || state.called_off.is_some() {
            return Self::released_as(&state);
        }
        // Out of patience, with the gate still shut and nobody called off. This
        // racer cannot tell whether the missing racers are slow or gone, and it
        // cannot just leave: call the drive off through the guard it is still
        // holding, so the refusal is published to every other racer and to every
        // later arrival before this one reports anything. It then reports the
        // timeout as its own, the way a refused spawn reports itself, so the report
        // says which racer could not get to the gate — and everyone else reports the
        // call-off quoting this reason.
        let reason = format!(
            "waited {wait:?} for {expected} racers to arrive at the start gate and only {} did, \
             so the gate never opened and no call was issued",
            state.arrived,
            wait = self.wait,
        );
        Self::call_off_with(&mut state, &self.released, reason.clone());
        Err(reason)
    }

    /// Call the drive off and release everyone waiting with `reason`.
    fn call_off(&self, reason: String) {
        let mut state = take(&self.state);
        Self::call_off_with(&mut state, &self.released, reason);
    }

    /// The call-off itself, through a guard the caller already holds: record the
    /// first reason, open the gate so nobody waits for a drive that has been
    /// refused, and wake everyone waiting.
    ///
    /// Takes the guard rather than taking one of its own because the racer whose
    /// wait timed out has to decide this *without releasing the lock* — dropping it
    /// here to re-take it is the window a late arrival would open the gate through,
    /// which is the failure this whole path exists to close.
    fn call_off_with(state: &mut GateState, released: &Condvar, reason: String) {
        if state.called_off.is_none() {
            state.called_off = Some(reason);
        }
        state.open = true;
        released.notify_all();
    }

    /// What a released racer should do: act if the gate opened on arrivals, report
    /// the call-off if it did not. Decided here, under the lock, so a call-off that
    /// arrives afterwards cannot change a racer's answer.
    fn released_as(state: &GateState) -> Result<(), String> {
        match &state.called_off {
            None => Ok(()),
            Some(reason) => Err(format!(
                "the drive was called off before it started, so no call was issued: {reason}"
            )),
        }
    }
}

/// Run `every` on `racers` threads at once, released together by a [`StartGate`],
/// and refuse the drive if they were not in fact together.
///
/// This is what turns a race check into a race. Errai's two race tests spawn
/// their forty calls; a sequential `for … await` loop cannot fail them however
/// the store is built, because each call is answered before the next is issued
/// and the store is never asked to choose between two callers. Here a racer says
/// it has arrived, does not issue its call until every racer has arrived, and
/// reports the drive as broken if that never happens — so "issued together" is
/// something the driver asserts about itself rather than something the checks
/// have to trust. Each thread runs its own [`block_on`], since this target has no
/// runtime.
///
/// Four fail-closed rules, because a race that silently lost racers would be
/// worse than no race at all: a racer whose thread could not start calls the gate
/// off, so the racers already waiting report that and no store call of theirs is
/// issued; a racer whose wait at the gate runs out calls it off the same way, so a
/// racer that was merely never scheduled cannot leave a drive that reports a
/// refusal *and* issues calls (see [`StartGate::arrive`], and the test named on
/// [`START_WAIT`]); a racer that panics is a `problem` and gets `None`, never a gap
/// a count could read as a loser; and a racer that parks without a waker hangs the
/// test rather than being answered on its behalf, which is [`block_on`]'s rule
/// unchanged.
pub fn race<'s, S, T, F>(store: &'s S, racers: usize, every: F) -> Drives<T>
where
    S: GraphStore,
    T: Send,
    F: Fn(&'s S, usize) -> Boxed<'s, T> + Sync,
{
    drive(store, racers, &Plan::plain(), every)
}

/// The stack size no thread can be given, so that a racer can be made to fail to
/// start for real, on every run, without the test touching a resource limit or a
/// cgroup.
///
/// Measured here, on Linux x86_64: `pthread_create` answers EINVAL and the racer
/// reports `Invalid argument (os error 22)`. The reason is arithmetic rather than
/// configuration — no address space can back `usize::MAX` bytes of stack — so the
/// refusal does not depend on `RLIMIT_STACK`; it is not measured on macOS, and if
/// some platform accepted the request the consequence is a failed assertion naming
/// the whole problem list, not a drive that quietly passed.
pub const STACK_NO_THREAD: usize = usize::MAX;

/// Like [`race`], except that every racer from `refused_from` on is asked for
/// [`STACK_NO_THREAD`] bytes of stack and so fails to start.
///
/// This exists for one test — the one that has to show a lost racer is *reported*
/// rather than waited for forever — and it is the reason the failure is genuine
/// and mixed at once. Racers below `refused_from` are started for real and really
/// do sit at the start gate; the racer at `refused_from` is refused by the
/// operating system, which is the event that used to hang the drive. A seam that
/// only failed every spawn would prove nothing, because then nobody is ever
/// waiting. It is not a way to make a drive pass: `race`, which every check uses,
/// asks for the platform default at every racer.
pub fn race_with_spawn_refused_from<'s, S, T, F>(
    store: &'s S,
    racers: usize,
    refused_from: usize,
    every: F,
) -> Drives<T>
where
    S: GraphStore,
    T: Send,
    F: Fn(&'s S, usize) -> Boxed<'s, T> + Sync,
{
    assert!(
        refused_from < racers,
        "a drive that refuses no spawn is race()'s, not this one's"
    );
    drive(
        store,
        racers,
        &Plan {
            stack: Box::new(move |racer| {
                if racer >= refused_from {
                    STACK_NO_THREAD
                } else {
                    0
                }
            }),
            ..Plan::plain()
        },
        every,
    )
}

/// Like [`race`], except that racer `racer` waits `delays[racer]` before it arrives
/// at the start gate, and the gate gives up after `gate_wait` rather than after
/// [`START_WAIT`].
///
/// One test uses this, and it needs both halves at once and on schedule. The
/// failure being tested is a racer whose wait at the gate ran out *while another
/// racer was still on its way* — and with every racer leaving at the same moment
/// their deadlines are the same instant, so a real timeout there is followed by no
/// live waiter at all and the difference between refusing and issuing cannot be
/// observed. Staggering the arrivals gives three different deadlines: one racer
/// times out first, a second is still waiting when the delayed racer arrives, and
/// the gate has to be shut for both of them. `gate_wait` is what makes that cost
/// milliseconds instead of the minute the real bound is; the delays are what make
/// the order of events a schedule rather than a hope.
///
/// It is not a way to make a drive pass: `race`, which every check uses, has no
/// delays and the full bound.
pub fn race_with_arrivals_at<'s, S, T, F>(
    store: &'s S,
    racers: usize,
    gate_wait: Duration,
    delays: &[Duration],
    every: F,
) -> Drives<T>
where
    S: GraphStore,
    T: Send,
    F: Fn(&'s S, usize) -> Boxed<'s, T> + Sync,
{
    assert!(
        delays.len() == racers,
        "every racer needs a delay before it arrives: {racers} racers, {} delays",
        delays.len(),
    );
    let delays = delays.to_vec();
    drive(
        store,
        racers,
        &Plan {
            gate_wait,
            before_arrive: Box::new(move |racer| {
                let delay = delays[racer];
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            }),
            ..Plan::plain()
        },
        every,
    )
}

/// How a drive is put together: the seams a test may set, every one of them at its
/// default for the drive that [`race`] runs.
///
/// They are named fields rather than positional arguments because two of them are
/// the same type and the difference between them is the whole point of the test
/// that sets one: being late *at the gate* (`before_arrive`, which is how a racer
/// ends up arriving after another racer's wait has already run out) and being late
/// *as a counted racer* (`on_arrivals`, which is how a racer ends up missing the
/// overlap) are different failures, and a call site that swapped them would still
/// compile.
struct Plan {
    /// Stack bytes to ask for per racer, by racer index; 0 asks for the platform
    /// default. [`STACK_NO_THREAD`] is how a spawn is refused for real.
    stack: Box<dyn Fn(usize) -> usize + Sync>,
    /// How long a racer waits at the start gate. [`START_WAIT`] for every drive a
    /// check runs.
    gate_wait: Duration,
    /// Run before the racer arrives at the gate, so a test can make an arrival late
    /// on schedule. Nothing in a check does this; only the timeout test does.
    before_arrive: Box<dyn Fn(usize) + Sync>,
    /// Run just after the racer counts itself in, so a test can make a *counted*
    /// racer late. See [`driver_reached_lagged`].
    on_arrivals: Box<dyn Fn(usize) + Sync>,
}

impl Plan {
    /// The drive every check in the suite runs: platform-default stacks, the full
    /// [`START_WAIT`] at the gate, and nothing done before or after arriving.
    fn plain() -> Self {
        Self {
            stack: Box::new(|_| 0),
            gate_wait: START_WAIT,
            before_arrive: Box::new(|_| {}),
            on_arrivals: Box::new(|_| {}),
        }
    }
}

/// The drive itself: `racers` threads, each arriving at the gate before it issues
/// `every`, under the [`Plan`] that says what each racer asks for and when it is
/// deliberately late.
fn drive<'s, S, T, F>(store: &'s S, racers: usize, plan: &Plan, every: F) -> Drives<T>
where
    S: GraphStore,
    T: Send,
    F: Fn(&'s S, usize) -> Boxed<'s, T> + Sync,
{
    assert!(racers > 1, "a race needs more than one racer");
    // Declared out here, not inside the scope: a scoped thread may borrow from
    // the caller's frame, and these are what the threads wait on. `inside` counts
    // racers that have reached the call they are about to issue, and is only ever
    // raised, so it says how many got there within this one drive.
    let gate = StartGate::new(racers, plan.gate_wait);
    let inside = AtomicUsize::new(0);
    let passed = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let gate = &gate;
        let passed = &passed;
        let inside = &inside;
        let every = &every;
        let stack = &plan.stack;
        let before_arrive = &plan.before_arrive;
        let on_arrivals = &plan.on_arrivals;
        let mut started: Vec<(usize, std::thread::ScopedJoinHandle<'_, Result<T, String>>)> =
            Vec::with_capacity(racers);
        let mut problems: Vec<String> = Vec::new();
        for racer in 0..racers {
            let mut builder = std::thread::Builder::new();
            builder = builder.name(format!("conformance-racer-{racer}"));
            let wanted = stack(racer);
            if wanted > 0 {
                builder = builder.stack_size(wanted);
            }
            match builder.spawn_scoped(scope, move || {
                // A racer that is late here is late *at the gate*, which is what
                // makes another racer's wait run out first. `Plan::plain` does
                // nothing here, so no check's drive is touched by it.
                before_arrive(racer);
                // Arriving is the first thing a racer does after that, so a racer
                // that never arrived is one whose thread was never started — the
                // other failure the gate is called off for.
                gate.arrive()?;
                // Released by the gate, whatever this racer does next: whether it
                // ever reaches a store is a later question, and a drive that was
                // called off must be able to say it released nobody.
                passed.fetch_add(1, Ordering::SeqCst);
                let start = Instant::now();
                let mut arrived = inside.fetch_add(1, Ordering::SeqCst) + 1;
                // After counting itself in, so a hook that parks this thread is
                // late *as a counted racer*, which is the only lateness that can
                // hide the overlap from it.
                on_arrivals(arrived);
                let mut yields = 0;
                while arrived < racers && start.elapsed() < ISSUED_WAIT {
                    std::thread::yield_now();
                    yields += 1;
                    if yields % WAIT_YIELDS == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    arrived = arrived.max(inside.load(Ordering::SeqCst));
                }
                if arrived != racers {
                    return Err(format!(
                        "only {arrived} of {racers} racers reached their call at the same \
                         instant within {}s, so this was not a race and its answers mean \
                         nothing",
                        ISSUED_WAIT.as_secs()
                    ));
                }
                Ok(block_on(every(store, racer)))
            }) {
                Ok(handle) => started.push((racer, handle)),
                Err(error) => {
                    // A waiting racer cannot discover this one on its own: its
                    // arrival count would never reach `racers`. Call the drive off
                    // first, so everyone already at the gate leaves with this
                    // reason, then report it.
                    let reason = format!("racer {racer} could not start: {error}");
                    gate.call_off(reason.clone());
                    problems.push(reason);
                }
            }
        }
        let mut answers: Vec<Option<T>> = (0..racers).map(|_| None).collect();
        for (racer, handle) in started {
            match handle.join() {
                Err(_) => problems.push(format!(
                    "racer {racer} panicked, so what its call did to the store is unknown"
                )),
                Ok(Err(problem)) => problems.push(format!("racer {racer}: {problem}")),
                Ok(Ok(answer)) => answers[racer] = Some(answer),
            }
        }
        Drives {
            answers,
            problems,
            passed: passed.load(Ordering::SeqCst),
        }
    })
}

/// The store [`driver_reached`] drives: no operation of it is ever called,
/// because the drive's futures count instead of calling anything. It exists so
/// that the drive goes through [`race`] itself, which is the thing being measured.
struct NoStore;

impl GraphStore for NoStore {
    async fn create_item(
        &self,
        _actor: Actor,
        _request_id: String,
        _draft: Draft,
    ) -> Result<Item, StoreError> {
        missing("create_item")
    }

    async fn note(
        &self,
        _actor: Actor,
        _lease: Lease,
        _request_id: String,
        _text: String,
    ) -> Result<(), StoreError> {
        missing("note")
    }

    async fn add_edge(
        &self,
        _actor: Actor,
        _from: String,
        _kind: EdgeKind,
        _to: String,
    ) -> Result<(), StoreError> {
        missing("add_edge")
    }

    async fn set_status(
        &self,
        _actor: Actor,
        _id: String,
        _status: Status,
    ) -> Result<(), StoreError> {
        missing("set_status")
    }

    async fn show(&self, _actor: Actor, _id: String) -> Result<Option<Show>, StoreError> {
        missing("show")
    }

    async fn all_items(&self, _actor: Actor) -> Result<Vec<Item>, StoreError> {
        missing("all_items")
    }

    async fn ready(&self, _actor: Actor, _limit: usize) -> Result<Vec<Item>, StoreError> {
        missing("ready")
    }

    async fn claim(
        &self,
        _actor: Actor,
        _claimant: String,
        _id: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        missing("claim")
    }

    async fn claim_next(
        &self,
        _actor: Actor,
        _claimant: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        missing("claim_next")
    }

    async fn heartbeat(&self, _lease: Lease, _lease_for: Duration) -> Result<(), StoreError> {
        missing("heartbeat")
    }

    async fn finish(&self, _lease: Lease, _done: Finish) -> Result<(), StoreError> {
        missing("finish")
    }

    async fn sweep(&self, _actor: Actor) -> Result<Vec<String>, StoreError> {
        missing("sweep")
    }

    async fn remember(
        &self,
        _actor: Actor,
        _request_id: String,
        _key: String,
        _value: String,
    ) -> Result<(), StoreError> {
        missing("remember")
    }

    async fn prompt_memories(
        &self,
        _actor: Actor,
        _limit: usize,
    ) -> Result<Vec<Memory>, StoreError> {
        missing("prompt_memories")
    }

    async fn set_close_policy(
        &self,
        _actor: Actor,
        _requires: Vec<Evidence>,
    ) -> Result<(), StoreError> {
        missing("set_close_policy")
    }

    async fn link_commit(&self, _lease: Lease, _sha: String) -> Result<(), StoreError> {
        missing("link_commit")
    }

    async fn import(
        &self,
        _actor: Actor,
        _jsonl: String,
        _dry_run: bool,
    ) -> Result<Report, StoreError> {
        missing("import")
    }

    async fn export(&self, _actor: Actor) -> Result<String, StoreError> {
        missing("export")
    }
}

/// The store reference [`driver_reached`] hands its racers. A `&` of a `const` is
/// `'static`, which is what lets the drive outlive nothing at all.
const NO_STORE: NoStore = NoStore;

/// What the counted racers share: how many are inside, and the highest that has
/// ever been measured at once.
///
/// The shared high-water mark is the point. A racer measures the overlap by
/// reading a counter, and a thread descheduled between raising that counter and
/// reading it back can miss the instant the suite came to measure — which is what
/// a container under a CPU quota does to forty threads. If each racer then trusted
/// only its own reading, its remaining exit would be to watch the counter *fall*,
/// and a fall only shows that somebody left, which is weaker than the fact the
/// drive is asserting. So every arrival and every reading raises one shared peak,
/// and a racer that starts late sees the overlap that was already measured through
/// it: the wait ends on something that happened, not on whether this thread
/// happened to be scheduled while it did.
///
/// The peak is a measurement and not a hope. The counter is raised on entry and
/// lowered only after a racer has finished measuring, so a peak of `target` means
/// `target` racers were resident at one instant — and only the racer whose own
/// arrival or own reading saw that number ever raises the peak to it.
#[derive(Debug)]
pub struct Reached {
    /// Racers inside their counted call right now.
    inside: AtomicUsize,
    /// The largest `inside` ever measured, by any racer, at arrival or at a read.
    peak: AtomicUsize,
    /// How many racers inside their counted call counts as the overlap.
    target: usize,
    /// When the peak first reached `target`, which is how a test tells a racer that
    /// exited on the shared measurement from one that exited because the count
    /// fell, and proves the overlap was known before a parked racer resumed.
    full_at: Mutex<Option<Instant>>,
}

impl Reached {
    /// A counter whose overlap is `target` racers inside at once.
    pub fn new(target: usize) -> Self {
        assert!(target > 0, "a counted overlap of nobody is not an overlap");
        Self {
            inside: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            target,
            full_at: Mutex::new(None),
        }
    }

    /// How many racers inside counts as the overlap.
    pub fn target(&self) -> usize {
        self.target
    }

    /// How many racers are inside right now.
    pub fn inside_now(&self) -> usize {
        self.inside.load(Ordering::SeqCst)
    }

    /// The most ever measured inside at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// Whether the overlap has been measured — the shared fact every racer may
    /// stop on, rather than a reading this thread had to be scheduled to take.
    pub fn full_seen(&self) -> bool {
        self.peak() >= self.target
    }

    /// When the overlap was first measured, if it has been.
    pub fn full_at(&self) -> Option<Instant> {
        *take(&self.full_at)
    }

    /// Arrive, and raise the shared peak if this arrival is what made `target`
    /// racers resident at once. Returns how many were inside after arriving.
    fn arrive(&self) -> usize {
        let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
        self.note(now);
        now
    }

    /// Take one measurement: read the counter and raise the shared peak with what
    /// it says now, which is how a reading by any racer becomes available to every
    /// other one.
    fn measure(&self) {
        let now = self.inside.load(Ordering::SeqCst);
        self.note(now);
    }

    /// Leave. Every counted racer lowers the counter on every path, so a racer
    /// that reported a problem still leaves the count as it found it.
    fn leave(&self) {
        self.inside.fetch_sub(1, Ordering::SeqCst);
    }

    /// Record one measurement: raise the peak, and stamp the instant the overlap
    /// was first reached. `now` always comes from this racer's own arrival or own
    /// reading of the counter, which is what keeps the peak a measurement.
    fn note(&self, now: usize) {
        if self.peak.fetch_max(now, Ordering::SeqCst) < now && now >= self.target {
            let mut at = take(&self.full_at);
            if at.is_none() {
                *at = Some(Instant::now());
            }
        }
    }
}

/// Drive [`race`] with futures that count instead of calling a store, and report
/// how many racers it had inside their call at the same instant.
///
/// This is the driver measured on its own. Until the crate has a store both race
/// checks stop at `create_item`, so nothing downstream of the driver runs, and a
/// `race` that had degraded into a sequential loop would fail no check at all —
/// a sequential drive passes both against any backend whatever it does. Here the
/// same driver is handed futures that raise `inside` on entry, wait for the other
/// racers to enter, read the count and lower it again, so the number reported is
/// how many the driver really held at once. Against a sequential driver the first
/// future finishes before the second starts, every wait runs out, and the count
/// it reports is one.
///
/// `overlap.target()` is how many counted racers inside counts as the overlap,
/// which for a normal drive is `racers`; a smaller target is only for a test that
/// wants the counters to measure fewer arrivals than the drive starts threads —
/// see [`Reached`] and the lagging-racer test that uses it.
///
/// The live call is a counter rather than a store call because a store cannot be
/// counted from outside one; what this proves is the release discipline that the
/// store calls of the real checks are issued under.
pub fn driver_reached(racers: usize, overlap: &Reached) -> Drives<usize> {
    counted(racers, overlap, None, Duration::ZERO)
}

/// [`driver_reached`] with one racer parked on purpose, so that a test can be late
/// on schedule instead of hoping the scheduler is rude enough to be late for it.
///
/// The racer that counts itself in at the driver as number `late_at` sleeps
/// `late_for` before it issues its call, so it reaches the counted overlap after
/// the others have come and gone. For that to be a race it missed rather than a
/// race it was late for, `racers` must exceed `overlap.target()`: if the parked
/// racer were needed to complete the overlap, it would be the one that measures it
/// on arrival and there would be nothing to test.
///
/// Test-only in the strict sense: reached solely by the lagging-racer test, with no
/// environment variable, configuration file or atom able to ask for a parked racer.
#[cfg(test)]
pub fn driver_reached_lagged(
    racers: usize,
    overlap: &Reached,
    late_at: usize,
    late_for: Duration,
) -> Drives<usize> {
    counted(racers, overlap, Some(late_at), late_for)
}

/// The counted drive behind [`driver_reached`] and [`driver_reached_lagged`].
fn counted(
    racers: usize,
    overlap: &Reached,
    late_at: Option<usize>,
    late_for: Duration,
) -> Drives<usize> {
    assert!(
        overlap.target() <= racers,
        "the drive starts {racers} racers and the counters measure an overlap of {}, so the \
         overlap could never be reached",
        overlap.target()
    );
    let on_arrivals = move |arrived: usize| {
        if Some(arrived) == late_at {
            std::thread::sleep(late_for);
        }
    };
    if let Some(late_at) = late_at {
        assert!(
            late_at > 0 && late_at <= racers,
            "the parked racer has to be one of the {racers} this drive starts (late_at \
             {late_at}), or nothing is parked",
        );
        assert!(
            racers > overlap.target(),
            "an overlap of {} out of {racers} counted racers leaves nobody to measure it once \
             one of them is parked — the parked racer cannot be the one that completes it, or \
             it could not have missed it",
            overlap.target()
        );
    }
    drive(
        &NO_STORE,
        racers,
        &Plan {
            on_arrivals: Box::new(on_arrivals),
            ..Plan::plain()
        },
        |_, _| {
            let count = Count { overlap };
            Box::pin(async move { count.count().await })
        },
    )
}

/// One counted racer's future: enter, wait for the others to enter, read the
/// count, leave.
struct Count<'a> {
    overlap: &'a Reached,
}

impl Count<'_> {
    /// Arrive, watch until the overlap is known, leave, and report the most ever
    /// measured inside at once.
    ///
    /// Two ways this wait ends:
    ///
    /// * the shared peak reaches the target. It may reach it through this racer's
    ///   own arrival or reading, or through another racer's — and taking it from the
    ///   shared peak rather than from this thread's own reading is the whole point:
    ///   a racer that was descheduled through the overlap cannot read it again,
    ///   because by the time it is scheduled the other racers have gone, and the
    ///   previous shape of this loop did exactly that and spent its whole bound
    ///   doing so. `a_counted_racer_that_missed_the_overlap_exits_on_what_others_measured`
    ///   is the proof, and switching either the starting value or the loop's reading
    ///   back to this thread's own measurement is what makes it fail;
    /// * [`ISSUED_WAIT`] runs out, which is the last-resort stop for a racer that
    ///   was never scheduled at all. It reports the peak as measured, so a drive
    ///   that never had every racer inside together is reported as not a race by the
    ///   assertion over the answers rather than by a special case here.
    ///
    /// What it returns is the shared peak, not a personal best: the overlap it
    /// reports is one this racer took part in and was entitled to read, which is a
    /// different claim from "this thread saw forty racers at once", and the driver's
    /// assertion over the answers is written for the shared reading.
    async fn count(self) -> usize {
        let start = Instant::now();
        self.overlap.arrive();
        // Start from what has been measured, not from what this thread can see right
        // now: every arrival and every reading raises the shared peak.
        let mut peak = self.overlap.peak();
        let mut yields = 0;
        while peak < self.overlap.target() && start.elapsed() < ISSUED_WAIT {
            std::thread::yield_now();
            yields += 1;
            if yields % WAIT_YIELDS == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.overlap.measure();
            peak = self.overlap.peak();
        }
        self.overlap.leave();
        peak
    }
}

/// A store that has no implementation for anything: what `atomic-graph` is
/// today, expressed as the trait.
///
/// Every operation answers [`StoreError::MissingImplementation`] naming itself,
/// which is exactly the answer a store that does not exist yet must give. It
/// writes nothing, so it cannot make a check pass, and it is what the
/// conformance target runs the checks against before the SQLite backend lands.
#[derive(Debug, Default)]
pub struct Unavailable {
    clock: Clock,
}

impl Unavailable {
    /// The missing store, carrying a clock it will never be asked to read.
    pub fn new(clock: Clock) -> Self {
        Self { clock }
    }

    /// What its clock reads now, which is what a store built the same way
    /// reads at each operation.
    ///
    /// `#[allow(dead_code)]`: nothing in the crate constructs a store that
    /// reads a clock yet, so only a real backend calls this. It goes away with
    /// the store gate named in the conformance target's `PENDING_STORE`.
    #[allow(dead_code)]
    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }
}

impl GraphStore for Unavailable {
    async fn create_item(
        &self,
        _actor: Actor,
        _request_id: String,
        _draft: Draft,
    ) -> Result<Item, StoreError> {
        missing("create_item")
    }

    async fn note(
        &self,
        _actor: Actor,
        _lease: Lease,
        _request_id: String,
        _text: String,
    ) -> Result<(), StoreError> {
        missing("note")
    }

    async fn add_edge(
        &self,
        _actor: Actor,
        _from: String,
        _kind: EdgeKind,
        _to: String,
    ) -> Result<(), StoreError> {
        missing("add_edge")
    }

    async fn set_status(
        &self,
        _actor: Actor,
        _id: String,
        _status: Status,
    ) -> Result<(), StoreError> {
        missing("set_status")
    }

    async fn show(&self, _actor: Actor, _id: String) -> Result<Option<Show>, StoreError> {
        missing("show")
    }

    async fn all_items(&self, _actor: Actor) -> Result<Vec<Item>, StoreError> {
        missing("all_items")
    }

    async fn ready(&self, _actor: Actor, _limit: usize) -> Result<Vec<Item>, StoreError> {
        missing("ready")
    }

    async fn claim(
        &self,
        _actor: Actor,
        _claimant: String,
        _id: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        missing("claim")
    }

    async fn claim_next(
        &self,
        _actor: Actor,
        _claimant: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        missing("claim_next")
    }

    async fn heartbeat(&self, _lease: Lease, _lease_for: Duration) -> Result<(), StoreError> {
        missing("heartbeat")
    }

    async fn finish(&self, _lease: Lease, _done: Finish) -> Result<(), StoreError> {
        missing("finish")
    }

    async fn sweep(&self, _actor: Actor) -> Result<Vec<String>, StoreError> {
        missing("sweep")
    }

    async fn remember(
        &self,
        _actor: Actor,
        _request_id: String,
        _key: String,
        _value: String,
    ) -> Result<(), StoreError> {
        missing("remember")
    }

    async fn prompt_memories(
        &self,
        _actor: Actor,
        _limit: usize,
    ) -> Result<Vec<Memory>, StoreError> {
        missing("prompt_memories")
    }

    async fn set_close_policy(
        &self,
        _actor: Actor,
        _requires: Vec<Evidence>,
    ) -> Result<(), StoreError> {
        missing("set_close_policy")
    }

    async fn link_commit(&self, _lease: Lease, _sha: String) -> Result<(), StoreError> {
        missing("link_commit")
    }

    async fn import(
        &self,
        _actor: Actor,
        _jsonl: String,
        _dry_run: bool,
    ) -> Result<Report, StoreError> {
        missing("import")
    }

    async fn export(&self, _actor: Actor) -> Result<String, StoreError> {
        missing("export")
    }
}

/// The answer every [`Unavailable`] operation gives.
fn missing<T>(operation: &'static str) -> Result<T, StoreError> {
    Err(StoreError::MissingImplementation { operation })
}

/// How many `claim_next` answers [`NonAtomic`] hands out without reserving the
/// item it handed out: the racing calls, which is what a lost reservation looks
/// like from outside. Every caller that arrived together is given the same item,
/// and the drain that follows walks the rest of the queue once each and ends, so
/// the check fails on the duplicate rather than in a loop.
const UNRESERVED_HANDOUTS: usize = RACERS;

/// Take a stub's lock, recovering it if a racer panicked while holding it.
///
/// Not a fallback that hides a value: nothing is substituted for what cannot be
/// read. Each write below is one `insert` into a map, which either happened or
/// did not, so a recovered lock holds exactly the entries the racers wrote; and
/// [`race`] reports the panic itself and stops the check before any of these
/// answers is compared with anything.
fn take<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A backend whose claims are not atomic, which is the defect the two race
/// checks exist to catch.
///
/// `claim` hands an item to every claimant that asks and remembers only the
/// first as its holder; `claim_next` gives the same item to every caller that
/// arrived with it. Errai's two race tests were written against exactly this
/// shape — one holder out of forty, and nothing given out twice — and until a
/// store exists in the crate there is nothing else in this suite a race check
/// can fail against: against [`Unavailable`] both checks stop at `create_item`,
/// so their race assertions are unreachable. That unreachability is why this
/// stub is here, and why the conformance target drives both checks against it
/// (`control_e_…`, `control_f_…`) instead of only against the missing store. Take
/// either rule out of this stub — let `claim` reserve on first touch, or let
/// `claim_next` reserve before it answers — and the control that names it stops
/// failing, which is this control tested by removing it.
///
/// It answers `create_item`, `claim`, `claim_next` and `show`, and refuses the
/// other fourteen operations by name, so it is a defect made concrete and not a
/// store.
#[derive(Debug, Default)]
pub struct NonAtomic {
    clock: Clock,
    items: Mutex<BTreeMap<String, Item>>,
    holders: Mutex<BTreeMap<String, Holder>>,
    /// What `claim_next` has reserved, which is deliberately not written by the
    /// racing calls.
    reserved: Mutex<BTreeSet<String>>,
    /// Leases this stub handed out, which is what its control asserts alongside
    /// the check's report.
    leases: AtomicUsize,
    /// `claim_next` calls served, so the stub knows which ones raced.
    calls: AtomicUsize,
}

impl NonAtomic {
    /// The defect, with an empty graph. Takes a clock for the same reason every
    /// store does: `create_item` stamps `created_at` from it.
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            items: Mutex::new(BTreeMap::new()),
            holders: Mutex::new(BTreeMap::new()),
            reserved: Mutex::new(BTreeSet::new()),
            leases: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        }
    }

    /// How many leases this stub told a caller it held.
    pub fn leases_issued(&self) -> usize {
        self.leases.load(Ordering::SeqCst)
    }

    /// How many items this stub records as held.
    pub fn holders_recorded(&self) -> usize {
        take(&self.holders).len()
    }
}

impl GraphStore for NonAtomic {
    async fn create_item(
        &self,
        actor: Actor,
        _request_id: String,
        draft: Draft,
    ) -> Result<Item, StoreError> {
        let item = Item {
            id: draft.id.clone(),
            title: draft.title,
            body: draft.body,
            kind: draft.kind,
            priority: draft.priority,
            status: draft.status,
            parent: draft.parent,
            trust: if actor.human {
                Trust::Human
            } else {
                Trust::Agent
            },
            created_by: actor.principal,
            created_at: self.clock.now(),
            closed_at: None,
            due_at: draft.due_at,
        };
        take(&self.items).insert(item.id.clone(), item.clone());
        Ok(item)
    }

    async fn note(
        &self,
        _actor: Actor,
        _lease: Lease,
        _request_id: String,
        _text: String,
    ) -> Result<(), StoreError> {
        missing("note")
    }

    async fn add_edge(
        &self,
        _actor: Actor,
        _from: String,
        _kind: EdgeKind,
        _to: String,
    ) -> Result<(), StoreError> {
        missing("add_edge")
    }

    async fn set_status(
        &self,
        _actor: Actor,
        _id: String,
        _status: Status,
    ) -> Result<(), StoreError> {
        missing("set_status")
    }

    async fn show(&self, _actor: Actor, id: String) -> Result<Option<Show>, StoreError> {
        let item = take(&self.items).get(&id).cloned();
        let Some(item) = item else {
            return Ok(None);
        };
        let holder = take(&self.holders).get(&id).cloned();
        let status = if holder.is_some() {
            Status::InProgress
        } else {
            item.status
        };
        Ok(Some(Show {
            item: Item { status, ..item },
            notes: Vec::new(),
            blocked_by: Vec::new(),
            holder,
        }))
    }

    async fn all_items(&self, _actor: Actor) -> Result<Vec<Item>, StoreError> {
        missing("all_items")
    }

    async fn ready(&self, _actor: Actor, _limit: usize) -> Result<Vec<Item>, StoreError> {
        missing("ready")
    }

    async fn claim(
        &self,
        _actor: Actor,
        claimant: String,
        id: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        // The defect, in one line: the item is handed out whether or not someone
        // already holds it. `or_insert` keeps the *first* holder in the store's
        // own record, so the mismatch between the leases issued and the holder
        // recorded is visible to a check that reads the item back.
        take(&self.holders).entry(id.clone()).or_insert(Holder {
            claimant: claimant.clone(),
            epoch: 1,
        });
        self.leases.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Lease {
            item_id: id,
            claimant,
            session: 1,
            epoch: 1,
            acquisition: "non-atomic-acquisition".to_string(),
        }))
    }

    async fn claim_next(
        &self,
        _actor: Actor,
        claimant: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let items = take(&self.items);
        let mut reserved = take(&self.reserved);
        let taken = if call < UNRESERVED_HANDOUTS {
            // The racing calls: the reservation is written by nobody, so every
            // caller that arrived together is given the lowest item.
            items.keys().next().cloned()
        } else {
            items.keys().find(|id| !reserved.contains(*id)).cloned()
        };
        let Some(item_id) = taken else {
            return Ok(None);
        };
        if call >= UNRESERVED_HANDOUTS {
            reserved.insert(item_id.clone());
        }
        drop(reserved);
        drop(items);
        self.leases.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Lease {
            item_id,
            claimant,
            session: 1,
            epoch: 1,
            acquisition: "non-atomic-acquisition".to_string(),
        }))
    }

    async fn heartbeat(&self, _lease: Lease, _lease_for: Duration) -> Result<(), StoreError> {
        missing("heartbeat")
    }

    async fn finish(&self, _lease: Lease, _done: Finish) -> Result<(), StoreError> {
        missing("finish")
    }

    async fn sweep(&self, _actor: Actor) -> Result<Vec<String>, StoreError> {
        missing("sweep")
    }

    async fn remember(
        &self,
        _actor: Actor,
        _request_id: String,
        _key: String,
        _value: String,
    ) -> Result<(), StoreError> {
        missing("remember")
    }

    async fn prompt_memories(
        &self,
        _actor: Actor,
        _limit: usize,
    ) -> Result<Vec<Memory>, StoreError> {
        missing("prompt_memories")
    }

    async fn set_close_policy(
        &self,
        _actor: Actor,
        _requires: Vec<Evidence>,
    ) -> Result<(), StoreError> {
        missing("set_close_policy")
    }

    async fn link_commit(&self, _lease: Lease, _sha: String) -> Result<(), StoreError> {
        missing("link_commit")
    }

    async fn import(
        &self,
        _actor: Actor,
        _jsonl: String,
        _dry_run: bool,
    ) -> Result<Report, StoreError> {
        missing("import")
    }

    async fn export(&self, _actor: Actor) -> Result<String, StoreError> {
        missing("export")
    }
}

/// A backend that answers the operations it is told to with a value, and refuses
/// the rest as missing.
///
/// A store that refuses everything is the easy case to catch. The one that
/// passes conformance by accident is the one that answers `Ok` where the check
/// asked for a refusal: the close that has to be held is allowed, the write that
/// has to be fenced goes through, and a runner that treated an unexpected `Ok`
/// as success would say nothing at all. This is the backend that drives that
/// half — see `control_d_a_refusal_that_was_allowed_is_reported` in the
/// conformance target, which is where the runner's refusal rule is tested by
/// removing it.
///
/// It answers the four operations that check needs and knows no others, so it
/// cannot be mistaken for a store: the other fourteen still refuse by name.
#[derive(Debug)]
pub struct Partial {
    answers: &'static [&'static str],
    clock: Clock,
}

impl Partial {
    /// A backend answering exactly `answers`, which has to be operations this
    /// stub knows — `create_item`, `claim`, `set_close_policy` and `finish`.
    ///
    /// A name outside those four is a mistake in the check that asked for it, so
    /// it fails here rather than quietly answering nothing and sending the check
    /// off at a missing implementation it was not looking for.
    pub fn new(answers: &'static [&'static str], clock: Clock) -> Self {
        const KNOWN: [&str; 4] = ["create_item", "claim", "set_close_policy", "finish"];
        for answer in answers {
            assert!(
                KNOWN.contains(answer),
                "Partial answers only {KNOWN:?}, so {answer} would be answered by nothing",
            );
        }
        Self { answers, clock }
    }

    /// Answer with `value` if this backend was told to answer `operation`, and
    /// refuse it as missing otherwise.
    fn answered<T>(&self, operation: &'static str, value: T) -> Result<T, StoreError> {
        if self.answers.contains(&operation) {
            Ok(value)
        } else {
            Err(StoreError::MissingImplementation { operation })
        }
    }
}

impl GraphStore for Partial {
    async fn create_item(
        &self,
        actor: Actor,
        _request_id: String,
        draft: Draft,
    ) -> Result<Item, StoreError> {
        let principal = actor.principal;
        self.answered(
            "create_item",
            Item {
                id: draft.id,
                title: draft.title,
                body: draft.body,
                kind: draft.kind,
                priority: draft.priority,
                status: draft.status,
                parent: draft.parent,
                trust: if actor.human {
                    Trust::Human
                } else {
                    Trust::Agent
                },
                created_by: principal,
                created_at: self.clock.now(),
                closed_at: None,
                due_at: draft.due_at,
            },
        )
    }

    async fn note(
        &self,
        _actor: Actor,
        _lease: Lease,
        _request_id: String,
        _text: String,
    ) -> Result<(), StoreError> {
        missing("note")
    }

    async fn add_edge(
        &self,
        _actor: Actor,
        _from: String,
        _kind: EdgeKind,
        _to: String,
    ) -> Result<(), StoreError> {
        missing("add_edge")
    }

    async fn set_status(
        &self,
        _actor: Actor,
        _id: String,
        _status: Status,
    ) -> Result<(), StoreError> {
        missing("set_status")
    }

    async fn show(&self, _actor: Actor, _id: String) -> Result<Option<Show>, StoreError> {
        missing("show")
    }

    async fn all_items(&self, _actor: Actor) -> Result<Vec<Item>, StoreError> {
        missing("all_items")
    }

    async fn ready(&self, _actor: Actor, _limit: usize) -> Result<Vec<Item>, StoreError> {
        missing("ready")
    }

    async fn claim(
        &self,
        _actor: Actor,
        claimant: String,
        id: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        self.answered(
            "claim",
            Some(Lease {
                item_id: id,
                claimant,
                session: 1,
                epoch: 1,
                acquisition: "synthetic-acquisition".to_string(),
            }),
        )
    }

    async fn claim_next(
        &self,
        _actor: Actor,
        _claimant: String,
        _lease_for: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        missing("claim_next")
    }

    async fn heartbeat(&self, _lease: Lease, _lease_for: Duration) -> Result<(), StoreError> {
        missing("heartbeat")
    }

    /// The operation this stub exists to make possible: an answer where the
    /// check asked for a refusal.
    async fn finish(&self, _lease: Lease, _done: Finish) -> Result<(), StoreError> {
        self.answered("finish", ())
    }

    async fn sweep(&self, _actor: Actor) -> Result<Vec<String>, StoreError> {
        missing("sweep")
    }

    async fn remember(
        &self,
        _actor: Actor,
        _request_id: String,
        _key: String,
        _value: String,
    ) -> Result<(), StoreError> {
        missing("remember")
    }

    async fn prompt_memories(
        &self,
        _actor: Actor,
        _limit: usize,
    ) -> Result<Vec<Memory>, StoreError> {
        missing("prompt_memories")
    }

    async fn set_close_policy(
        &self,
        _actor: Actor,
        _requires: Vec<Evidence>,
    ) -> Result<(), StoreError> {
        self.answered("set_close_policy", ())
    }

    async fn link_commit(&self, _lease: Lease, _sha: String) -> Result<(), StoreError> {
        missing("link_commit")
    }

    async fn import(
        &self,
        _actor: Actor,
        _jsonl: String,
        _dry_run: bool,
    ) -> Result<Report, StoreError> {
        missing("import")
    }

    async fn export(&self, _actor: Actor) -> Result<String, StoreError> {
        missing("export")
    }
}

/// Run one check's operations, reporting what did not hold.
///
/// The runner is the suite's only place where a store's answer turns into a
/// pass or a failure, so all of its fail-closed rules live here:
///
/// - An [`StoreError::MissingImplementation`] is reported as the operation it
///   names, and the check stops there: with no store there is nothing left to
///   assert, and stopping is also what keeps one missing operation from
///   stacking up a hundred meaningless failures.
/// - Any other error is reported with its message. Nothing here turns an error,
///   an absent item or an empty list into a default that could satisfy an
///   expectation: `ok` refuses `Err`, `none` refuses `Err` and `Some`, `some`
///   refuses `Err` and `None`.
/// - A store that answers `Ok(empty)` where the check needs a row fails on the
///   empty result, which is a decision the store made, not an absence to fill
///   in.
#[derive(Debug)]
pub struct Run {
    /// What has not held so far.
    problems: Vec<String>,
}

impl Run {
    /// A check that has found nothing yet.
    pub fn new() -> Self {
        Self {
            problems: Vec::new(),
        }
    }

    /// Record that an expectation did not hold, naming it.
    pub fn fail(&mut self, what: &str) {
        self.problems.push(what.to_string());
    }

    /// Report a store error against the expectation that met it.
    ///
    /// A missing implementation is reported on its own, with no expectation
    /// text in front of it: the reason a store could not answer is the thing a
    /// reader needs first, and it is what the conformance target asserts
    /// exactly. Every other error keeps the expectation it came from, because
    /// "expected the item to stay held, got …" is the sentence that explains a
    /// backend's behaviour.
    fn report(&mut self, expected: &str, error: &StoreError) {
        match error {
            StoreError::MissingImplementation { .. } => self.fail(&error.to_string()),
            other => self.fail(&format!("{expected}: {other}")),
        }
    }

    /// What did not hold, in the order it was found. Empty means the check
    /// passed, which is the shape a real backend is run for.
    pub fn finish(self) -> Vec<String> {
        self.problems
    }

    /// Take the value of a call that had to succeed, or report the failure and
    /// give `None` so the caller returns early.
    pub async fn ok<T, F, Fut>(&mut self, operation: &str, future: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, StoreError>>,
    {
        match future().await {
            Ok(value) => Some(value),
            Err(error) => {
                self.report(operation, &error);
                None
            }
        }
    }

    /// Take an answer that had to be `Some`, or report what it was. `nothing`
    /// says what the caller expected the store to have, so that a refusal that
    /// should have named a reason says it.
    pub async fn some<T, F, Fut>(&mut self, expected: &str, future: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<T>, StoreError>>,
        T: std::fmt::Debug,
    {
        match future().await {
            Ok(Some(value)) => Some(value),
            Ok(None) => {
                self.fail(&format!("expected {expected}, got nothing"));
                None
            }
            Err(error) => {
                self.report(expected, &error);
                None
            }
        }
    }

    /// Take an answer that had to be `None`, or report what came back.
    pub async fn none<T, F, Fut>(&mut self, expected: &str, future: F) -> Option<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<T>, StoreError>>,
        T: std::fmt::Debug,
    {
        match future().await {
            Ok(None) => Some(()),
            Ok(Some(value)) => {
                self.fail(&format!("expected no {expected}, got {value:?}"));
                None
            }
            Err(error) => {
                self.report(&format!("expected no {expected}"), &error);
                None
            }
        }
    }

    /// Take an answer that had to be a particular refusal, or report what
    /// happened instead. `want` names what the refusal must be — the variant
    /// and the item or action it names — so a check asserts the reason and not
    /// merely that something failed. A missing implementation is reported as
    /// such, which still stops the check: no store decided that refusal.
    ///
    /// The `Ok` arm is a control in its own right, and it has one: the
    /// conformance target's `control_d_a_refusal_that_was_allowed_is_reported`
    /// drives a check against [`Partial`], a backend that allows the close the
    /// check asks to be held, and asserts the allowance is reported. Change it
    /// to report success and that test fails.
    pub async fn refused<T, F, Fut, P>(&mut self, expected: &str, want: P, future: F) -> Option<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, StoreError>>,
        T: std::fmt::Debug,
        P: FnOnce(&StoreError) -> bool,
    {
        match future().await {
            Ok(value) => {
                self.fail(&format!("expected {expected}, got {value:?}"));
                None
            }
            Err(error) if want(&error) => Some(()),
            Err(error) => {
                self.report(&format!("expected {expected}"), &error);
                None
            }
        }
    }

    /// Fold the answers of a concurrent drive into the run, and keep going only
    /// when every racer answered with `Ok`.
    ///
    /// Two rules here, and both are about not letting a race quietly become a
    /// pass:
    ///
    /// - A [`StoreError::MissingImplementation`] is reported once, bare, naming
    ///   its operation. Forty racers stopped at one absent operation are one
    ///   absent operation, and forty identical lines would bury the rest of a
    ///   run.
    /// - Any other refusal is reported with the expectation, because `claim` and
    ///   `claim_next` answer `Ok(None)` when someone else won: a refused racer
    ///   is a store that declined to answer the question, not a claimant that
    ///   lost, and a check must not read one as the other.
    pub fn folded_ok<T>(
        &mut self,
        expected: &str,
        answers: Vec<Result<T, StoreError>>,
    ) -> Option<Vec<T>> {
        let mut values = Vec::with_capacity(answers.len());
        let mut reported: BTreeSet<String> = BTreeSet::new();
        let mut refused = false;
        for answer in answers {
            match answer {
                Ok(value) => values.push(value),
                Err(error) => {
                    refused = true;
                    let text = error.to_string();
                    if reported.insert(text.clone()) {
                        if matches!(error, StoreError::MissingImplementation { .. }) {
                            self.fail(&text);
                        } else {
                            self.fail(&format!("{expected}: {other}", other = text));
                        }
                    }
                }
            }
        }
        (!refused).then_some(values)
    }

    /// The item ids of a `ready` or `sweep` answer, sorted, so a check compares
    /// sets and not one backend's ordering.
    pub fn ids(&self, items: &[Item]) -> Vec<String> {
        let mut ids: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
        ids.sort();
        ids
    }

    /// The ids of a `ready` answer that are due, with the rest reported.
    pub fn due_ids(&mut self, items: &[Item], not_due: &[&str]) -> Vec<String> {
        let ids = self.ids(items);
        let unexpected: BTreeSet<String> = not_due
            .iter()
            .map(|id| (*id).to_string())
            .filter(|id| ids.contains(id))
            .collect();
        if !unexpected.is_empty() {
            self.fail(&format!(
                "ready listed what is not due or never ready: {}",
                unexpected.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        ids
    }
}

/// The store backends these checks still have to be run against.
/// This is the pending half of the M5 acceptance clause "the ported suite
/// compiles against the store trait and fails for the right reason": the suite
/// compiles and fails for the right reason today, and it passes only once each
/// row here runs. It is read by a test that asserts the shape of each row, so
/// the list cannot quietly lose the backend it names.
pub fn pending_backends() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "postgres",
            "run the production GraphStore scenarios against a PostgreSQL implementation under postgres-tests; no PostgreSQL GraphStore exists yet",
        ),
        (
            "tenancy",
            "two-project isolation needs the tenant-aware store planned for M7",
        ),
        (
            "beads-differential",
            "compare ready sets and exported fields with a pinned bd release, as M5 acceptance asks",
        ),
    ]
}

/// How long a lease is meant to last in a check that never lets it expire.
pub const LONG: Duration = Duration::from_secs(3600);

/// How long a lease lasts in a check that then makes it expire by moving the
/// clock, never by sleeping.
pub const LAPSES: Duration = Duration::from_millis(1);

/// How far past a lapse the clock moves before the check asserts again.
pub const PAST: Duration = Duration::from_millis(20);

/// The person every scenario assumes, for the actions only a person may take.
pub fn person() -> Actor {
    Actor::human("synthetic-person")
}

/// The agent every scenario assumes, for everything an atom does.
pub fn atom() -> Actor {
    Actor::agent("synthetic-agent")
}
