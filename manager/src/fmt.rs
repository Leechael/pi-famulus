//! Human formatting for the CLI: terminal display width (CJK/emoji take two
//! columns), durations, local timestamps, paths.

use std::path::Path;

/// Columns a character occupies in a terminal: 0 for combining marks and
/// zero-width characters, 2 for East Asian wide/fullwidth and emoji, else 1.
/// A compact table covering the common ranges, not a full UAX #11.
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if u == 0 || (0x0300..=0x036F).contains(&u)
        || (0x200B..=0x200F).contains(&u)
        || (0xFE00..=0xFE0F).contains(&u)
        || u == 0x20E3
    {
        return 0;
    }
    if u < 0x20 || (0x7F..0xA0).contains(&u) {
        return 0;
    }
    let wide = (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0x303E).contains(&u)
        || (0x3041..=0x33FF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xA000..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1F64F).contains(&u)
        || (0x1F900..=0x1F9FF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u);
    if wide {
        2
    } else {
        1
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Cut `s` to at most `max` display columns, ending with "…" when cut.
pub fn truncate_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Keep the *end* of `s` within `max` columns ("…" + tail); for paths.
pub fn truncate_width_left(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if w + cw > max - 1 {
            break;
        }
        tail.push(c);
        w += cw;
    }
    let mut out = String::from("…");
    out.extend(tail.into_iter().rev());
    out
}

/// Pad `s` with spaces to `w` display columns (left-aligned).
pub fn pad(s: &str, w: usize) -> String {
    let dw = display_width(s);
    if dw >= w {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(w - dw))
}

/// "850ms", "12s", "3m04s", "2h03m", "3d04h".
pub fn human_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let (m, s) = (s / 60, s % 60);
    if m < 60 {
        return format!("{m}m{s:02}s");
    }
    let (h, m) = (m / 60, m % 60);
    if h < 24 {
        return format!("{h}h{m:02}m");
    }
    let (d, h) = (h / 24, h % 24);
    format!("{d}d{h:02}h")
}

/// Parse "500ms", "30s", "10m", "2h", "1d" (a bare number is seconds).
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("bad duration {s:?} (examples: 30s, 10m, 2h, 1d)"))?;
    let mult = match unit {
        "ms" => 1,
        "" | "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(format!("bad duration unit in {s:?} (use ms, s, m, h, d)")),
    };
    Ok(n.saturating_mul(mult))
}

/// Local broken-down time for an epoch-ms timestamp.
struct Local {
    year: i32,
    mon: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
    ms: u32,
}

fn local(ms: u64) -> Local {
    let tm = crate::sys::localtime((ms / 1000) as i64);
    Local {
        year: tm.year,
        mon: tm.month,
        day: tm.day,
        hour: tm.hour,
        min: tm.min,
        sec: tm.sec,
        ms: (ms % 1000) as u32,
    }
}

/// "2026-09-23 14:03:22.123"
pub fn datetime_ms(ms: u64) -> String {
    let t = local(ms);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.year, t.mon, t.day, t.hour, t.min, t.sec, t.ms
    )
}

/// "2026-09-23 14:03:22"
pub fn datetime(ms: u64) -> String {
    let t = local(ms);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.mon, t.day, t.hour, t.min, t.sec
    )
}

/// Compact: "14:03:22" today, otherwise "09-22 14:03".
pub fn short_time(ms: u64, now: u64) -> String {
    let t = local(ms);
    let n = local(now);
    if (t.year, t.mon, t.day) == (n.year, n.mon, n.day) {
        format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec)
    } else {
        format!("{:02}-{:02} {:02}:{:02}", t.mon, t.day, t.hour, t.min)
    }
}

/// "3m ago" style age.
pub fn ago(ms: u64, now: u64) -> String {
    format!("{} ago", human_duration(now.saturating_sub(ms)))
}

/// Replace the user's home directory prefix with "~".
pub fn tilde(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        if let Ok(rest) = Path::new(path).strip_prefix(home) {
            let rest = rest.to_string_lossy();
            return if rest.is_empty() { "~".into() } else { format!("~/{rest}") };
        }
    }
    path.to_string()
}

/// First line of a (possibly multi-line) command.
pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_and_truncation() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("e\u{301}"), 1); // combining accent
        assert_eq!(display_width("😀"), 2);
        assert_eq!(truncate_width("中文字符", 5), "中文…");
        assert_eq!(display_width(&truncate_width("中文字符abc", 6)), 5);
        assert_eq!(truncate_width("abc", 3), "abc");
        assert_eq!(truncate_width("abcd", 3), "ab…");
        assert_eq!(truncate_width_left("/a/b/中文", 5), "…中文");
        assert_eq!(truncate_width_left("/a/b/c", 4), "…b/c");
        assert_eq!(pad("中", 4), "中  ");
        assert_eq!(display_width(&pad("中", 4)), 4);
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(850), "850ms");
        assert_eq!(human_duration(12_300), "12s");
        assert_eq!(human_duration(184_000), "3m04s");
        assert_eq!(human_duration(803_428), "13m23s");
        assert_eq!(human_duration(7_380_000), "2h03m");
        assert_eq!(human_duration(273_600_000), "3d04h");
        assert_eq!(parse_duration("10m").unwrap(), 600_000);
        assert_eq!(parse_duration("30").unwrap(), 30_000);
        assert_eq!(parse_duration("500ms").unwrap(), 500);
        assert_eq!(parse_duration("1d").unwrap(), 86_400_000);
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("m").is_err());
    }
}
