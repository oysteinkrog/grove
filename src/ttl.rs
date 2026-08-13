//! TTL parsing for ephemeral worktrees (`grove new/fork --ephemeral [--ttl DUR]`).
//!
//! Durations are written as `<positive integer><unit>`, e.g. `14d`, `48h`,
//! `30m`, `2w`, `90s`. There is no daemon evaluating TTLs as they elapse —
//! `expires_at` is an absolute UTC instant recorded at creation time, and
//! callers (e.g. `grove list`, `grove gc`) decide "expired or not" lazily by
//! comparing it to `OffsetDateTime::now_utc()` whenever they happen to run.

use thiserror::Error;
use time::Duration;

/// Default TTL applied to `--ephemeral` worktrees when `--ttl` is omitted.
pub const DEFAULT_TTL_DAYS: i64 = 14;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TtlParseError {
    #[error(
        "invalid TTL '{0}': expected a positive integer followed by a unit (s, m, h, d, w), e.g. '14d'"
    )]
    InvalidFormat(String),
    #[error("invalid TTL '{0}': value must be a positive, non-zero duration")]
    OutOfRange(String),
}

pub fn default_ttl() -> Duration {
    Duration::days(DEFAULT_TTL_DAYS)
}

/// Parse a duration string like `14d`, `48h`, `30m`, `2w`, `90s`.
pub fn parse_ttl(input: &str) -> Result<Duration, TtlParseError> {
    let trimmed = input.trim();
    let Some((num_part, unit)) = split_unit(trimmed) else {
        return Err(TtlParseError::InvalidFormat(input.to_string()));
    };

    let seconds_per_unit: i64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(TtlParseError::InvalidFormat(input.to_string())),
    };

    let count: i64 = num_part
        .parse()
        .map_err(|_| TtlParseError::InvalidFormat(input.to_string()))?;

    if count <= 0 {
        return Err(TtlParseError::OutOfRange(input.to_string()));
    }

    count
        .checked_mul(seconds_per_unit)
        .map(Duration::seconds)
        .ok_or_else(|| TtlParseError::OutOfRange(input.to_string()))
}

/// Split `"14d"` into `("14", "d")`. Returns `None` when there's no trailing
/// ASCII-alphabetic unit or no leading digits.
fn split_unit(s: &str) -> Option<(&str, &str)> {
    if s.is_empty() {
        return None;
    }
    let unit_start = s.rfind(|c: char| c.is_ascii_digit()).map(|i| i + 1)?;
    let (num_part, unit) = s.split_at(unit_start);
    if num_part.is_empty() || unit.is_empty() {
        return None;
    }
    Some((num_part, unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_days() {
        assert_eq!(parse_ttl("14d").unwrap(), Duration::seconds(14 * 86_400));
    }

    #[test]
    fn parses_hours() {
        assert_eq!(parse_ttl("48h").unwrap(), Duration::seconds(48 * 3_600));
    }

    #[test]
    fn parses_minutes() {
        assert_eq!(parse_ttl("30m").unwrap(), Duration::seconds(30 * 60));
    }

    #[test]
    fn parses_weeks() {
        assert_eq!(parse_ttl("2w").unwrap(), Duration::seconds(2 * 604_800));
    }

    #[test]
    fn parses_seconds() {
        assert_eq!(parse_ttl("90s").unwrap(), Duration::seconds(90));
    }

    #[test]
    fn rejects_missing_unit() {
        assert!(matches!(
            parse_ttl("14"),
            Err(TtlParseError::InvalidFormat(_))
        ));
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(matches!(
            parse_ttl("14x"),
            Err(TtlParseError::InvalidFormat(_))
        ));
    }

    #[test]
    fn rejects_zero() {
        assert!(matches!(parse_ttl("0d"), Err(TtlParseError::OutOfRange(_))));
    }

    #[test]
    fn rejects_negative() {
        assert!(matches!(
            parse_ttl("-5d"),
            Err(TtlParseError::OutOfRange(_))
        ));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            parse_ttl(""),
            Err(TtlParseError::InvalidFormat(_))
        ));
    }

    #[test]
    fn rejects_non_numeric_prefix() {
        assert!(matches!(
            parse_ttl("abcd"),
            Err(TtlParseError::InvalidFormat(_))
        ));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            parse_ttl("  14d  ").unwrap(),
            Duration::seconds(14 * 86_400)
        );
    }

    #[test]
    fn default_ttl_is_14_days() {
        assert_eq!(default_ttl(), Duration::seconds(14 * 86_400));
    }
}
