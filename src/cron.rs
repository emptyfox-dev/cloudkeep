//! 5 段 cron 表达式解析与「下次执行时间」计算。
//!
//! 格式：`分 时 日 月 周`，支持 `*`、逗号列表、范围 `a-b`、
//! 步进 `*/n` 与 `a-b/n`、以及月/周名称（jan..dec, sun..sat）。

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use chrono_tz::Tz;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
    /// 日字段是否被限定（非 `*`），用于 日/周 的「或」语义
    dom_restricted: bool,
    /// 周字段是否被限定（非 `*`）
    dow_restricted: bool,
    /// 原始表达式，用于日志输出
    pub expression: String,
}

impl CronSchedule {
    /// 解析 5 段 cron 表达式。
    pub fn parse(expression: &str) -> Result<Self> {
        let trimmed = expression.trim();
        let fields: Vec<&str> = trimmed.split_whitespace().collect();

        if fields.len() != 5 {
            bail!(
                "cron 表达式需要 5 段（分 时 日 月 周），实际 {} 段: {trimmed}",
                fields.len()
            );
        }

        let minutes = parse_field(fields[0], 0, 59, &[])?;
        let hours = parse_field(fields[1], 0, 23, &[])?;
        let days = parse_field(fields[2], 1, 31, &[])?;
        let months = parse_field(fields[3], 1, 12, MONTH_NAMES)?;

        // 星期允许 0-7，7 与 0 都表示周日；归一后必须去重，
        // 否则 `*`（0-7）会变成 8 个值并误判为「被限定」。
        let mut weekdays: Vec<u32> = parse_field(fields[4], 0, 7, DAY_NAMES)?
            .into_iter()
            .map(|d| if d == 7 { 0 } else { d })
            .collect();
        weekdays.sort_unstable();
        weekdays.dedup();

        if minutes.is_empty()
            || hours.is_empty()
            || days.is_empty()
            || months.is_empty()
            || weekdays.is_empty()
        {
            bail!("cron 表达式存在空字段: {trimmed}");
        }

        Ok(CronSchedule {
            minutes,
            hours,
            days,
            months,
            weekdays,
            dom_restricted: !is_wildcard(fields[2]),
            dow_restricted: !is_wildcard(fields[4]),
            expression: trimmed.to_string(),
        })
    }

    /// 判断某个本地时间是否命中本表达式。
    pub fn matches(&self, dt: &NaiveDateTime) -> bool {
        self.minutes.contains(&dt.minute())
            && self.hours.contains(&dt.hour())
            && self.months.contains(&dt.month())
            && self.day_matches(&dt.date())
    }

    /// 计算 `after` 之后的下一个执行时间。
    ///
    /// 最多向前搜索 5 年，足以覆盖 `0 0 29 2 *` 这类稀疏表达式。
    pub fn next_after(&self, after: DateTime<Tz>, tz: Tz) -> Option<DateTime<Tz>> {
        let start_naive = after.with_timezone(&tz).naive_local();

        // 严格晚于当前分钟：从「当前分钟 + 1」开始搜索
        let floor = start_naive.with_second(0)?.with_nanosecond(0)?;
        let mut lower = floor.time() + Duration::minutes(1);
        let mut date = floor.date();
        if lower < floor.time() {
            // +1 分钟跨过了午夜，进入次日
            date = date.succ_opt()?;
            lower = midnight();
        }

        let limit = floor.date() + Duration::days(366 * 5 + 1);

        while date <= limit {
            if !self.months.contains(&date.month()) {
                date = advance_month(date)?;
                lower = midnight();
                continue;
            }

            if !self.day_matches(&date) {
                date = date.succ_opt()?;
                lower = midnight();
                continue;
            }

            match self.next_time_of_day(lower) {
                Some(found) => {
                    let candidate = date.and_time(found);
                    // DST 空洞会让本地时间不存在，此时跳过该时刻继续找
                    if let Some(local) = tz.from_local_datetime(&candidate).single() {
                        return Some(local);
                    }
                    let next = found + Duration::minutes(1);
                    if next <= found {
                        date = date.succ_opt()?;
                        lower = midnight();
                    } else {
                        lower = next;
                    }
                }
                None => {
                    date = date.succ_opt()?;
                    lower = midnight();
                }
            }
        }

        None
    }

