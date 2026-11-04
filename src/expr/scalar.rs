// SPDX-License-Identifier: AGPL-3.0-only
//! MySQL scalar function evaluation (CONCAT, UPPER, ROUND, NOW,
//! DATE_FORMAT, ...) and the datetime helper suite backing them.
//! Split out of the former expr.rs monolith — no behaviour change.

use super::eval::{fn_as_f64, fn_to_text, is_truthy};
use crate::relational::Value;

/// Public wrapper for the SQL→DnaQL translator: evaluate a function over
/// already-computed argument values (constant folding path).
pub fn eval_for_translator(name: &str, vals: &[Option<Value>]) -> Option<Value> {
    eval_scalar_fn(name, vals)
}

/// MySQL scalar function evaluation (CMS edition). NULL semantics follow
/// the server: most functions return NULL on any NULL input; COALESCE /
/// IFNULL / CONCAT_WS / IF have their documented exceptions.
#[allow(clippy::too_many_lines)]
pub(crate) fn eval_scalar_fn(name: &str, vals: &[Option<Value>]) -> Option<Value> {
    // eval() yields Option<Value> where BOTH None (no value) and
    // Some(Value::Null) mean SQL NULL here — function NULL semantics must
    // treat them identically (CONCAT('a', NULL) → NULL etc.).
    let any_null = vals
        .iter()
        .any(|v| matches!(v, None | Some(Value::Null)));
    let sql_non_null = |v: &Option<Value>| !matches!(v, None | Some(Value::Null));
    let txt = |i: usize| vals.get(i).cloned().flatten().map(|v| fn_to_text(&v));
    let num = |i: usize| vals.get(i).cloned().flatten().and_then(|v| fn_as_f64(&v));
    let int = |i: usize| match vals.get(i).cloned().flatten() {
        Some(Value::Int(i)) => Some(i),
        Some(v) => fn_as_f64(&v).map(|f| f as i64),
        None => None,
    };
    match name {
        "CONCAT" => {
            if any_null {
                return Some(Value::Null);
            }
            Some(Value::Text(
                (0..vals.len()).filter_map(txt).collect::<Vec<_>>().join(""),
            ))
        }
        "CONCAT_WS" => {
            // CONCAT_WS(NULL, ...) → NULL; NULL arguments are skipped.
            let sep = txt(0)?;
            let parts: Vec<String> = (1..vals.len())
                .filter_map(|i| vals.get(i).cloned().flatten())
                .filter(|v| !matches!(v, Value::Null))
                .map(|v| fn_to_text(&v))
                .collect();
            Some(Value::Text(parts.join(&sep)))
        }
        "UPPER" | "UCASE" => txt(0).map(|s| Value::Text(s.to_uppercase())),
        "LOWER" | "LCASE" => txt(0).map(|s| Value::Text(s.to_lowercase())),
        "LENGTH" => txt(0).map(|s| Value::Int(s.len() as i64)),
        "CHAR_LENGTH" | "CHARACTER_LENGTH" => {
            txt(0).map(|s| Value::Int(s.chars().count() as i64))
        }
        "TRIM" => txt(0).map(|s| Value::Text(s.trim().to_string())),
        "LTRIM" => txt(0).map(|s| Value::Text(s.trim_start().to_string())),
        "RTRIM" => txt(0).map(|s| Value::Text(s.trim_end().to_string())),
        "SUBSTRING" | "SUBSTR" | "MID" => {
            let s = txt(0)?;
            let chars: Vec<char> = s.chars().collect();
            let pos = int(1)?;
            // MySQL 1-based; negative pos counts from the end; 0 → ''.
            let start = if pos > 0 {
                (pos - 1) as usize
            } else if pos < 0 {
                let back = (-pos) as usize;
                if back >= chars.len() {
                    return Some(Value::Text(String::new()));
                }
                chars.len() - back
            } else {
                return Some(Value::Text(String::new()));
            };
            let end = match int(2) {
                Some(len) if len <= 0 => start,
                Some(len) => (start + len as usize).min(chars.len()),
                None => chars.len(),
            };
            if start >= chars.len() || start >= end {
                return Some(Value::Text(String::new()));
            }
            Some(Value::Text(chars[start..end].iter().collect()))
        }
        "REPLACE" => {
            let (s, from, to) = (txt(0)?, txt(1)?, txt(2)?);
            if from.is_empty() {
                return Some(Value::Text(s));
            }
            Some(Value::Text(s.replace(&from, &to)))
        }
        "LEFT" => {
            let s = txt(0)?;
            let n = int(1)?;
            if n <= 0 {
                return Some(Value::Text(String::new()));
            }
            Some(Value::Text(
                s.chars().take(n as usize).collect::<String>(),
            ))
        }
        "RIGHT" => {
            let s = txt(0)?;
            let n = int(1)?;
            if n <= 0 {
                return Some(Value::Text(String::new()));
            }
            let chars: Vec<char> = s.chars().collect();
            let take = (n as usize).min(chars.len());
            Some(Value::Text(
                chars[chars.len() - take..].iter().collect::<String>(),
            ))
        }
        "REVERSE" => txt(0).map(|s| Value::Text(s.chars().rev().collect::<String>())),
        "LPAD" | "RPAD" => {
            let s = txt(0)?;
            let target = int(1)? as usize;
            let pad = txt(2)?;
            if pad.is_empty() {
                return Some(Value::Text(s));
            }
            let cur = s.chars().count();
            if target <= cur {
                let out: String = s.chars().take(target).collect();
                return Some(Value::Text(out));
            }
            let fill: String = pad.chars().cycle().take(target - cur).collect();
            Some(Value::Text(if name == "LPAD" {
                format!("{fill}{s}")
            } else {
                format!("{s}{fill}")
            }))
        }
        "REPEAT" => {
            let s = txt(0)?;
            let n = int(1)?;
            if n <= 0 {
                return Some(Value::Text(String::new()));
            }
            Some(Value::Text(s.repeat(n as usize)))
        }
        "ABS" => match vals.first().cloned().flatten() {
            Some(Value::Int(i)) => Some(Value::Int(i.abs())),
            Some(Value::Float(f)) => Some(Value::Float(f.abs())),
            Some(v) => fn_as_f64(&v).map(|f| Value::Float(f.abs())),
            None => None,
        },
        "ROUND" => {
            let x = num(0)?;
            let d = int(1).unwrap_or(0);
            // MySQL rounds half away from zero.
            let factor = 10f64.powi(d as i32);
            let r = (x * factor).round() / factor;
            if vals.first().cloned().flatten().is_some_and(|v| matches!(v, Value::Int(_))) && d <= 0 {
                Some(Value::Int(r as i64))
            } else {
                Some(Value::Float(r))
            }
        }
        "FLOOR" => num(0).map(|f| Value::Int(f.floor() as i64)),
        "CEIL" | "CEILING" => num(0).map(|f| Value::Int(f.ceil() as i64)),
        "SQRT" => num(0).and_then(|f| {
            if f < 0.0 {
                None // MySQL: negative input → NULL
            } else {
                Some(Value::Float(f.sqrt()))
            }
        }),
        "POW" | "POWER" => num(0)?
            .powf(num(1)?)
            .try_into()
            .ok()
            .map(Value::Float),
        "SIGN" => num(0).map(|f| Value::Int(f.signum() as i64)),
        "MOD" => {
            let (a, b) = (num(0)?, num(1)?);
            if b == 0.0 {
                return None;
            }
            if matches!(vals.first(), Some(Some(Value::Int(_))))
                && matches!(vals.get(1), Some(Some(Value::Int(_))))
            {
                Some(Value::Int(a as i64 % b as i64))
            } else {
                Some(Value::Float(a % b))
            }
        }
        "COALESCE" => vals
            .iter()
            .find(|v| sql_non_null(v))
            .cloned()
            .flatten()
            .or(Some(Value::Null)),
        "IFNULL" | "NVL" => {
            let a = vals.first().cloned().flatten();
            match a {
                Some(v) if !matches!(v, Value::Null) => Some(v),
                _ => Some(vals.get(1).cloned().flatten().unwrap_or(Value::Null)),
            }
        }
        "NULLIF" => {
            let (a, b) = (vals.first().cloned().flatten()?, vals.get(1).cloned().flatten()?);
            if !a.is_null() && !b.is_null() && a.compare(&b) == std::cmp::Ordering::Equal {
                Some(Value::Null)
            } else {
                Some(a)
            }
        }
        "IF" => {
            let cond = vals.first().cloned().flatten()?;
            let branch = if is_truthy(&cond) { 1 } else { 2 };
            vals.get(branch).cloned().flatten()
        }
        "GREATEST" | "LEAST" => {
            if any_null {
                return None;
            }
            let mut best = vals.first().cloned().flatten()?;
            for v in &vals[1..] {
                let v = v.clone()?;
                let ord = v.compare(&best);
                let take = match name {
                    "GREATEST" => ord == std::cmp::Ordering::Greater,
                    _ => ord == std::cmp::Ordering::Less,
                };
                if take {
                    best = v;
                }
            }
            Some(best)
        }
        "NOW" => Some(Value::Text(datetime_text(now_parts()))),
        "CURDATE" => {
            let (y, m, d, _, _, _) = now_parts();
            Some(Value::Text(format!("{y:04}-{m:02}-{d:02}")))
        }
        "CURTIME" => {
            let (_, _, _, hh, mm, ss) = now_parts();
            Some(Value::Text(format!("{hh:02}:{mm:02}:{ss:02}")))
        }
        "UNIX_TIMESTAMP" => Some(Value::Int(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        )),
        "DATE_FORMAT" | "TIME_FORMAT" => {
            let dt = txt(0)?;
            let fmt = txt(1)?;
            Some(Value::Text(format_datetime(&parse_datetime(&dt), &fmt)))
        }
        "YEAR" => Some(Value::Int(parse_datetime(&txt(0)?).0)),
        "MONTH" => Some(Value::Int(parse_datetime(&txt(0)?).1)),
        "DAY" | "DAYOFMONTH" => Some(Value::Int(parse_datetime(&txt(0)?).2)),
        "HOUR" => Some(Value::Int(parse_datetime(&txt(0)?).3)),
        "MINUTE" => Some(Value::Int(parse_datetime(&txt(0)?).4)),
        "SECOND" => Some(Value::Int(parse_datetime(&txt(0)?).5)),
        "QUARTER" => {
            let m = parse_datetime(&txt(0)?).1;
            Some(Value::Int((m - 1) / 3 + 1))
        }
        "DAYOFWEEK" => {
            // MySQL: 1 = Sunday … 7 = Saturday
            let (y, mo, d, _, _, _) = parse_datetime(&txt(0)?);
            let wd = (days_from_civil(y, mo as u32, d as u32).rem_euclid(7) + 4) % 7;
            Some(Value::Int(wd + 1))
        }
        "DAYOFYEAR" => {
            let (y, mo, d, _, _, _) = parse_datetime(&txt(0)?);
            Some(Value::Int(
                days_from_civil(y, mo as u32, d as u32)
                    - days_from_civil(y, 1, 1)
                    + 1,
            ))
        }
        "WEEK" => {
            let (y, mo, d, _, _, _) = parse_datetime(&txt(0)?);
            let doy = days_from_civil(y, mo as u32, d as u32)
                - days_from_civil(y, 1, 1)
                + 1;
            Some(Value::Int((doy - 1) / 7 + 1))
        }
        "DAYNAME" => {
            let (y, mo, d, _, _, _) = parse_datetime(&txt(0)?);
            let wd = (days_from_civil(y, mo as u32, d as u32).rem_euclid(7) + 4) % 7;
            Some(Value::Text(DAY_NAMES[wd as usize].to_string()))
        }
        "MONTHNAME" => {
            let m = parse_datetime(&txt(0)?).1;
            Some(Value::Text(
                MONTH_NAMES[(m as usize - 1).clamp(0, 11)].to_string(),
            ))
        }
        _ => None, // is_scalar_fn gate keeps this unreachable
    }
}

