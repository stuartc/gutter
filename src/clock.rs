//! The injectable [`Clock`] the render loop is generic over. See ADR-007.
//!
//! The loop touches time in three places — reading "now", the blocking wait for
//! a frame's first message (`recv`), and the bounded wait while draining to the
//! deadline (`recv_until`). All three go through the trait so a virtual clock can
//! drive the cap, starvation, idle-park and liveness tests on scripted time, with
//! no wall-clock sleeps and no real channel to block on.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// The outcome of a bounded wait, mirroring `mpsc::RecvTimeoutError` but as a
/// success/timeout/disconnect tri-state the loop matches on directly.
pub enum Recv<M> {
    /// A message arrived.
    Msg(M),
    /// The wait hit its deadline with no message (idle-gap exit).
    Timeout,
    /// All senders dropped — the shutdown backstop, not the normal
    /// `ChildExited` route.
    Disconnected,
}

/// The render loop's view of time and its message source. See ADR-007.
///
/// The clock owns the receiver so the loop never touches a raw channel — that is
/// what lets the virtual clock intercept the blocking `recv()`, not just the
/// timed waits. `Instant` is abstract so the virtual clock can use a plain tick
/// counter; the real clock uses `std::time::Instant`.
pub trait Clock {
    /// The message type drained from the channel.
    type Msg;
    /// The instant type — `Copy` and ordered, compared against the deadline.
    type Instant: Copy + Ord;

    /// The current instant. Counts as a wakeup in the virtual clock so the
    /// idle-park test can assert the loop never polls time while parked.
    fn now(&mut self) -> Self::Instant;

    /// The per-frame deadline `from + dur`, captured once per frame.
    fn deadline(&self, from: Self::Instant, dur: Duration) -> Self::Instant;

    /// Blocks until the first message of a frame arrives; `None` once all senders
    /// are gone. This is the loop's only park point — zero idle CPU.
    fn recv(&mut self) -> Option<Self::Msg>;

    /// Blocks for the next message until at most `deadline`.
    /// `Receiver::recv_deadline` is nightly-only, so each impl computes the
    /// remaining timeout and calls the stable `recv_timeout`; the loop stays in
    /// abstract `Instant`s either way.
    fn recv_until(&mut self, deadline: Self::Instant) -> Recv<Self::Msg>;

    /// Milliseconds since the clock was made — the timestamps of a recording
    /// (`src/record.rs`). Not a wakeup: it is only read while recording.
    fn elapsed_ms(&self) -> u64;
}

/// The production clock: real monotonic time and a real blocking channel.
pub struct RealClock<M> {
    rx: Receiver<M>,
    start: Instant,
}

impl<M> RealClock<M> {
    pub fn new(rx: Receiver<M>) -> Self {
        Self {
            rx,
            start: Instant::now(),
        }
    }
}

impl<M> Clock for RealClock<M> {
    type Msg = M;
    type Instant = Instant;

    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn deadline(&self, from: Instant, dur: Duration) -> Instant {
        from + dur
    }

    fn recv(&mut self) -> Option<M> {
        self.rx.recv().ok()
    }

    fn recv_until(&mut self, deadline: Instant) -> Recv<M> {
        use std::sync::mpsc::RecvTimeoutError;
        // If the deadline already passed, saturating to zero makes this a
        // non-blocking poll. The loop's `now >= deadline` check normally
        // prevents that, but don't underflow if it slips through.
        let timeout = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(timeout) {
            Ok(m) => Recv::Msg(m),
            Err(RecvTimeoutError::Timeout) => Recv::Timeout,
            Err(RecvTimeoutError::Disconnected) => Recv::Disconnected,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}
