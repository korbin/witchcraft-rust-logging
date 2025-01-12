// Copyright 2019 Palantir Technologies, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use crate::Clock;
use parking_lot::Mutex;
use std::convert::TryFrom;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const INTERVAL_SECS: u64 = 5;
const SECONDS_PER_MINUTE: f64 = 60.;

struct State {
    count: i64,
    rate_10s: Ewma,
    rate_30s: Ewma,
    rate_1m: Ewma,
    rate_5m: Ewma,
    rate_15m: Ewma,
}

/// A metric tracking the rate of occurrence of an event.
///
/// The meter tracks rolling average rates in the same manner as the Linux kernel's load factor measurement.
pub struct Meter {
    uncounted: AtomicI64,
    last_tick: AtomicU64,
    start_time: Instant,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl Default for Meter {
    fn default() -> Meter {
        Meter::new()
    }
}

impl Meter {
    /// Creates a new meter with a [`SystemClock`](crate::SystemClock).
    pub fn new() -> Meter {
        Meter::new_with(crate::SYSTEM_CLOCK.clone())
    }

    /// Creates a new meter using the provided [`Clock`] as its time source.
    pub fn new_with(clock: Arc<dyn Clock>) -> Meter {
        Meter {
            uncounted: AtomicI64::new(0),
            last_tick: AtomicU64::new(0),
            start_time: clock.now(),
            clock,
            state: Mutex::new(State {
                count: 0,
                rate_10s: Ewma::new(0.16),
                rate_30s: Ewma::new(0.5),
                rate_1m: Ewma::new(1.),
                rate_5m: Ewma::new(5.),
                rate_15m: Ewma::new(15.),
            }),
        }
    }

    /// Mark the occurrence of `n` event(s).
    pub fn mark(&self, n: i64) {
        self.tick_if_necessary();
        self.uncounted.fetch_add(n, Ordering::SeqCst);
    }

    /// Returns the number of events registered by the meter.
    pub fn count(&self) -> i64 {
        self.state.lock().count + self.uncounted.load(Ordering::SeqCst)
    }

    /// Returns the ten second rolling average rate of the occurrence of events measured in events per second.
    pub fn ten_second_rate(&self) -> f64 {
        self.tick_if_necessary();
        self.state.lock().rate_10s.get()
    }

    /// Returns the thirty second rolling average rate of the occurrence of events measured in events per second.
    pub fn thirty_second_rate(&self) -> f64 {
        self.tick_if_necessary();
        self.state.lock().rate_30s.get()
    }

    /// Returns the one minute rolling average rate of the occurrence of events measured in events per second.
    pub fn one_minute_rate(&self) -> f64 {
        self.tick_if_necessary();
        self.state.lock().rate_1m.get()
    }

    /// Returns the five minute rolling average rate of the occurrence of events measured in events per second.
    pub fn five_minute_rate(&self) -> f64 {
        self.tick_if_necessary();
        self.state.lock().rate_5m.get()
    }

    /// Returns the fifteen minute rolling average rate of the occurrence of events measured in events per second.
    pub fn fifteen_minute_rate(&self) -> f64 {
        self.tick_if_necessary();
        self.state.lock().rate_15m.get()
    }

    /// Returns the mean rate of the occurrence of events since the creation of the meter measured in events per second.
    pub fn mean_rate(&self) -> f64 {
        let count = self.count() as f64;
        if count == 0. {
            0.
        } else {
            let time = (self.clock.now() - self.start_time).as_secs_f64();
            count / time
        }
    }

    fn tick_if_necessary(&self) {
        let time = self.clock.now();
        let old_tick = self.last_tick.load(Ordering::SeqCst);
        let new_tick = (time - self.start_time).as_secs();
        let age = new_tick - old_tick;

        if age < INTERVAL_SECS {
            return;
        }

        let new_interval_start_tick = new_tick - age % INTERVAL_SECS;
        if self
            .last_tick
            .compare_exchange(
                old_tick,
                new_interval_start_tick,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            // another thread has already ticked for us
            return;
        }

        let required_ticks = age / INTERVAL_SECS;
        let mut state = self.state.lock();

        let uncounted = self.uncounted.swap(0, Ordering::SeqCst);
        state.count += uncounted;

        state.rate_10s.tick(uncounted);
        state.rate_10s.decay(required_ticks - 1);

        state.rate_30s.tick(uncounted);
        state.rate_30s.decay(required_ticks - 1);

        state.rate_1m.tick(uncounted);
        state.rate_1m.decay(required_ticks - 1);

        state.rate_5m.tick(uncounted);
        state.rate_5m.decay(required_ticks - 1);

        state.rate_15m.tick(uncounted);
        state.rate_15m.decay(required_ticks - 1);
    }
}

// Modeled after Java metrics-core's EWMA.java
struct Ewma {
    rate: f64,
    alpha: f64,
    initialized: bool,
}

impl Ewma {
    fn new(minutes: f64) -> Ewma {
        Ewma {
            rate: 0.,
            alpha: 1. - (-(INTERVAL_SECS as f64) / SECONDS_PER_MINUTE / minutes).exp(),
            initialized: false,
        }
    }

    fn tick(&mut self, count: i64) {
        let instant_rate = count as f64 / INTERVAL_SECS as f64;
        if self.initialized {
            self.rate += self.alpha * (instant_rate - self.rate);
        } else {
            self.rate = instant_rate;
            self.initialized = true;
        }
    }

    /// Equivalent to calling ewma.tick(0) `ticks` times, but isn't linear in `ticks`.
    ///
    /// x1 = x0 + alpha * (0 - x0)
    /// x1 = x0 - alpha * x0
    /// x1 = x0 * (1 - alpha)
    ///
    /// x2 = x1 * (1 - alpha)
    /// x2 = x0 * (1 - alpha) * (1 - alpha)
    fn decay(&mut self, ticks: u64) {
        match i32::try_from(ticks) {
            Ok(ticks) => self.rate *= (1. - self.alpha).powi(ticks),
            Err(_) => self.rate = 0.,
        }
    }

    fn get(&self) -> f64 {
        self.rate
    }
}

#[cfg(test)]
mod test {
    use crate::clock::test::TestClock;
    use crate::Meter;
    use assert_approx_eq::assert_approx_eq;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    #[allow(clippy::float_cmp)]
    fn starts_out_with_no_rates_or_count() {
        let clock = Arc::new(TestClock::new());
        let meter = Meter::new_with(clock);

        assert_eq!(meter.count(), 0);
        assert_eq!(meter.one_minute_rate(), 0.);
        assert_eq!(meter.five_minute_rate(), 0.);
        assert_eq!(meter.fifteen_minute_rate(), 0.);
        assert_eq!(meter.mean_rate(), 0.)
    }

    #[test]
    fn marks_events_and_updates_rate_and_count() {
        let clock = Arc::new(TestClock::new());
        let meter = Meter::new_with(clock.clone());

        meter.mark(1);

        clock.advance(Duration::from_secs(10));
        meter.mark(2);

        assert_approx_eq!(meter.mean_rate(), 0.3, 0.001);
        assert_approx_eq!(meter.one_minute_rate(), 0.1840, 0.001);
        assert_approx_eq!(meter.five_minute_rate(), 0.1966, 0.001);
        assert_approx_eq!(meter.fifteen_minute_rate(), 0.1988, 0.001);
    }
}
