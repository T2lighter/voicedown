//! VoiceDown - 时间工具模块
//!
//! 文件名时间戳生成（`capture_<YYYYMMDD_HHMMSS>.wav`）。`timestamp_string` 取当前
//! UTC+8 时间组装；`days_since_epoch_to_date` + `is_leap_year` 是其手写日历算术辅助
//!（无 chrono 依赖，纯函数可单测）。A1 候选自 lib.rs「工具函数」节迁出。

/// 当前 UTC+8 时间 → "YYYYMMDD_HHMMSS"（用于 capture 文件名）。
///
/// 注：UTC+8 偏移硬编码（产品定位中国时区）。依赖 `SystemTime` 非纯，核心日历逻辑
/// 在 `days_since_epoch_to_date`（纯函数，单测覆盖闰年/闰二月/月边界）。
pub(crate) fn timestamp_string() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let secs = now.as_secs();
    let total_secs = secs + 8 * 3600;
    let days = total_secs / 86400;
    let remaining = total_secs % 86400;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;
    let (year, month, day) = days_since_epoch_to_date(days);
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        year, month, day, hours, minutes, seconds
    )
}

/// UNIX epoch（1970-01-01）后的天数 → `(year, month, day)`，month/day 均 1-based。
///
/// 逐年减 `days_in_year`、再逐月减 `month_days`。纯函数（无 I/O、无全局状态）。
pub(crate) fn days_since_epoch_to_date(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970u64;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let month_days = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u64;
    for &md in month_days.iter() {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

/// 标准格里高利闰年：÷4 且非 ÷100，或 ÷400。
pub(crate) fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_leap_year_standard_rules() {
        assert!(is_leap_year(2000)); // ÷400 闰
        assert!(!is_leap_year(1900)); // ÷100 非 ÷400 平
        assert!(is_leap_year(2024)); // ÷4 非 ÷100 闰
        assert!(!is_leap_year(2023)); // 平
        assert!(is_leap_year(1972)); // epoch 后首个闰
        assert!(!is_leap_year(1970)); // epoch 年平
    }

    #[test]
    fn days_since_epoch_to_date_at_epoch() {
        // UNIX epoch 第 0 天 = 1970-01-01
        assert_eq!(days_since_epoch_to_date(0), (1970, 1, 1));
    }

    #[test]
    fn days_since_epoch_to_date_year_rollover() {
        // 1970 平(365) → 1971-01-01
        assert_eq!(days_since_epoch_to_date(365), (1971, 1, 1));
        // +1971 平(365) = 730 → 1972-01-01（1972 闰）
        assert_eq!(days_since_epoch_to_date(730), (1972, 1, 1));
        // +1972 闰(366) = 1096 → 1973-01-01
        assert_eq!(days_since_epoch_to_date(1096), (1973, 1, 1));
    }

    #[test]
    fn days_since_epoch_to_date_leap_february_29() {
        // 1972-02-29（闰年二月二十九存在）：730(→1972-01-01) + 31(jan) + 28 = 789 → 02-29
        assert_eq!(days_since_epoch_to_date(789), (1972, 2, 29));
    }

    #[test]
    fn days_since_epoch_to_date_month_boundaries() {
        // 1970 平年月边界：+31→02-01, +28→03-01, +31→04-01
        assert_eq!(days_since_epoch_to_date(31), (1970, 2, 1));
        assert_eq!(days_since_epoch_to_date(59), (1970, 3, 1)); // 31+28
        assert_eq!(days_since_epoch_to_date(90), (1970, 4, 1)); // +31
    }

    #[test]
    fn timestamp_string_format() {
        // 结构：YYYYMMDD_HHMMSS = 15 字符，第 9 位是 '_'，前缀合法年份
        let ts = timestamp_string();
        assert_eq!(ts.len(), 15);
        assert_eq!(ts.chars().nth(8), Some('_'));
        let year: u32 = ts[..4].parse().expect("前 4 位应为数字年份");
        assert!(year >= 2024);
    }
}
