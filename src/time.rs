use anyhow::{bail, Result};
use chrono::{Local, TimeZone, Utc};

use crate::config::DAY_MS;

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn parse_timestamp(input: &str) -> Result<i64> {
    let trimmed = input.trim();

    if trimmed.eq_ignore_ascii_case("now") {
        return Ok(now_ms());
    }

    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Ok(timestamp.timestamp_millis());
    }

    if let Ok(unix) = trimmed.parse::<i64>() {
        return Ok(if trimmed.len() >= 13 { unix } else { unix * 1_000 });
    }

    if let Some(duration) = trimmed.strip_suffix(" ago") {
        return Ok(now_ms() - parse_compound_duration_ms(duration)? as i64);
    }

    bail!(
        "unsupported timestamp `{trimmed}`. Use RFC3339, unix seconds/millis, `now`, or values like `2h ago`."
    );
}

pub fn format_exact(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(timestamp) => timestamp.format("%Y-%m-%d %H:%M:%S%.3f %:z").to_string(),
        None => format!("{ms} ms"),
    }
}

pub fn format_human(ms: i64) -> String {
    let delta = now_ms() - ms;
    let abs_delta = delta.abs();

    let (value, unit) = if abs_delta < 60_000 {
        (abs_delta / 1_000, "second")
    } else if abs_delta < 3_600_000 {
        (abs_delta / 60_000, "minute")
    } else if abs_delta < DAY_MS {
        (abs_delta / 3_600_000, "hour")
    } else {
        (abs_delta / DAY_MS, "day")
    };

    let noun = if value == 1 {
        unit.to_string()
    } else {
        format!("{unit}s")
    };

    if delta >= 0 {
        format!("{value} {noun} ago")
    } else {
        format!("in {value} {noun}")
    }
}

pub fn format_exact_and_human(ms: i64) -> String {
    format!("{} ({})", format_exact(ms), format_human(ms))
}

fn parse_compound_duration_ms(input: &str) -> Result<u64> {
    let cleaned = input.trim().replace(' ', "");
    if cleaned.is_empty() {
        bail!("empty relative duration");
    }

    let mut total = 0_u64;
    let mut digits = String::new();

    for character in cleaned.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }

        if digits.is_empty() {
            bail!("invalid relative duration `{input}`");
        }

        let value = digits.parse::<u64>()?;
        digits.clear();

        let multiplier = match character {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => DAY_MS as u64,
            'w' => 7 * DAY_MS as u64,
            _ => bail!("unsupported duration unit `{character}` in `{input}`"),
        };

        total = total.saturating_add(value.saturating_mul(multiplier));
    }

    if !digits.is_empty() {
        bail!("missing duration unit in `{input}`");
    }

    Ok(total)
}
