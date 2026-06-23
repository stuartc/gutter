//! Injectable `Clock` trait (`now()` + a `recv_timeout`-equivalent) so the
//! 60fps coalescing deadline is testable without wall-clock flake. The CI cap
//! and starvation-regression tests drive a virtual clock through this.

// trait Clock {
//     fn now(&self) -> Instant-like;
//     fn recv_timeout(&self, ...) -> Result<Msg, RecvTimeoutError>;
// }
