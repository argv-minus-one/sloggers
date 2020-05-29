//! A simple retry implementation that never gives up and drops log entries rather than sleeping.

use slog::{Drain, Level, OwnedKVList, Record};
use std::cell::RefCell;
use std::time::{Duration, Instant};

/// How long to wait between retries.
const RETRY_TIME: Duration = Duration::from_millis(50);

struct RetryState<D> {
    current_drain: Option<D>,
    dropped_logs: usize,
    last_try_time: Instant,
}

impl<D> RetryState<D> {
    /// Safely increments the count of log messages dropped.
    fn incr_dropped_logs(&mut self) {
        self.dropped_logs = self.dropped_logs.saturating_add(1);
    }

    /// Checks if it's time to try again. If it is, the timeout is also reset.
    fn should_try_again(&mut self) -> bool {
        let now = Instant::now();

        if now.saturating_duration_since(self.last_try_time) < RETRY_TIME {
            false
        }
        else {
            self.last_try_time = now;
            true
        }
    }
}

pub struct Retry<D, N> {
    new_drain: N,
    state: RefCell<RetryState<D>>,
}

impl<D, E, N> Retry<D, N>
where D: Drain, N: Fn() -> Result<D, E> {
    pub fn new(new_drain: N) -> Result<Retry<D, N>, E> {
        let drain = new_drain()?;

        Ok(Retry {
            new_drain,
            state: RefCell::new(RetryState {
                current_drain: Some(drain),
                dropped_logs: 0,
                last_try_time: Instant::now(),
            }),
        })
    }

    /// Fudges the retry timeout so that it times out after an hour. Used for testing.
    #[cfg(test)]
    fn fudge_timeout_long(&self) {
        self.state.borrow_mut().last_try_time = Instant::now() + Duration::from_secs(3600) - RETRY_TIME;
    }

    /// Fudges the retry timeout so that it times out instantly. Used for testing.
    #[cfg(test)]
    fn fudge_timeout_instant(&self) {
        self.state.borrow_mut().last_try_time = Instant::now() - RETRY_TIME;
    }
}

