//! Following the host's clock, and deciding how to correct drift.
//!
//! Guests never trust their own wall clock: PING/PONG round trips give the offset between
//! the guest's monotonic clock and the host's (`host = local + offset`), using the sample
//! with the shortest round trip, which has the least error.

use std::collections::VecDeque;
use std::time::Duration;

const SAMPLES: usize = 8;
/// Below this, playback counts as in sync.
pub const IN_SYNC_MS: i64 = 40;
/// Above this, jump instead of nudging the speed.
pub const HARD_SEEK_MS: i64 = 500;
/// Speed change used to catch up or fall back.
pub const RATE_STEP: f32 = 0.02;
const MAX_NUDGE: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Clock {
    /// (round trip, offset) in ms.
    samples: VecDeque<(i64, i64)>,
}

impl Clock {
    /// A PONG arrived: `t0` was sent at local time `t0`, answered at `host_time`, received at `t1`.
    pub fn pong(&mut self, t0: i64, host_time: i64, t1: i64) {
        let rtt = (t1 - t0).max(0);
        let offset = host_time + rtt / 2 - t1;
        self.samples.push_back((rtt, offset));
        if self.samples.len() > SAMPLES {
            self.samples.pop_front();
        }
    }

    pub fn offset(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|s| s.0).map(|s| s.1)
    }

    pub fn round_trip(&self) -> Option<i64> {
        self.samples.iter().map(|s| s.0).min()
    }

    /// The host's clock now, if known yet.
    pub fn host_now(&self, local_now: i64) -> Option<i64> {
        self.offset().map(|o| local_now + o)
    }
}

/// Where the host's playback is now, given its last heartbeat.
pub fn expected_position(hb_position: i64, hb_host_time: i64, host_now: i64, playing: bool) -> i64 {
    if playing { hb_position + (host_now - hb_host_time).max(0) } else { hb_position }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Correction {
    InSync,
    /// Play at this speed for this long.
    Rate(f32, Duration),
    /// Jump to this position.
    Seek(i64),
}

/// How to bring `local` (ms) to `expected` (ms).
pub fn correction(local: i64, expected: i64, can_change_rate: bool) -> Correction {
    let drift = local - expected;
    if drift.abs() <= IN_SYNC_MS {
        return Correction::InSync;
    }
    if drift.abs() > HARD_SEEK_MS || !can_change_rate {
        return Correction::Seek(expected.max(0));
    }
    // Ahead: slow down; behind: speed up. At ±2 % the gap closes by 20 ms per second.
    let rate = if drift > 0 { 1.0 - RATE_STEP } else { 1.0 + RATE_STEP };
    let secs = drift.abs() as f32 / 1000.0 / RATE_STEP;
    Correction::Rate(rate, Duration::from_secs_f32(secs).min(MAX_NUDGE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_from_the_fastest_round_trip() {
        let mut c = Clock::default();
        // Host clock is 10 000 ms ahead. A slow sample (200 ms) and a fast one (20 ms).
        c.pong(1000, 11_150, 1200);
        c.pong(2000, 12_010, 2020);
        assert_eq!(c.round_trip(), Some(20));
        assert_eq!(c.offset(), Some(10_000));
        assert_eq!(c.host_now(5000), Some(15_000));
    }

    #[test]
    fn expected_positions() {
        assert_eq!(expected_position(41_000, 1_000, 3_500, true), 43_500);
        assert_eq!(expected_position(41_000, 1_000, 3_500, false), 41_000);
    }

    #[test]
    fn three_drift_regions() {
        assert_eq!(correction(10_020, 10_000, true), Correction::InSync);
        match correction(10_300, 10_000, true) {
            Correction::Rate(r, d) => {
                assert!(r < 1.0);
                assert!((d.as_secs_f32() - 10.0).abs() < 0.01); // 300 ms at 2 % = 15 s, capped at 10 s
            }
            other => panic!("{other:?}"),
        }
        match correction(9_900, 10_000, true) {
            Correction::Rate(r, d) => {
                assert!(r > 1.0);
                assert!((d.as_secs_f32() - 5.0).abs() < 0.01);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(correction(11_000, 10_000, true), Correction::Seek(10_000));
        // Without speed control (low-end mode) small drift is fixed with a jump too.
        assert_eq!(correction(10_300, 10_000, false), Correction::Seek(10_000));
    }
}