// --- MySQL datetime helpers (scalar fn support) --------------------------

/// Current UTC time as (year, month, day, hour, minute, second).
pub(crate) fn now_parts() -> (i64, i64, i64, i64, i64, i64) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m as i64, d as i64, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Howard Hinnant's days-since-epoch → (year, month, day).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since epoch for a proleptic Gregorian date (inverse of civil_from_days).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m as i64;
    let doy = (153 * (if mp > 2 { mp - 3 } else { mp + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DD[ HH:MM:SS[:fraction]]` (MySQL DATE/DATETIME text).
/// Missing time parts default to 00:00:00.
pub(crate) fn parse_datetime(s: &str) -> (i64, i64, i64, i64, i64, i64) {
    let s = s.trim();
    let mut date = s;
    let mut time = "";
    if let Some(sp) = s.find(' ') {
        date = &s[..sp];
        time = s[sp + 1..].trim_end_matches('Z');
    }
    let nums: Vec<i64> = date
        .split('-')
        .filter_map(|p| p.parse::<i64>().ok())
        .collect();
    let (y, m, d) = match nums.as_slice() {
        [y, m, d] => (*y, *m, *d),
        _ => {
            let (y, m, d, _, _, _) = now_parts();
            (y, m, d)
        }
    };
    let tnums: Vec<i64> = time
        .split(':')
        .map(|p| p.split('.').next().unwrap_or(p))
        .filter_map(|p| p.parse::<i64>().ok())
        .collect();
    let (hh, mm, ss) = match tnums.as_slice() {
        [a, b, c] => (*a, *b, *c),
        [a, b] => (*a, *b, 0),
        [a] => (*a, 0, 0),
        _ => (0, 0, 0),
    };
    (y, m, d, hh, mm, ss)
}

/// (y, m, d, hh, mm, ss) → `YYYY-MM-DD HH:MM:SS`.
pub(crate) fn datetime_text(p: (i64, i64, i64, i64, i64, i64)) -> String {
    let (y, m, d, hh, mm, ss) = p;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        p.0, p.1, p.2, p.3, p.4, p.5
    )
}

/// Shift a datetime value by `n` units (MySQL `+/- INTERVAL n UNIT`).
/// Date parts carry (months for MONTH/YEAR, days for the rest).
pub(crate) fn shift_datetime(
    v: &Value,
    n: i64,
    unit: &str,
) -> Value {
    let (y, mo, d, hh, mm, ss) = parse_datetime(&fn_to_text(v));
    match unit.to_ascii_uppercase().as_str() {
        "YEAR" | "MONTH" => {
            let months = y * 12 + mo as i64 - 1
                + if unit.eq_ignore_ascii_case("YEAR") { n * 12 } else { n };
            let ny = months.div_euclid(12);
            let nmo = months.rem_euclid(12) as u32 + 1;
            // clamp day to the target month's length
            let dim = days_in_month(ny, nmo);
            let nd = (d as i64).min(dim) as u32;
            Value::Text(datetime_text((ny, nmo as i64, nd as i64, hh, mm, ss)))
        }
        "SECOND" | "MINUTE" | "HOUR" | "DAY" | "WEEK" => {
            let mult = match unit.to_ascii_uppercase().as_str() {
                "SECOND" => 1,
                "MINUTE" => 60,
                "HOUR" => 3600,
                "DAY" => 86_400,
                _ => 604_800,
            };
            let total = days_from_civil(y, mo as u32, d as u32) * 86_400
                + hh * 3600
                + mm * 60
                + ss
                + n * mult;
            let days = total.div_euclid(86_400);
            let rem = total.rem_euclid(86_400);
            let (ny, nmo, nd) = civil_from_days(days);
            Value::Text(datetime_text((
                ny,
                nmo as i64,
                nd as i64,
                rem / 3600,
                (rem % 3600) / 60,
                rem % 60,
            )))
        }
        _ => v.clone(),
    }
}

fn days_in_month(y: i64, m: u32) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July",
    "August", "September", "October", "November", "December",
];

const DAY_NAMES: [&str; 7] = [
    "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday",
];

/// MySQL DATE_FORMAT subset: the specifiers real CMS dumps use.
fn format_datetime(p: &(i64, i64, i64, i64, i64, i64), fmt: &str) -> String {
    let (y, mo, d, hh, mm, ss) = *p;
    let weekday = (days_from_civil(y, mo as u32, d as u32).rem_euclid(7) + 4) % 7; // 1970-01-01 = Thursday
    let months = &MONTH_NAMES;
    let days_n = &DAY_NAMES;
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('y') => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Some('m') => out.push_str(&format!("{mo:02}")),
            Some('c') => out.push_str(&mo.to_string()),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('e') => out.push_str(&d.to_string()),
            Some('H') => out.push_str(&format!("{hh:02}")),
            Some('k') => out.push_str(&hh.to_string()),
            Some('h') | Some('I') => {
                let h12 = if hh % 12 == 0 { 12 } else { hh % 12 };
                out.push_str(&format!("{h12:02}"))
            }
            Some('i') => out.push_str(&format!("{mm:02}")),
            Some('s') | Some('S') => out.push_str(&format!("{ss:02}")),
            Some('f') => out.push_str("000000"),
            Some('p') => out.push_str(if hh < 12 { "AM" } else { "PM" }),
            Some('W') => out.push_str(days_n[weekday as usize]),
            Some('a') => out.push_str(&days_n[weekday as usize][..3]),
            Some('M') => out.push_str(months[(mo as usize - 1).min(11)]),
            Some('b') => out.push_str(&months[(mo as usize - 1).min(11)][..3]),
            Some('j') => out.push_str(&format!(
                "{:03}",
                days_from_civil(y, mo as u32, d as u32) - days_from_civil(y, 1, 1) + 1
            )),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}
