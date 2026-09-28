//! A small pool of connections to the durable store.
//!
//! # Why this exists
//!
//! The Corndogs client carries one call at a time on one connection. One
//! `CorndogsQueue` once held one such connection behind one lock, so every
//! intake worker, the forwarder, the sweep, and the depth count queued behind
//! each other, and Corndogs only coalesces commits across connections.
//!
//! # What it no longer does
//!
//! It used to give each connection to a worker thread and put the deadline on
//! the wait for that thread, because the client read with no deadline and kept
//! its socket private. From Corndogs commit `23caaf1` the client takes an I/O
//! deadline for the whole call and drops a connection that timed out, so the
//! next call dials again and cannot read a late reply. The deadline is now the
//! client's, the call runs on the caller's thread, and a stuck worker no longer
//! exists to be replaced.
//!
//! What is left is the part that was always the point: a bounded number of
//! connections, one caller on each at a time, and a bounded wait for a free
//! one.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Why a pooled call produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PoolError {
    /// Every connection was in use for the whole wait.
    Busy,
    /// The durable store did not answer within the client's deadline.
    TimedOut,
    /// The connection could not be opened, or the call failed.
    Failed(String),
}

pub(crate) type Factory<C> = Box<dyn Fn() -> Result<C, String> + Send + Sync>;

struct State<C> {
    /// Connections that no caller holds. The last one in is the first one out,
    /// so a quiet process keeps using one connection and never dials the rest.
    idle: Vec<C>,
    /// Connections that exist, held or idle. It never passes `size`.
    open: usize,
}

pub(crate) struct Pool<C> {
    state: Mutex<State<C>>,
    ready: Condvar,
    factory: Factory<C>,
    size: usize,
    /// How long a caller waits for a free connection.
    wait: Duration,
}

impl<C> Pool<C> {
    /// A pool of at most `size` connections. `first` is a connection the caller
    /// already opened, so a durable store that cannot be reached fails the
    /// start rather than the first batch.
    pub(crate) fn new(
        size: usize,
        wait: Duration,
        first: Option<C>,
        factory: Factory<C>,
    ) -> Pool<C> {
        let idle: Vec<C> = first.into_iter().collect();
        Pool {
            state: Mutex::new(State {
                open: idle.len(),
                idle,
            }),
            ready: Condvar::new(),
            factory,
            size: size.max(1),
            wait,
        }
    }

    /// Run one call on a free connection. The connection's own deadline bounds
    /// the call; `timed_out` says whether a failure was that deadline.
    pub(crate) fn run<R>(
        &self,
        call: impl FnOnce(&C) -> Result<R, String>,
        timed_out: impl Fn(&str) -> bool,
    ) -> Result<R, PoolError> {
        let connection = self.check_out()?;
        let outcome = call(&connection);
        // The connection goes back whatever happened. A client that timed out
        // has already dropped its socket, and dials again on its next call.
        self.give_back(connection);
        outcome.map_err(|reason| {
            if timed_out(&reason) {
                PoolError::TimedOut
            } else {
                PoolError::Failed(reason)
            }
        })
    }

