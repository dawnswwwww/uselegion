//! `/loop` command: schedule a recurring prompt as a cron job.
//!
//! Syntax: `/loop [interval] <prompt>` where interval is `Ns`, `Nm`, `Nh`,
//! `Nd`, or a natural-language expression such as `every 2 minutes`,
//! `每2分钟`, `每隔5分钟一次`. Defaults to 10 minutes. The prompt is executed
//! immediately and then on every cron fire.

use regex::Regex;
use std::sync::LazyLock;
use thiserror::Error;

/// Default interval when the user does not specify one.
pub const DEFAULT_INTERVAL: &str = "10m";

/// Parsed `/loop` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopRequest {
    /// Raw interval string (e.g. "5m", "2h").
    pub interval: String,
    /// Prompt or slash command to run on each fire.
    pub prompt: String,
}

/// Errors parsing a `/loop` command.
#[derive(Debug, Error, PartialEq)]
pub enum LoopParseError {
    #[error("missing prompt; usage: /loop [interval] <prompt>")]
    MissingPrompt,
    #[error("invalid interval '{0}'; expected Ns, Nm, Nh, or Nd (e.g. 5m, 2h)")]
    InvalidInterval(String),
    #[error("interval must be at least 1 minute")]
    TooShort,
}

/// Parse the argument string of a `/loop` command.
///
/// Rules (in priority order):
/// 1. Leading token matching `^\d+[smhd]$` is the interval; rest is prompt.
/// 2. Trailing `every <N><unit>` or `every <N> <unit-word>` is the interval.
/// 3. Natural-language interval in English or Chinese anywhere in the input
///    (e.g. `every 2 minutes`, `每2分钟`, `每隔5分钟一次`).
/// 4. Otherwise default to [`DEFAULT_INTERVAL`] and treat all input as prompt.
pub fn parse_loop(args: &str) -> Result<LoopRequest, LoopParseError> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err(LoopParseError::MissingPrompt);
    }

    // Rule 1: leading interval token.
    if let Some(idx) = trimmed.find(char::is_whitespace) {
        let first = &trimmed[..idx];
        if is_interval_token(first) {
            let prompt = normalize_prompt(&trimmed[idx..]);
            if prompt.is_empty() {
                return Err(LoopParseError::MissingPrompt);
            }
            return Ok(LoopRequest {
                interval: normalize_interval(first)?,
                prompt,
            });
        }
    } else if is_interval_token(trimmed) {
        return Err(LoopParseError::MissingPrompt);
    }

    // Rule 2: trailing "every ..." clause.
    if let Some((interval, prompt)) = extract_trailing_every(trimmed) {
        let prompt = normalize_prompt(&prompt);
        if prompt.is_empty() {
            return Err(LoopParseError::MissingPrompt);
        }
        return Ok(LoopRequest { interval, prompt });
    }

    // Rule 3: natural-language interval (English/Chinese).
    if let Some((interval, prompt)) = extract_natural_language_interval(trimmed) {
        let prompt = normalize_prompt(&prompt);
        if prompt.is_empty() {
            return Err(LoopParseError::MissingPrompt);
        }
        return Ok(LoopRequest { interval, prompt });
    }

    // Rule 4: default interval.
    Ok(LoopRequest {
        interval: DEFAULT_INTERVAL.to_string(),
        prompt: trimmed.to_string(),
    })
}