    /// 在 `[from_time, 24:00)` 内找下一个匹配的 时:分；无解返回 None。
    fn next_time_of_day(&self, from_time: chrono::NaiveTime) -> Option<chrono::NaiveTime> {
        let mut hour = from_time.hour();
        let mut minute = from_time.minute();

        // minutes / hours 已排序，直接取「>=」的第一个候选
        if !self.hours.contains(&hour) {
            hour = *self.hours.iter().find(|h| **h > hour)?;
            minute = 0;
        }

        loop {
            if let Some(&m) = self.minutes.iter().find(|m| **m >= minute) {
                return chrono::NaiveTime::from_hms_opt(hour, m, 0);
            }

            // 当前小时已无匹配分钟，进入下一个匹配小时
            hour = *self.hours.iter().find(|h| **h > hour)?;
            minute = 0;
        }
    }

    /// 日 / 周 的匹配：两者都被限定时取「或」（标准 Vixie cron 语义）。
    fn day_matches(&self, date: &NaiveDate) -> bool {
        let dom = self.days.contains(&date.day());
        let dow = self
            .weekdays
            .contains(&date.weekday().num_days_from_sunday());

        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }
}

fn midnight() -> chrono::NaiveTime {
    chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
}

fn is_wildcard(field: &str) -> bool {
    field.trim() == "*"
}

/// 跳到下个月的 1 号。
fn advance_month(date: NaiveDate) -> Option<NaiveDate> {
    let (year, month) = if date.month() == 12 {
        (date.year() + 1, 1)
    } else {
        (date.year(), date.month() + 1)
    };
    NaiveDate::from_ymd_opt(year, month, 1)
}

