//! Calendar dates from Unix time, for log and backup names and shieldd's build date

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 as YYYY-MM-DD, proleptic Gregorian. Howard Hinnant's civil_from_days.
pub fn date(days: u64) -> String {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// YYYY-MM-DDTHH_mm_SS, UTC like the log's lines: what log files and conf backups are named
/// for. Windows forbids `:` in a file name.
pub fn stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    format!("{}T{h:02}_{m:02}_{s:02}", date(secs / 86_400))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stamps_are_utc_and_colonless() {
        let t = UNIX_EPOCH + Duration::from_secs(1_791_554_464);
        assert_eq!(stamp(t), "2026-10-09T14_01_04");
    }
}
