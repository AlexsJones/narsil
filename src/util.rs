//! Small shared helpers: ids, time, text truncation, logging.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// An info-level line in the log. Kept as a macro of its own for the plain
/// "this happened" lines; use `tracing::warn!`, `error!` and `debug!` directly
/// for anything else, and fields (`key = value`) for values worth filtering on.
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {{
        tracing::info!($($arg)*);
    }};
}

pub fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Short random id (8 chars, 40 bits), easy to type as a prefix.
pub fn new_id() -> String {
    use std::io::Read;
    let mut buf = [0u8; 8];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        buf = (nanos ^ ((std::process::id() as u64) << 32)).to_le_bytes();
    }
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstvwxyz0123456789";
    let mut n = u64::from_le_bytes(buf);
    let mut s = String::with_capacity(8);
    for _ in 0..8 {
        s.push(ALPHABET[(n & 31) as usize] as char);
        n >>= 5;
    }
    s
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

pub fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn ceil_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Keep the head and tail of an over-long string, eliding the middle.
pub fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max || max < 64 {
        return s.to_string();
    }
    let head = floor_boundary(s, max * 6 / 10);
    let tail = ceil_boundary(s, s.len() - max * 4 / 10);
    let dropped = s.len() - head - (s.len() - tail);
    format!(
        "{}\n\n[... {} bytes elided by narsil ...]\n\n{}",
        &s[..head],
        dropped,
        &s[tail..]
    )
}

/// First line, cut to `max` characters with an ellipsis.
pub fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

pub fn fmt_age(ts: i64) -> String {
    let d = (now() - ts).max(0);
    match d {
        0..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86_400),
    }
}

/// A duration as m:ss, or h:mm:ss from an hour up.
pub fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Rough token estimate for providers that do not report usage.
pub fn estimate_tokens(chars: usize) -> i64 {
    (chars / 4) as i64
}

pub fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_both_ends_on_char_boundaries() {
        let s = "é".repeat(500);
        let t = truncate_middle(&s, 200);
        assert!(t.starts_with('é') && t.ends_with('é'));
        assert!(t.contains("elided by narsil"));
        assert!(t.len() < s.len());
    }

    #[test]
    fn ids_are_short_and_distinct() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.len(), 8);
        assert_ne!(a, b);
    }
}