/// Collapse whitespace and trim a prompt so removing an interval clause does
/// not leave awkward multiple spaces.
fn normalize_prompt(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Convert an interval string to a 5-field cron expression in local time.
///
/// Supported patterns:
/// - `Nm` where N <= 59  -> `*/N * * * *`
/// - `Nm` where N >= 60  -> `0 */H * * *` (H = N/60, must divide 24)
/// - `Nh` where N <= 23  -> `0 */N * * *`
/// - `Nd`                -> `0 0 */N * *`
/// - `Ns`                -> rounded up to minutes
pub fn interval_to_cron(interval: &str) -> Result<String, LoopParseError> {
    let normalized = normalize_interval(interval)?;
    let (value, unit) = parse_interval(&normalized)?;

    match unit {
        's' => {
            let minutes = value.div_ceil(60).max(1);
            interval_to_cron(&format!("{minutes}m"))
        }
        'm' => {
            if value <= 59 {
                Ok(format!("*/{value} * * * *"))
            } else {
                let hours = value / 60;
                if value % 60 != 0 || hours == 0 || 24 % hours != 0 {
                    return Err(LoopParseError::InvalidInterval(format!(
                        "{value}m does not divide cleanly into hours; try {hours}h or {}h",
                        hours + 1
                    )));
                }
                Ok(format!("0 */{hours} * * *"))
            }
        }
        'h' => {
            if value == 0 || value > 23 || 24 % value != 0 {
                return Err(LoopParseError::InvalidInterval(format!(
                    "{value}h must be 1-23 and divide 24"
                )));
            }
            Ok(format!("0 */{value} * * *"))
        }
        'd' => Ok(format!("0 0 */{value} * *")),
        _ => Err(LoopParseError::InvalidInterval(interval.to_string())),
    }
}

/// Human-readable description of a cron expression produced by this module.
pub fn cron_human_summary(cron: &str) -> String {
    match cron {
        "*/1 * * * *" => "every minute".to_string(),
        "*/2 * * * *" => "every 2 minutes".to_string(),
        "*/5 * * * *" => "every 5 minutes".to_string(),
        "*/10 * * * *" => "every 10 minutes".to_string(),
        "*/15 * * * *" => "every 15 minutes".to_string(),
        "*/30 * * * *" => "every 30 minutes".to_string(),
        "0 */1 * * *" => "every hour".to_string(),
        "0 */2 * * *" => "every 2 hours".to_string(),
        "0 */6 * * *" => "every 6 hours".to_string(),
        "0 */12 * * *" => "every 12 hours".to_string(),
        "0 0 */1 * *" => "every day".to_string(),
        _ => cron.to_string(),
    }
}

fn is_interval_token(token: &str) -> bool {
    parse_interval(token).is_ok()
}

fn normalize_interval(interval: &str) -> Result<String, LoopParseError> {
    let interval = interval.trim().to_lowercase();
    if interval.is_empty() {
        return Err(LoopParseError::InvalidInterval(interval));
    }
    // Accept "10 m" / "2 h" style with a space.
    if interval.contains(' ') {
        let parts: Vec<&str> = interval.split_whitespace().collect();
        if parts.len() == 2 {
            let unit = match parts[1] {
                "second" | "seconds" | "sec" | "secs" | "s" => 's',
                "minute" | "minutes" | "min" | "mins" | "m" => 'm',
                "hour" | "hours" | "hr" | "hrs" | "h" => 'h',
                "day" | "days" | "d" => 'd',
                _ => return Err(LoopParseError::InvalidInterval(interval)),
            };
            let value: u32 = parts[0]
                .parse()
                .map_err(|_| LoopParseError::InvalidInterval(interval.clone()))?;
            return Ok(format!("{value}{unit}"));
        }
    }
    // Validate the compact form.
    let _ = parse_interval(&interval)?;
    Ok(interval)
}

fn parse_interval(interval: &str) -> Result<(u32, char), LoopParseError> {
    let interval = interval.trim().to_lowercase();
    if interval.is_empty() {
        return Err(LoopParseError::InvalidInterval(interval));
    }
    let unit = interval
        .chars()
        .last()
        .ok_or_else(|| LoopParseError::InvalidInterval(interval.clone()))?;
    if !matches!(unit, 's' | 'm' | 'h' | 'd') {
        return Err(LoopParseError::InvalidInterval(interval));
    }
    let value_str = &interval[..interval.len() - 1];
    let value: u32 = value_str
        .parse()
        .map_err(|_| LoopParseError::InvalidInterval(interval.clone()))?;
    if value == 0 {
        return Err(LoopParseError::InvalidInterval(interval));
    }
    Ok((value, unit))
}

fn extract_trailing_every(input: &str) -> Option<(String, String)> {
    // Match "... every <N><unit>" or "... every <N> <unit-word>" at the end.
    let lower = input.to_lowercase();
    if let Some(pos) = lower.rfind(" every ") {
        let after = &input[pos + 7..];
        let after_trimmed = after.trim();
        if after_trimmed.is_empty() {
            return None;
        }
        // Try compact token first.
        let parts: Vec<&str> = after_trimmed.split_whitespace().collect();
        if parts.len() == 1 {
            if is_interval_token(parts[0]) {
                let interval = normalize_interval(parts[0]).ok()?;
                let prompt = input[..pos].to_string();
                return Some((interval, prompt));
            }
        } else if parts.len() == 2 {
            let candidate = format!("{} {}", parts[0], parts[1]);
            if normalize_interval(&candidate).is_ok() {
                let interval = normalize_interval(&candidate).ok()?;
                let prompt = input[..pos].to_string();
                return Some((interval, prompt));
            }
        }
    }
    None
}

/// Extract a natural-language interval and the remaining prompt from input.
/// Supports both English ("every 2 minutes") and Chinese ("每2分钟",
/// "每隔5分钟一次") expressions. Duration phrases like "持续10分钟" are
/// stripped from the prompt before interval extraction so they are not
/// mistaken for the loop interval.
fn extract_natural_language_interval(input: &str) -> Option<(String, String)> {
    // Strip duration phrases such as "持续10分钟" / "持续十分钟" so they are
    // not confused with the loop interval.
    let without_duration = strip_duration(input);

    // Try Chinese first: 每2分钟 / 每隔2分钟 / 2分钟一次 / 每两分钟.
    if let Some(result) = extract_chinese_interval(&without_duration) {
        return Some(result);
    }

    // Try English "every" anywhere in the string (not just trailing).
    extract_english_interval(&without_duration)
}

static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:持续|for)\s*(\d+|[一二两三四五六七八九十]+)\s*(秒|秒钟|分钟|分|小时|时|钟头|天|日|seconds?|minutes?|hours?|days?)"
    )
    .expect("duration strip regex compiles")
});

