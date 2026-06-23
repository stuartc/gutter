//! The injectable [`Clock`] the render loop is generic over (ADR-007).
//!
//! The 60fps coalescing loop must be deterministically testable with no
//! wall-clock sleeps. The loop's behaviour depends on time in three places, and
//! ALL three must be interceptable or a virtual-clock test would still touch the
//! wall clock (or block forever on a real channel):
//!
//! - reading "now" (`now` / `deadline`),
//! - the **blocking** wait for the first message of a frame (`recv`), and
//! - the **bounded** wait while draining to the deadline (`recv_timeout`).
//!
//! `Receiver::recv_deadline` is nightly-only, so the real bounded wait is the
//! stable `recv_timeout(Duration)` — but the loop works in abstract `Instant`s,
//! so the trait exposes the bounded wait as `recv_until(deadline)` and each
//! impl computes the remaining timeout internally. The real impl
//! ([`RealClock`]) wraps `std::time::Instant` and a real
//! `std::sync::mpsc::Receiver`; the virtual impl (in the render-loop tests)
//! advances time only when the test scripts it and resolves both receives
//! against a scripted queue — so the cap, starvation, idle-park and
//! input-liveness tests are deterministic with no sleeps.
//!
//! The `Clock` owns the receiver so the loop never touches a raw channel: that
//! is what lets the virtual clock intercept the blocking `recv()` too (otherwise
//! the idle-park test would block forever instead of proving zero wakeups).

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// The outcome of a bounded wait, mirroring `mpsc::RecvTimeoutError` but as a
/// success/timeout/disconnect tri-state the loop matches on directly.
pub enum Recv<M> {
    /// A message arrived.
    Msg(M),
    /// The wait hit its deadline with no message (idle-gap exit).
    Timeout,
    /// All senders dropped — the shutdown backstop (ADR-009/010).
    Disconnected,
}

/// The render loop's view of time and its message source (ADR-007).
///
/// `Instant` is abstract so the virtual clock can use a plain tick counter; the
/// real clock uses `std::time::Instant`.
pub trait Clock {
    /// The message type drained from the channel.
    type Msg;
    /// The instant type — `Copy` and ordered, compared against the deadline.
    type Instant: Copy + Ord;

    /// The current instant. Counts as a wakeup in the virtual clock so the
    /// idle-park test can assert the loop never polls time while parked.
    fn now(&mut self) -> Self::Instant;

    /// `from + dur` — the per-frame deadline, captured once.
    fn deadline(&self, from: Self::Instant, dur: Duration) -> Self::Instant;

    /// Block until the first message of a frame arrives (Phase A). `None` means
    /// all senders are gone (the loop then exits via the backstop). This is the
    /// single park point — zero idle CPU.
    fn recv(&mut self) -> Option<Self::Msg>;

    /// Block for the next message until at most `deadline` (Phase B). Mirrors
    /// `Receiver::recv_timeout(deadline - now)` — the impl computes the
    /// remaining timeout — so the loop logic is identical under both clocks.
    fn recv_until(&mut self, deadline: Self::Instant) -> Recv<Self::Msg>;
}

/// The production clock: real monotonic time and a real blocking channel.
pub struct RealClock<M> {
    rx: Receiver<M>,
}

impl<M> RealClock<M> {
    pub fn new(rx: Receiver<M>) -> Self {
        Self { rx }
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
        // saturating: if the deadline already passed, a zero timeout is a
        // non-blocking poll. The loop's explicit `now >= deadline` check
        // normally prevents calling this past the deadline, but stay safe.
        let timeout = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(timeout) {
            Ok(m) => Recv::Msg(m),
            Err(RecvTimeoutError::Timeout) => Recv::Timeout,
            Err(RecvTimeoutError::Disconnected) => Recv::Disconnected,
        }
    }
}