const MONTH_NAMES: &[(&str, u32)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const DAY_NAMES: &[(&str, u32)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// 解析单个字段，支持 `*`、`,`、`-`、`/` 与名称别名。
fn parse_field(field: &str, min: u32, max: u32, names: &[(&str, u32)]) -> Result<Vec<u32>> {
    let field = field.trim();
    if field.is_empty() {
        bail!("cron 字段为空（范围 {min}-{max}）");
    }

    let mut out: Vec<u32> = Vec::new();

    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            bail!("cron 字段存在空片段: {field}");
        }

        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("步进值不合法: {part}（应为正整数）"))?;
                if step == 0 {
                    bail!("步进值不能为 0: {part}");
                }
                if r.is_empty() {
                    bail!("步进值缺少区间: {part}");
                }
                (r, Some(step))
            }
            None => (part, None),
        };

        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((lo, hi)) = range_part.split_once('-') {
            (
                resolve_value(lo, min, max, names)?,
                resolve_value(hi, min, max, names)?,
            )
        } else {
            let value = resolve_value(range_part, min, max, names)?;
            match step {
                // `5/15` 表示从 5 开始、每次加 15，直到超出上限
                Some(_) => (value, max),
                None => (value, value),
            }
        };

        if start > end {
            bail!("cron 字段区间起点大于终点: {part}（范围 {min}-{max}）");
        }

        let step = step.unwrap_or(1);
        let mut value = start;
        while value <= end {
            if !out.contains(&value) {
                out.push(value);
            }
            value = match value.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }

    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn resolve_value(token: &str, min: u32, max: u32, names: &[(&str, u32)]) -> Result<u32> {
    let token = token.trim().to_ascii_lowercase();

    if let Some((_, value)) = names.iter().find(|(name, _)| *name == token) {
        return Ok(*value);
    }

    let value: u32 = token.parse().map_err(|_| {
        anyhow::anyhow!("cron 字段值不合法: {token}（应为 {min}-{max} 之间的数字）")
    })?;

    if value < min || value > max {
        bail!("cron 字段值超出范围: {token}（应为 {min}-{max}）");
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> Tz {
        "Asia/Shanghai".parse().unwrap()
    }

    /// `from` 按目标时区的本地挂钟时间解释，便于直接断言本地时间。
    fn next(expr: &str, from: &str) -> String {
        let schedule = CronSchedule::parse(expr).unwrap();
        let naive = chrono::NaiveDateTime::parse_from_str(from, "%Y-%m-%d %H:%M:%S").unwrap();
        let at = tz()
            .from_local_datetime(&naive)
            .single()
            .expect("测试用例的本地时间应唯一存在");
        schedule
            .next_after(at, tz())
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    #[test]
    fn parse_rejects_wrong_field_count() {
        assert!(CronSchedule::parse("0 3 * *").is_err());
        assert!(CronSchedule::parse("0 3 * * * *").is_err());
        assert!(CronSchedule::parse("").is_err());
        assert!(CronSchedule::parse("a b c d e").is_err());
    }

    #[test]
    fn parse_basic_fields() {
        let s = CronSchedule::parse("0 3 * * *").unwrap();
        assert_eq!(s.minutes, vec![0]);
        assert_eq!(s.hours, vec![3]);
        assert_eq!(s.days, (1..=31).collect::<Vec<u32>>());
        assert_eq!(s.weekdays, vec![0, 1, 2, 3, 4, 5, 6], "周日归一后必须去重");
        assert!(!s.dom_restricted);
        assert!(!s.dow_restricted);
    }

    #[test]
    fn parse_step_expressions() {
        let s = CronSchedule::parse("*/15 * * * *").unwrap();
        assert_eq!(s.minutes, vec![0, 15, 30, 45]);

        let s = CronSchedule::parse("0 */6 * * *").unwrap();
        assert_eq!(s.hours, vec![0, 6, 12, 18]);
    }

    #[test]
    fn parse_ranges_and_lists() {
        let s = CronSchedule::parse("0 1-3,5 * * *").unwrap();
        assert_eq!(s.hours, vec![1, 2, 3, 5]);

        let s = CronSchedule::parse("30 2 * * 1").unwrap();
        assert_eq!(s.weekdays, vec![1]);
        assert!(!s.dom_restricted, "日为 * 时未被限定");
        assert!(s.dow_restricted, "周为 1 时被限定");
    }

    #[test]
    fn parse_range_with_step() {
        let s = CronSchedule::parse("0 8-17/4 * * *").unwrap();
        assert_eq!(s.hours, vec![8, 12, 16]);
    }

    #[test]
    fn parse_named_months_and_days() {
        let s = CronSchedule::parse("0 0 1 jan *").unwrap();
        assert_eq!(s.months, vec![1]);

        let s = CronSchedule::parse("0 0 * * mon").unwrap();
        assert_eq!(s.weekdays, vec![1]);

        let s = CronSchedule::parse("0 0 * jan-mar mon-fri").unwrap();
        assert_eq!(s.months, vec![1, 2, 3]);
        assert_eq!(s.weekdays, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_sunday_accepts_0_and_7() {
        assert_eq!(CronSchedule::parse("0 0 * * 0").unwrap().weekdays, vec![0]);
        assert_eq!(CronSchedule::parse("0 0 * * 7").unwrap().weekdays, vec![0]);
        assert_eq!(
            CronSchedule::parse("0 0 * * *").unwrap().weekdays,
            vec![0, 1, 2, 3, 4, 5, 6],
            "`*` 展开到 0-7 后归一去重应剩 7 个值"
        );
    }

    #[test]
    fn parse_rejects_out_of_range() {
        assert!(CronSchedule::parse("60 3 * * *").is_err(), "分钟不能超过 59");
        assert!(CronSchedule::parse("0 24 * * *").is_err(), "小时不能超过 23");
        assert!(CronSchedule::parse("0 3 32 * *").is_err(), "日期不能超过 31");
        assert!(CronSchedule::parse("0 3 * 13 *").is_err(), "月份不能超过 12");
        assert!(CronSchedule::parse("0 3 * * 8").is_err(), "星期不能超过 7");
    }

    #[test]
    fn parse_rejects_bad_syntax() {
        assert!(CronSchedule::parse("*/0 * * * *").is_err(), "步进不能为 0");
        assert!(CronSchedule::parse("5-1 * * * *").is_err(), "区间起点不能大于终点");
        assert!(CronSchedule::parse("1,,2 * * * *").is_err());
        assert!(CronSchedule::parse("1-2-3 * * * *").is_err());
        assert!(CronSchedule::parse("/5 * * * *").is_err(), "缺少区间的步进应报错");
    }

    #[test]
    fn next_daily_three_am() {
        assert_eq!(next("0 3 * * *", "2024-03-05 00:00:00"), "2024-03-05 03:00:00");
        // 正好等于触发时刻时应顺延到下一次，不能重复触发同一分钟
        assert_eq!(next("0 3 * * *", "2024-03-05 03:00:00"), "2024-03-06 03:00:00");
        assert_eq!(next("0 3 * * *", "2024-03-05 03:30:00"), "2024-03-06 03:00:00");
    }

    #[test]
    fn next_late_night_does_not_wrap_to_24() {
        // 回归用例：旧实现在 23 点做 hour+1 会得到 24 并让搜索直接失败
        assert_eq!(next("0 3 * * *", "2024-03-05 23:30:00"), "2024-03-06 03:00:00");
        assert_eq!(next("30 2 * * *", "2024-03-05 23:00:00"), "2024-03-06 02:30:00");
    }

    #[test]
    fn next_every_six_hours() {
        assert_eq!(next("0 */6 * * *", "2024-03-05 01:00:00"), "2024-03-05 06:00:00");
        assert_eq!(next("0 */6 * * *", "2024-03-05 06:00:00"), "2024-03-05 12:00:00");
        assert_eq!(next("0 */6 * * *", "2024-03-05 18:00:00"), "2024-03-06 00:00:00");
        assert_eq!(next("0 */6 * * *", "2024-03-05 20:00:00"), "2024-03-06 00:00:00");
    }

    #[test]
    fn next_weekly_monday() {
        // 2024-03-05 是周二
        assert_eq!(next("30 2 * * 1", "2024-03-05 00:00:00"), "2024-03-11 02:30:00");
        // 周一当天 02:30 之后取下周一
        assert_eq!(next("30 2 * * 1", "2024-03-11 03:00:00"), "2024-03-18 02:30:00");
    }

    #[test]
    fn next_monthly_first_day() {
        assert_eq!(next("0 4 1 * *", "2024-03-05 00:00:00"), "2024-04-01 04:00:00");
        assert_eq!(next("0 4 1 * *", "2024-01-01 04:00:00"), "2024-02-01 04:00:00");
    }

    #[test]
    fn next_february_29_handles_leap_years() {
        assert_eq!(
            next("0 0 29 2 *", "2024-03-01 00:00:00"),
            "2028-02-29 00:00:00",
            "闰日表达式应跳到下一个闰年"
        );
    }

    #[test]
    fn next_crosses_year_boundary() {
        assert_eq!(next("0 0 1 1 *", "2024-06-01 00:00:00"), "2025-01-01 00:00:00");
        assert_eq!(
            next("59 23 31 12 *", "2024-06-01 00:00:00"),
            "2024-12-31 23:59:00"
        );
    }

    #[test]
    fn next_handles_february_end_of_year() {
        assert_eq!(next("0 12 28 2 *", "2024-03-05 00:00:00"), "2025-02-28 12:00:00");
    }

    #[test]
    fn day_and_week_are_ored_when_both_restricted() {
        // `0 5 1 * 1` = 每月 1 号 或 每周一 的 05:00
        assert_eq!(next("0 5 1 * 1", "2024-03-02 00:00:00"), "2024-03-04 05:00:00", "下周一是 3/4");
        assert_eq!(next("0 5 1 * 1", "2024-03-05 00:00:00"), "2024-03-11 05:00:00", "周一早于 4/1");
        // 3/11 周一 06:00 已过：下一个匹配是 3/18 周一（仍早于 4/1）
        assert_eq!(next("0 5 1 * 1", "2024-03-11 06:00:00"), "2024-03-18 05:00:00");
        // 4/1 恰好也是周一，验证两个分支都命中
        assert_eq!(next("0 5 1 * 1", "2024-03-25 06:00:00"), "2024-04-01 05:00:00");
    }

    #[test]
    fn dom_only_restricted_ignores_weekday() {
        // 仅限定日：每月 15 号，不管星期
        assert_eq!(next("0 0 15 * *", "2024-03-16 00:00:00"), "2024-04-15 00:00:00");
    }

    #[test]
    fn every_minute_expression() {
        assert_eq!(next("* * * * *", "2024-03-05 10:00:30"), "2024-03-05 10:01:00");
        assert_eq!(next("* * * * *", "2024-03-05 23:59:10"), "2024-03-06 00:00:00");
    }

    #[test]
    fn next_respects_timezone_offset() {
        // UTC 2024-03-05 23:00 = 上海 2024-03-06 07:00，下一次 08:00 在同一天
        let schedule = CronSchedule::parse("0 8 * * *").unwrap();
        let at = chrono::Utc
            .with_ymd_and_hms(2024, 3, 5, 23, 0, 0)
            .unwrap()
            .with_timezone(&tz());
        let result = schedule.next_after(at, tz()).unwrap();
        assert_eq!(result.format("%Y-%m-%d %H:%M").to_string(), "2024-03-06 08:00");
    }

    #[test]
    fn matches_agrees_with_next() {
        let schedule = CronSchedule::parse("30 2 * * 1").unwrap();

        let monday = chrono::NaiveDate::from_ymd_opt(2024, 3, 11)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert!(schedule.matches(&monday));

        let tuesday = chrono::NaiveDate::from_ymd_opt(2024, 3, 12)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert!(!schedule.matches(&tuesday), "周二不应命中「仅周一」");

        let wrong_minute = chrono::NaiveDate::from_ymd_opt(2024, 3, 11)
            .unwrap()
            .and_hms_opt(2, 31, 0)
            .unwrap();
        assert!(!schedule.matches(&wrong_minute));
    }

    #[test]
    fn next_after_is_monotonic_and_never_returns_input() {
        let schedule = CronSchedule::parse("0 3 * * *").unwrap();
        let at = tz()
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str("2024-03-05 03:00:00", "%Y-%m-%d %H:%M:%S")
                    .unwrap(),
            )
            .single()
            .unwrap();
        let nxt = schedule.next_after(at, tz()).unwrap();
        assert!(nxt > at, "下次执行必须严格晚于当前时刻");
    }

    #[test]
    fn repeated_next_after_walks_the_schedule_forward() {
        // 连续调用应严格按 cron 序列推进
        let schedule = CronSchedule::parse("0 */6 * * *").unwrap();
        let mut at = tz()
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str("2024-03-05 07:00:00", "%Y-%m-%d %H:%M:%S")
                    .unwrap(),
            )
            .single()
            .unwrap();

        let mut seen = Vec::new();
        for _ in 0..5 {
            at = schedule.next_after(at, tz()).unwrap();
            seen.push(at.format("%H:%M").to_string());
        }
        assert_eq!(seen, vec!["12:00", "18:00", "00:00", "06:00", "12:00"]);
    }
}