/// Remove duration phrases like "持续10分钟" / "for 10 minutes" from the
/// input. These describe how long the loop should run, not how often.
fn strip_duration(input: &str) -> String {
    DURATION_RE.replace_all(input, " ").to_string()
}

static CHINESE_INTERVAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:每\s*(?:隔\s*)?)?(\d+|[一二两三四五六七八九十]+)\s*(秒|秒钟|分钟|分|小时|时|钟头|天|日)(?:\s*一次)?"
    )
    .expect("chinese interval regex compiles")
});

/// Extract Chinese-style interval expressions.
fn extract_chinese_interval(input: &str) -> Option<(String, String)> {
    let caps = CHINESE_INTERVAL_RE.captures(input)?;
    let full_match = caps.get(0)?;
    let num_str = caps.get(1)?.as_str();
    let unit_str = caps.get(2)?.as_str();

    let value = parse_chinese_number(num_str)?;
    let unit = normalize_chinese_unit(unit_str)?;

    let interval = format!("{value}{unit}");
    let prompt = input[..full_match.start()].to_string() + " " + &input[full_match.end()..];
    Some((interval, prompt))
}

static ENGLISH_INTERVAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)every\s*(\d+)\s*(seconds?|minutes?|hours?|days?|secs?|mins?|hrs?|s|m|h|d)")
        .expect("english interval regex compiles")
});

/// Extract English-style interval expressions anywhere in the input.
fn extract_english_interval(input: &str) -> Option<(String, String)> {
    let caps = ENGLISH_INTERVAL_RE.captures(input)?;
    let full_match = caps.get(0)?;
    let num_str = caps.get(1)?.as_str();
    let unit_str = caps.get(2)?.as_str();

    let value: u32 = num_str.parse().ok()?;
    let unit = normalize_english_unit(unit_str)?;

    let interval = format!("{value}{unit}");
    let prompt = input[..full_match.start()].to_string() + " " + &input[full_match.end()..];
    Some((interval, prompt))
}

