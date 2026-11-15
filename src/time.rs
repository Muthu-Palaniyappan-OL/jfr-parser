//! Tick and unit conversion.

use crate::metadata::Unit;

/// Converts the tick counters stored in events to wall-clock time.
#[derive(Debug, Clone, Copy)]
pub struct TimeConverter {
    /// Wall-clock time (ns since the Unix epoch) at `start_ticks`.
    pub start_nanos: i64,
    /// Tick counter value corresponding to `start_nanos`.
    pub start_ticks: i64,
    /// Tick counter frequency.
    pub ticks_per_second: i64,
}

impl TimeConverter {
    /// Creates a converter. A non-positive `ticks_per_second` is treated as
    /// one tick per nanosecond.
    pub fn new(start_nanos: i64, start_ticks: i64, ticks_per_second: i64) -> Self {
        let tps = if ticks_per_second > 0 { ticks_per_second } else { 1_000_000_000 };
        TimeConverter { start_nanos, start_ticks, ticks_per_second: tps }
    }

    /// Absolute tick value to nanoseconds since the Unix epoch.
    #[inline]
    pub fn ticks_to_epoch_nanos(&self, ticks: i64) -> i64 {
        self.start_nanos.wrapping_add(self.duration_nanos(ticks.wrapping_sub(self.start_ticks)))
    }

    /// A tick *count* (a duration) to nanoseconds.
    #[inline]
    pub fn duration_nanos(&self, ticks: i64) -> i64 {
        if self.ticks_per_second == 1_000_000_000 {
            ticks
        } else {
            // i128 keeps the multiplication exact for any recording length.
            (ticks as i128 * 1_000_000_000 / self.ticks_per_second as i128) as i64
        }
    }

    /// Converts a raw numeric field to a canonical quantity based on its unit:
    /// durations become nanoseconds, timestamps become epoch nanoseconds,
    /// everything else is returned unchanged.
    #[inline]
    pub fn normalize(&self, unit: Unit, raw: i64) -> i64 {
        match unit {
            Unit::TimespanTicks => self.duration_nanos(raw),
            Unit::TimespanNanos => raw,
            Unit::TimespanMicros => raw.saturating_mul(1_000),
            Unit::TimespanMillis => raw.saturating_mul(1_000_000),
            Unit::TimespanSeconds => raw.saturating_mul(1_000_000_000),
            Unit::TimestampTicks => self.ticks_to_epoch_nanos(raw),
            Unit::TimestampEpochMillis => raw.saturating_mul(1_000_000),
            Unit::TimestampEpochNanos => raw,
            _ => raw,
        }
    }
}

impl Unit {
    /// True when [`TimeConverter::normalize`] yields nanoseconds for this unit.
    pub fn is_time(self) -> bool {
        matches!(
            self,
            Unit::TimespanTicks
                | Unit::TimespanNanos
                | Unit::TimespanMicros
                | Unit::TimespanMillis
                | Unit::TimespanSeconds
                | Unit::TimestampTicks
                | Unit::TimestampEpochMillis
                | Unit::TimestampEpochNanos
        )
    }
}
