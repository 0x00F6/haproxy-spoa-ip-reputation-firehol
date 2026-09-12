//! Allocation-free `Display` helpers for log messages.

use chrono::DateTime;
use std::fmt::{self, Write};

/// Integer with thousands separated by spaces, e.g. `1 234 567`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanCount(pub u64);

impl From<usize> for HumanCount {
    fn from(value: usize) -> Self {
        Self(value as u64)
    }
}

impl From<u32> for HumanCount {
    fn from(value: u32) -> Self {
        Self(u64::from(value))
    }
}

impl fmt::Display for HumanCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // u64::MAX has 20 digits.
        let mut digits = [0u8; 20];
        let mut value = self.0;
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let digits = &digits[start..];
        for (index, digit) in digits.iter().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                f.write_char(' ')?;
            }
            f.write_char(char::from(*digit))?;
        }
        Ok(())
    }
}

/// Byte count with a binary unit, e.g. `116.27 MB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanSize(pub u64);

impl fmt::Display for HumanSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut size = self.0 as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < UNITS.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        write!(f, "{size:.2} {}", UNITS[unit])
    }
}

/// Unix timestamp rendered as `YYYY-MM-DD HH:MM:SS` (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochSeconds(pub i64);

impl From<u64> for EpochSeconds {
    fn from(value: u64) -> Self {
        Self(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl fmt::Display for EpochSeconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match DateTime::from_timestamp(self.0, 0) {
            Some(datetime) => write!(f, "{}", datetime.format("%Y-%m-%d %H:%M:%S")),
            None => write!(f, "invalid epoch {}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_count_groups_digits_by_three() {
        assert_eq!(HumanCount(0).to_string(), "0");
        assert_eq!(HumanCount(999).to_string(), "999");
        assert_eq!(HumanCount(1_000).to_string(), "1 000");
        assert_eq!(HumanCount(1_234_567).to_string(), "1 234 567");
        assert_eq!(HumanCount::from(42usize).to_string(), "42");
        assert_eq!(
            HumanCount(u64::MAX).to_string(),
            "18 446 744 073 709 551 615"
        );
    }

    #[test]
    fn human_size_picks_the_largest_unit() {
        assert_eq!(HumanSize(0).to_string(), "0.00 B");
        assert_eq!(HumanSize(1023).to_string(), "1023.00 B");
        assert_eq!(HumanSize(1024).to_string(), "1.00 KB");
        assert_eq!(HumanSize(121_929_625).to_string(), "116.28 MB");
        assert_eq!(HumanSize(u64::MAX).to_string(), "16777216.00 TB");
    }

    #[test]
    fn epoch_seconds_formats_utc_or_reports_invalid() {
        assert_eq!(EpochSeconds(0).to_string(), "1970-01-01 00:00:00");
        assert_eq!(
            EpochSeconds(1_700_000_000).to_string(),
            "2023-11-14 22:13:20"
        );
        assert_eq!(
            EpochSeconds(i64::MAX).to_string(),
            format!("invalid epoch {}", i64::MAX)
        );
        assert_eq!(EpochSeconds::from(u64::MAX), EpochSeconds(i64::MAX));
    }
}