/// Parse a string that may be an Arabic numeral or a simple Chinese number.
fn parse_chinese_number(s: &str) -> Option<u32> {
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }

    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return None;
    }

    // Simple Chinese number parser for 1-99.
    let digit_value = |c: char| -> Option<u32> {
        match c {
            '一' | '壹' => Some(1),
            '二' | '两' | '贰' => Some(2),
            '三' | '叁' => Some(3),
            '四' | '肆' => Some(4),
            '五' | '伍' => Some(5),
            '六' | '陆' => Some(6),
            '七' | '柒' => Some(7),
            '八' | '捌' => Some(8),
            '九' | '玖' => Some(9),
            _ => None,
        }
    };

    if chars.len() == 1 {
        if chars[0] == '十' || chars[0] == '拾' {
            return Some(10);
        }
        return digit_value(chars[0]);
    }

    // Two characters: "十一" -> 11, "二十" -> 20, "二十三" -> 23.
    if chars.len() == 2 {
        if chars[0] == '十' || chars[0] == '拾' {
            // 十一..十九
            let ones = digit_value(chars[1])?;
            return Some(10 + ones);
        }
        if chars[1] == '十' || chars[1] == '拾' {
            // 二十..九十
            let tens = digit_value(chars[0])?;
            return Some(tens * 10);
        }
    }

    if chars.len() == 3 && (chars[1] == '十' || chars[1] == '拾') {
        let tens = digit_value(chars[0])?;
        let ones = digit_value(chars[2])?;
        return Some(tens * 10 + ones);
    }

    None
}

fn normalize_chinese_unit(s: &str) -> Option<char> {
    match s {
        "秒" | "秒钟" => Some('s'),
        "分钟" | "分" => Some('m'),
        "小时" | "时" | "钟头" => Some('h'),
        "天" | "日" => Some('d'),
        _ => None,
    }
}