    fn check_out(&self) -> Result<C, PoolError> {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(connection) = state.idle.pop() {
                return Ok(connection);
            }
            if state.open < self.size {
                // Count it before the dial, so two callers cannot both open
                // the last one, and dial with no lock held.
                state.open += 1;
                drop(state);
                return (self.factory)().map_err(|reason| {
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.open -= 1;
                    drop(state);
                    self.ready.notify_one();
                    PoolError::Failed(reason)
                });
            }
            let waited = started.elapsed();
            if waited >= self.wait {
                return Err(PoolError::Busy);
            }
            state = self
                .ready
                .wait_timeout(state, self.wait - waited)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn give_back(&self, connection: C) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.idle.push(connection);
        drop(state);
        self.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::Arc;

    /// A connection that is only a number, so a test can see which one served.
    fn numbered(opened: Arc<AtomicUsize>) -> Factory<usize> {
        Box::new(move || Ok(opened.fetch_add(1, Ordering::SeqCst)))
    }

    fn never_timed_out(_: &str) -> bool {
        false
    }

    #[test]
    fn a_quiet_caller_uses_one_connection_and_never_opens_the_rest() {
        let opened = Arc::new(AtomicUsize::new(0));
        let pool = Pool::new(
            4,
            Duration::from_secs(5),
            None,
            numbered(Arc::clone(&opened)),
        );
        for _ in 0..2_000 {
            let served = pool
                .run(|connection| Ok(*connection), never_timed_out)
                .expect("call");
            assert_eq!(
                served, 0,
                "a quiet caller was served on a second connection"
            );
        }
        assert_eq!(opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn two_callers_at_once_are_served_on_two_connections() {
        // The single lock this replaces made the second caller wait for the
        // first one's whole round trip.
        let opened = Arc::new(AtomicUsize::new(0));
        let pool = Arc::new(Pool::new(
            2,
            Duration::from_secs(5),
            None,
            numbered(Arc::clone(&opened)),
        ));
        let (release, held) = channel::<()>();
        let (entered, first_is_in) = channel::<()>();
        let slow = Arc::clone(&pool);
        let slow_call = std::thread::spawn(move || {
            slow.run(
                move |_| {
                    entered.send(()).unwrap();
                    held.recv().unwrap();
                    Ok(())
                },
                never_timed_out,
            )
        });
        first_is_in.recv().unwrap();
        pool.run(|_| Ok(()), never_timed_out)
            .expect("the second call is served");
        release.send(()).unwrap();
        slow_call.join().unwrap().expect("the first call is served");
        assert_eq!(opened.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_durable_store_that_never_answers_costs_the_caller_one_deadline() {
        // The client's deadline ends the call. The pool reports it as the store
        // not answering, and the connection is free for the next caller.
        let opened = Arc::new(AtomicUsize::new(0));
        let pool = Pool::new(
            1,
            Duration::from_secs(5),
            None,
            numbered(Arc::clone(&opened)),
        );
        let failure = pool
            .run(
                |_| -> Result<(), String> { Err("corndogs: read timed out".to_string()) },
                |reason| reason.contains("timed out"),
            )
            .expect_err("no answer is a failure");
        assert_eq!(failure, PoolError::TimedOut);
        assert_eq!(
            pool.run(|connection| Ok(*connection), never_timed_out),
            Ok(0),
            "the same connection serves the next call, and dials again inside the client"
        );
        assert_eq!(opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_caller_that_finds_every_connection_held_is_told_the_store_is_busy() {
        let opened = Arc::new(AtomicUsize::new(0));
        let pool = Arc::new(Pool::new(
            1,
            Duration::from_millis(30),
            None,
            numbered(opened),
        ));
        let (release, held) = channel::<()>();
        let (entered, first_is_in) = channel::<()>();
        let holder = Arc::clone(&pool);
        let holding = std::thread::spawn(move || {
            holder.run(
                move |_| {
                    entered.send(()).unwrap();
                    held.recv().unwrap();
                    Ok(())
                },
                never_timed_out,
            )
        });
        first_is_in.recv().unwrap();
        // The bounded wait is the thing under test, so this call waits 30 ms.
        assert_eq!(pool.run(|_| Ok(()), never_timed_out), Err(PoolError::Busy));
        release.send(()).unwrap();
        holding.join().unwrap().expect("the holder finishes");
    }

    #[test]
    fn a_connection_that_cannot_open_fails_the_call_and_the_next_call_tries_again() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let pool: Pool<usize> = Pool::new(
            1,
            Duration::from_secs(5),
            None,
            Box::new(move || {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("connection refused".to_string())
                } else {
                    Ok(7)
                }
            }),
        );
        assert_eq!(
            pool.run(|connection| Ok(*connection), never_timed_out),
            Err(PoolError::Failed("connection refused".to_string()))
        );
        assert_eq!(
            pool.run(|connection| Ok(*connection), never_timed_out),
            Ok(7)
        );
    }
}
