//! The injectable job-control seam for the suspend/resume cycle (ADR-0019).
//!
//! Mirrors [`crate::pty::PtyResizer`]: the render loop's suspend logic runs
//! against this trait so its tests drive a recorder instead of actually stopping
//! the test process. Production is [`RealSuspender`]; tests use `MockSuspender`.

/// The two job-control side effects the suspend cycle performs.
pub trait Suspender {
    /// Stop gutter's own process group — `kill(0, SIGTSTP)`. Returns only once the
    /// launching shell's `fg` (SIGCONT) resumes the process; execution continues on
    /// the very next statement, same thread, same stack.
    fn suspend_self(&self);
    /// Continue the child's process group — `kill(-child_pgid, SIGCONT)`. `ESRCH`
    /// (child died while gutter was stopped) is ignored.
    fn continue_child(&self);
}

/// The production suspender. `child_pid` is the child's pid, which equals its pgid
/// (portable-pty `setsid`s the child), so `-child_pid` targets its whole group.
pub struct RealSuspender {
    pub child_pid: Option<u32>,
}

impl Suspender for RealSuspender {
    fn suspend_self(&self) {
        // SIGTSTP disposition is default (nothing in gutter or crossterm installs a
        // TSTP handler), so this stops the whole process group with no SIG_DFL dance.
        unsafe {
            libc::kill(0, libc::SIGTSTP);
        }
    }

    fn continue_child(&self) {
        if let Some(pid) = self.child_pid {
            let pgid = -(pid as libc::pid_t);
            // ESRCH ignored: the child may have died while gutter was stopped.
            unsafe {
                libc::kill(pgid, libc::SIGCONT);
            }
        }
    }
}

#[cfg(test)]
pub mod mock {
    //! A recording [`Suspender`] that logs `SuspendSelf` / `ContinueChild` into the
    //! same shared order log the [`crate::terminal::mock::MockTerminal`] writes to,
    //! so a test can assert one interleaved sequence across both mocks (terminal
    //! restore vs self-stop vs continue).

    use std::cell::RefCell;
    use std::rc::Rc;

    use super::Suspender;
    use crate::terminal::mock::Call;

    /// A shared, ordered log of mock calls across the terminal and the suspender.
    pub type OrderLog = Rc<RefCell<Vec<Call>>>;

    pub struct MockSuspender {
        log: OrderLog,
        /// A side effect run inside `suspend_self` (before it records), modelling the
        /// world changing while gutter is stopped — the missed-resize test flips the
        /// mock terminal's reported size here.
        on_suspend: Option<Box<dyn Fn()>>,
    }

    impl MockSuspender {
        /// A suspender that shares `log` with a `MockTerminal::with_log(log)`.
        pub fn new(log: OrderLog) -> Self {
            Self {
                log,
                on_suspend: None,
            }
        }

        /// A standalone suspender with its own throwaway log — for tests that only
        /// need `run` to type-check and never inspect the suspend order.
        pub fn disconnected() -> Self {
            Self::new(Rc::new(RefCell::new(Vec::new())))
        }

        /// Register a side effect fired inside `suspend_self`, modelling the world
        /// changing while gutter is stopped (e.g. an outer-terminal resize).
        pub fn on_suspend(mut self, f: impl Fn() + 'static) -> Self {
            self.on_suspend = Some(Box::new(f));
            self
        }
    }

    impl Suspender for MockSuspender {
        fn suspend_self(&self) {
            if let Some(f) = &self.on_suspend {
                f();
            }
            self.log.borrow_mut().push(Call::SuspendSelf);
        }

        fn continue_child(&self) {
            self.log.borrow_mut().push(Call::ContinueChild);
        }
    }
}