impl<D, E, N> Drain for Retry<D, N>
where D: Drain, N: Fn() -> Result<D, E> {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &Record,
        values: &OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        let mut state = self.state.borrow_mut();

        // If a drain is already there, try to use it.
        if let Some(drain) = &state.current_drain {
            if drain.log(record, values).is_ok() {
                // Logged successfully. Good. We're done here.
                return Ok(());
            }
            else {
                // Failed. Drop the failed drain and start recovering.
                state.current_drain = None;
            }
        }

        // If that failed, then we need a new drain. First, check if it's been long enough since the last attempt.
        if !state.should_try_again() {
            // It hasn't been enough time yet. Give it a while.
            state.incr_dropped_logs();
            return Ok(());
        }

        // Ok, it's been long enough. Try again.
        let drain: D = {
            if let Ok(drain) = (self.new_drain)() {
                drain
            }
            else {
                // Nope, failed. Try again later.
                state.incr_dropped_logs();
                return Ok(());
            }
        };

        // Cool, got a new drain. If any messages were dropped, send a log message saying so.
        if state.dropped_logs != 0 {
            let log_message_result = drain.log(
                &record!(
                    Level::Error,
                    "sloggers::syslog",
                    &format_args!("sloggers::syslog: disconnected from log service; {} messages dropped", state.dropped_logs),
                    b!("count" => state.dropped_logs)
                ),
                values
            );

            if log_message_result.is_err() {
                // Nope, failed. Try again later.
                state.incr_dropped_logs();
                return Ok(());
            }

            // At this point, the count of dropped messages has been logged successfully, so reset that counter.
            state.dropped_logs = 0;
        }

        // Now, send the original log message.
        if drain.log(record, values).is_err() {
            // Nope, failed. Try again later.
            state.incr_dropped_logs();
            return Ok(());
        }

        // Everything went through. Great. Keep the new drain.
        state.current_drain = Some(drain);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use slog::{Key, KV, Serializer};
    use std::cell::Cell;
    use std::fmt;
    use super::*;

    #[derive(Default)]
    struct CountExtractor {
        count: Option<usize>,
    }

    impl Serializer for CountExtractor {
        fn emit_arguments(&mut self, _: Key, _: &fmt::Arguments) -> slog::Result {
            Ok(())
        }

        fn emit_usize(&mut self, key: Key, val: usize) -> slog::Result {
            if key == "count" {
                self.count = Some(val);
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    struct MockDrainError;

    #[derive(Debug)]
    struct MockDrainCtorError;

    #[derive(Default)]
    struct MockDrainState {
        dropped_logs: usize,
        received_logs: usize,
        more_errors: usize,
    }

    #[derive(Default)]
    struct MockDrain {
        state: RefCell<MockDrainState>,
    }

    impl Drain for MockDrain {
        type Ok = ();
        type Err = MockDrainError;

        fn log(
            &self,
            record: &Record,
            _: &OwnedKVList,
        ) -> Result<Self::Ok, Self::Err> {
            let mut state = self.state.borrow_mut();

            if state.more_errors != 0 {
                state.more_errors = state.more_errors.saturating_sub(1);
                eprintln!("Rejecting log message: {}", record.msg());
                return Err(MockDrainError);
            }

            if record.msg().to_string().starts_with("sloggers::syslog: disconnected from log service") {
                let mut ex = CountExtractor {
                    count: None,
                };
                record.kv().serialize(record, &mut ex).unwrap();
                let count = ex.count.expect("no count key");
                state.dropped_logs += count;
                eprintln!("Detected {} messages dropped.", count);
            }
            else {
                state.received_logs += 1;
                eprintln!("Accepting log message: {}", record.msg());
            }

            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Counters {
        pub ok: usize,
        pub drop: usize,
        pub ctor: usize,
        pub log_err: usize,
        pub ctor_err: usize,
    }

    #[test]
    fn test_retry() {
        let mock_drain = MockDrain::default();
        let ctor_count = Cell::new(0usize);
        let ctor_fail_count = Cell::new(0usize);

        let drain = Retry::new(|| {
            ctor_count.set(ctor_count.get() + 1);

            let fc = ctor_fail_count.get();
            if fc == 0 {
                Ok(&mock_drain)
            }
            else {
                ctor_fail_count.set(fc - 1);
                Err(MockDrainCtorError)
            }
        }).unwrap();

        let do_test_message = || -> () {
            drain.log(&record!(Level::Info, "", &format_args!("test message"), b!()), &o!().into()).unwrap();
        };

        let get_counts = || -> Counters {
            let state = mock_drain.state.borrow();
            Counters {
                ok: state.received_logs,
                drop: state.dropped_logs,
                ctor: ctor_count.get(),
                log_err: state.more_errors,
                ctor_err: ctor_fail_count.get(),
            }
        };

        // Send some test messages. Should go through without error.
        for n in 1..=4 {
            do_test_message();
            assert_eq!(get_counts(), Counters { ok: n, drop: 0, ctor: 1, log_err: 0, ctor_err: 0});
        }

        // Now, generate some errors.
        mock_drain.state.borrow_mut().more_errors = 4;
        drain.fudge_timeout_long();

        // Sending several messages before the timeout runs out should decrement `more_errors` by only 1, and not change the construction counts yet.
        for _ in 1..=4 {
            do_test_message();
            assert_eq!(get_counts(), Counters { ok: 4, drop: 0, ctor: 1, log_err: 3, ctor_err: 0});
        }

        // Resetting the timeout and *then* sending a log message should increase the ctor count and decrease the more_errors count.
        for n in 1..=3 {
            drain.fudge_timeout_instant();
            do_test_message();
            assert_eq!(get_counts(), Counters { ok: 4, drop: 0, ctor: 1 + n, log_err: 3 - n, ctor_err: 0});
        }

        // Now, waiting one more time should send the log through successfully.
        drain.fudge_timeout_instant();
        do_test_message();
        assert_eq!(get_counts(), Counters { ok: 5, drop: 7, ctor: 5, log_err: 0, ctor_err: 0 });

        // Now, test what happens when constructing new drains fails.
        mock_drain.state.borrow_mut().more_errors = 1;
        ctor_fail_count.set(4);
        drain.fudge_timeout_instant();

        for n in 1..=4 {
            do_test_message();
            assert_eq!(get_counts(), Counters { ok: 5, drop: 7, ctor: 5 + n, log_err: 0, ctor_err: 4 - n });
            drain.fudge_timeout_instant();
        }

        // Again, this final try should work.
        do_test_message();
        assert_eq!(get_counts(), Counters { ok: 6, drop: 11, ctor: 10, log_err: 0, ctor_err: 0 });
    }
}