fn normalize_english_unit(s: &str) -> Option<char> {
    match s.to_lowercase().as_str() {
        "second" | "seconds" | "sec" | "secs" | "s" => Some('s'),
        "minute" | "minutes" | "min" | "mins" | "m" => Some('m'),
        "hour" | "hours" | "hr" | "hrs" | "h" => Some('h'),
        "day" | "days" | "d" => Some('d'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_leading_interval() {
        let req = parse_loop("5m check the deploy").unwrap();
        assert_eq!(req.interval, "5m");
        assert_eq!(req.prompt, "check the deploy");
    }

    #[test]
    fn parse_trailing_every_minutes() {
        let req = parse_loop("check the deploy every 20m").unwrap();
        assert_eq!(req.interval, "20m");
        assert_eq!(req.prompt, "check the deploy");
    }

    #[test]
    fn parse_trailing_every_word_form() {
        let req = parse_loop("run tests every 5 minutes").unwrap();
        assert_eq!(req.interval, "5m");
        assert_eq!(req.prompt, "run tests");
    }

    #[test]
    fn parse_chinese_every_minutes() {
        let req = parse_loop("每2分钟搜索最新热点新闻").unwrap();
        assert_eq!(req.interval, "2m");
        assert_eq!(req.prompt, "搜索最新热点新闻");
    }

    #[test]
    fn parse_chinese_every_ge_minutes() {
        let req = parse_loop("每隔5分钟一次 提醒我喝水").unwrap();
        assert_eq!(req.interval, "5m");
        assert_eq!(req.prompt, "提醒我喝水");
    }

    #[test]
    fn parse_chinese_with_duration_stripped() {
        let req = parse_loop("每2分钟搜索最新热点新闻 持续10分钟").unwrap();
        assert_eq!(req.interval, "2m");
        assert_eq!(req.prompt, "搜索最新热点新闻");
    }

    #[test]
    fn parse_chinese_number_minutes() {
        let req = parse_loop("每两分钟搜索一次新闻").unwrap();
        assert_eq!(req.interval, "2m");
        assert_eq!(req.prompt, "搜索一次新闻");
    }

    #[test]
    fn parse_chinese_hours() {
        let req = parse_loop("每隔1小时检查邮件").unwrap();
        assert_eq!(req.interval, "1h");
        assert_eq!(req.prompt, "检查邮件");
    }

    #[test]
    fn parse_chinese_two_hours_word_form() {
        let req = parse_loop("每两小时检查邮件").unwrap();
        assert_eq!(req.interval, "2h");
        assert_eq!(req.prompt, "检查邮件");
    }

    #[test]
    fn parse_uppercase_unit() {
        let req = parse_loop("5M check").unwrap();
        assert_eq!(req.interval, "5m");
        assert_eq!(req.prompt, "check");
    }

    #[test]
    fn parse_english_everywhere_in_input() {
        let req = parse_loop("search news every 2 minutes and summarize").unwrap();
        assert_eq!(req.interval, "2m");
        assert_eq!(req.prompt, "search news and summarize");
    }

    #[test]
    fn parse_default_interval() {
        let req = parse_loop("check the deploy").unwrap();
        assert_eq!(req.interval, DEFAULT_INTERVAL);
        assert_eq!(req.prompt, "check the deploy");
    }

    #[test]
    fn parse_every_without_time_keeps_default() {
        // "check every PR" has "every" but not followed by a time expression.
        let req = parse_loop("check every PR").unwrap();
        assert_eq!(req.interval, DEFAULT_INTERVAL);
        assert_eq!(req.prompt, "check every PR");
    }

    #[test]
    fn parse_missing_prompt_errors() {
        assert_eq!(parse_loop(""), Err(LoopParseError::MissingPrompt));
        assert_eq!(parse_loop("5m"), Err(LoopParseError::MissingPrompt));
    }

    #[test]
    fn parse_interval_only_errors_as_missing_prompt() {
        // A valid-looking interval with no prompt is treated as missing prompt,
        // not as a prompt itself.
        assert_eq!(parse_loop("5m"), Err(LoopParseError::MissingPrompt));
    }

    #[test]
    fn interval_to_cron_rejects_invalid_value_zero() {
        assert!(matches!(
            interval_to_cron("0m"),
            Err(LoopParseError::InvalidInterval(_))
        ));
    }

    #[test]
    fn interval_to_cron_minutes() {
        assert_eq!(interval_to_cron("5m").unwrap(), "*/5 * * * *");
        assert_eq!(interval_to_cron("1m").unwrap(), "*/1 * * * *");
        assert_eq!(interval_to_cron("2m").unwrap(), "*/2 * * * *");
    }

    #[test]
    fn interval_to_cron_hours() {
        assert_eq!(interval_to_cron("2h").unwrap(), "0 */2 * * *");
        assert_eq!(interval_to_cron("6h").unwrap(), "0 */6 * * *");
    }

    #[test]
    fn interval_to_cron_days() {
        assert_eq!(interval_to_cron("1d").unwrap(), "0 0 */1 * *");
        assert_eq!(interval_to_cron("3d").unwrap(), "0 0 */3 * *");
    }

    #[test]
    fn interval_to_cron_seconds_rounds_up() {
        assert_eq!(interval_to_cron("30s").unwrap(), "*/1 * * * *");
        assert_eq!(interval_to_cron("90s").unwrap(), "*/2 * * * *");
        assert_eq!(interval_to_cron("120s").unwrap(), "*/2 * * * *");
    }

    #[test]
    fn interval_to_cron_rejects_unclean_hour_fractions() {
        assert!(matches!(
            interval_to_cron("90m"),
            Err(LoopParseError::InvalidInterval(_))
        ));
    }

    #[test]
    fn interval_to_cron_rejects_non_dividing_hours() {
        assert!(matches!(
            interval_to_cron("5h"),
            Err(LoopParseError::InvalidInterval(_))
        ));
    }

    #[test]
    fn cron_human_summary_smoke() {
        assert_eq!(cron_human_summary("*/2 * * * *"), "every 2 minutes");
        assert_eq!(cron_human_summary("*/5 * * * *"), "every 5 minutes");
        assert_eq!(cron_human_summary("0 */2 * * *"), "every 2 hours");
        assert_eq!(cron_human_summary("0 0 */1 * *"), "every day");
        assert_eq!(cron_human_summary("*/7 * * * *"), "*/7 * * * *");
    }
}
