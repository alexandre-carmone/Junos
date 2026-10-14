//! Sexagesimal parsing, after libindi's `f_scansexa` — the browser sends a
//! number property's edited string as-is (`components/devices.rs`), trusting
//! KStars to run it through that function.

/// Parse `"HH MM SS.s"`, `"DD:MM:SS"`, `"-5 23"`, `"12.5"` … into a float.
/// Separators may be spaces or colons; a leading sign applies to the whole.
/// `None` when nothing numeric is there.
pub fn parse(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    let (negative, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut parts = rest
        .split(|c: char| c == ':' || c.is_whitespace())
        .filter(|p| !p.is_empty());
    let d: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next().map(|p| p.parse().ok()).unwrap_or(Some(0.0))?;
    let sec: f64 = parts.next().map(|p| p.parse().ok()).unwrap_or(Some(0.0))?;
    let v = d + m / 60.0 + sec / 3600.0;
    Some(if negative { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn forms() {
        assert_eq!(parse("12.5"), Some(12.5));
        assert!((parse("05 35 17.3").unwrap() - 5.588_138_9).abs() < 1e-6);
        assert!((parse("-05:23:28").unwrap() + 5.391_111_1).abs() < 1e-6);
        assert!((parse("-0 30").unwrap() + 0.5).abs() < 1e-9);
        assert_eq!(parse("+10 00"), Some(10.0));
        assert_eq!(parse(""), None);
        assert_eq!(parse("abc"), None);
    }
}
