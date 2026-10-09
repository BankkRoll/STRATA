//! Timeouts for blocking calls on network paths.
//!
//! Win32 file calls on an SMB share can block for minutes when the server
//! stops answering, and there is no way to force-cancel an arbitrary blocking
//! call. Each walker thread therefore hands its network requests to a
//! dedicated I/O thread and waits with a deadline. On timeout (or walk
//! cancellation) it calls `CancelSynchronousIo` on that thread, which aborts
//! most pending SMB requests with `ERROR_OPERATION_ABORTED`. If the call
//! still has not returned after a short grace period, the I/O thread is
//! abandoned: it is detached, finishes (or stays blocked) on its own, and a
//! fresh one serves the next request. An abandoned thread costs one stack
//! until the redirector gives up; the walk itself is never stuck.

use std::cell::RefCell;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};

use crate::CancelToken;
use crate::sys::ThreadHandle;

type Job = Box<dyn FnOnce() + Send>;

/// How long to wait for a cancelled call to unwind before abandoning it.
const GRACE: Duration = Duration::from_millis(250);

/// Wake-up interval while waiting, so walk cancellation is noticed quickly.
const POLL: Duration = Duration::from_millis(50);

/// Why a timed call produced no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimedError {
    /// The deadline passed.
    TimedOut,
    /// The walk was cancelled while waiting.
    Cancelled,
}

impl TimedError {
    /// Equivalent OS error for classification (`ERROR_TIMEOUT` /
    /// `ERROR_OPERATION_ABORTED`).
    pub(crate) fn to_io(self) -> io::Error {
        io::Error::from_raw_os_error(match self {
            Self::TimedOut => 1460,
            Self::Cancelled => 995,
        })
    }
}

struct IoThread {
    jobs: Sender<Job>,
    thread: Arc<ThreadHandle>,
}

impl IoThread {
    fn spawn() -> io::Result<Self> {
        let (jobs, rx): (Sender<Job>, Receiver<Job>) = unbounded();
        let (htx, hrx) = bounded(1);
        std::thread::Builder::new()
            .name("strata-walk-io".into())
            .spawn(move || {
                let _ = htx.send(ThreadHandle::current().map(Arc::new));
                for job in rx {
                    job();
                }
            })?;
        let thread = hrx
            .recv()
            .map_err(|_| io::Error::other("I/O thread exited during startup"))??;
        Ok(Self { jobs, thread })
    }
}

/// Per-thread runner. Not shared: each walker thread owns one I/O thread so
/// a cancellation can never hit another worker's request.
#[derive(Default)]
pub(crate) struct TimedRunner {
    io: Option<IoThread>,
}

impl std::fmt::Debug for TimedRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedRunner")
            .field("has_thread", &self.io.is_some())
            .finish()
    }
}

impl TimedRunner {
    /// Runs `f` on this runner's I/O thread, waiting at most `timeout`.
    pub(crate) fn run<T, F>(
        &mut self,
        timeout: Duration,
        cancel: &CancelToken,
        f: F,
    ) -> Result<T, TimedError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (rtx, rrx) = bounded::<T>(1);
        let job: Job = Box::new(move || {
            let _ = rtx.send(f());
        });
        let io = match self.io.take() {
            Some(io) => io,
            None => IoThread::spawn().map_err(|_| TimedError::TimedOut)?,
        };
        if io.jobs.send(job).is_err() {
            return Err(TimedError::TimedOut);
        }
        let deadline = Instant::now() + timeout;
        let reason = loop {
            let wait = deadline.saturating_duration_since(Instant::now()).min(POLL);
            match rrx.recv_timeout(wait) {
                Ok(v) => {
                    self.io = Some(io);
                    return Ok(v);
                }
                Err(RecvTimeoutError::Disconnected) => break TimedError::TimedOut,
                Err(RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        break TimedError::Cancelled;
                    }
                    if Instant::now() >= deadline {
                        break TimedError::TimedOut;
                    }
                }
            }
        };
        io.thread.cancel_sync_io();
        if rrx.recv_timeout(GRACE).is_ok() {
            // NOTE: the call unwound after the cancel; the thread is idle and
            // reusable, but the result is from an aborted request: discard it.
            self.io = Some(io);
        }
        Err(reason)
    }
}

thread_local! {
    static RUNNER: RefCell<TimedRunner> = RefCell::default();
}

/// Runs `f` on the calling thread's [`TimedRunner`].
pub(crate) fn run_timed<T, F>(
    timeout: Duration,
    cancel: &CancelToken,
    f: F,
) -> Result<T, TimedError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    RUNNER.with(|r| r.borrow_mut().run(timeout, cancel, f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn fast_call_returns_value() {
        let mut r = TimedRunner::default();
        let v = r.run(Duration::from_secs(5), &CancelToken::new(), || 41 + 1);
        assert_eq!(v, Ok(42));
        assert_eq!(
            r.run(Duration::from_secs(5), &CancelToken::new(), || 1),
            Ok(1)
        );
    }

    #[test]
    fn slow_call_times_out_and_runner_recovers() {
        let mut r = TimedRunner::default();
        let start = Instant::now();
        let v = r.run(Duration::from_millis(100), &CancelToken::new(), || {
            std::thread::sleep(Duration::from_secs(3));
        });
        assert_eq!(v, Err(TimedError::TimedOut));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(
            r.run(Duration::from_secs(5), &CancelToken::new(), || 7),
            Ok(7)
        );
    }

    #[test]
    fn cancellation_interrupts_wait() {
        let mut r = TimedRunner::default();
        let token = CancelToken::new();
        let t2 = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            t2.cancel();
        });
        let start = Instant::now();
        let v = r.run(Duration::from_secs(30), &token, || {
            std::thread::sleep(Duration::from_secs(3));
        });
        assert_eq!(v, Err(TimedError::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// A synchronous pipe read is a real blocking kernel call that
    /// `CancelSynchronousIo` can abort, standing in for a hung SMB request.
    #[test]
    fn cancel_synchronous_io_unblocks_a_kernel_wait() {
        let (mut out, writer) = crate::sys::fixture::sync_pipe().expect("pipe");
        let mut r = TimedRunner::default();
        let (done_tx, done_rx) = bounded(1);
        let v = r.run(Duration::from_millis(300), &CancelToken::new(), move || {
            let mut b = [0u8; 1];
            let res = out.read(&mut b);
            let _ = done_tx.send(res.map_err(|e| e.raw_os_error()));
        });
        assert_eq!(v, Err(TimedError::TimedOut));
        let unwound = done_rx.recv_timeout(Duration::from_secs(2));
        drop(writer);
        assert_eq!(
            unwound,
            Ok(Err(Some(995))),
            "read should be aborted with ERROR_OPERATION_ABORTED"
        );
    }
}
