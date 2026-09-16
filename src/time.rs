use std::fmt;
use std::ops::Sub;
use std::time::Duration;

/// An absolute timestamp in the shared `GStreamer` clock domain.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ClockNs(u64);

impl ClockNs {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let nanos = u64::try_from(duration.as_nanos()).ok()?;
        self.0.checked_add(nanos).map(Self)
    }

    #[must_use]
    pub fn abs_diff(self, other: Self) -> Duration {
        Duration::from_nanos(self.0.abs_diff(other.0))
    }
}

impl fmt::Display for ClockNs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A position relative to the start of one local media item.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MediaPositionNs(u64);

impl MediaPositionNs {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for MediaPositionNs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Sub for MediaPositionNs {
    type Output = i128;

    fn sub(self, rhs: Self) -> Self::Output {
        i128::from(self.0) - i128::from(rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_add_rejects_overflow() {
        assert_eq!(
            ClockNs::new(u64::MAX).checked_add(Duration::from_nanos(1)),
            None
        );
    }

    #[test]
    fn clock_difference_is_symmetric() {
        let left = ClockNs::new(10);
        let right = ClockNs::new(25);
        assert_eq!(left.abs_diff(right), Duration::from_nanos(15));
        assert_eq!(right.abs_diff(left), Duration::from_nanos(15));
    }

    #[test]
    fn position_difference_is_signed_without_overflow() {
        assert_eq!(MediaPositionNs::new(2) - MediaPositionNs::new(5), -3);
    }
}
