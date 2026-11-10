//! C `printf` number formatting: `%g`, `%.*g`, `%.*f` and `%d`.
//!
//! PHD2 formats every number of its guide log (`guidinglog.cpp`) and of its
//! event server (`event_server.cpp`) through `wxString::Format`, i.e. the C
//! library's `printf`. Clients that parse those files and messages (PHD2 Log
//! Viewer, KStars' PHD2 driver, the junos-web Guide tab) see the exact glibc
//! spelling, and Rust's `{}` / `{:e}` spell numbers differently — there is no
//! `%g` at all. These helpers reproduce glibc byte for byte.
//!
//! The rules (C11 7.21.6.1, as glibc implements them):
//!
//! | spec     | rule                                                                  |
//! |----------|-----------------------------------------------------------------------|
//! | `%g`     | `%.6g`                                                                |
//! | `%.Pg`   | P = 1 when 0. Round to P significant digits; X = the decimal exponent  |
//! |          | *after* that rounding. If −4 ≤ X < P: `%.(P−1−X)f`, else `%.(P−1)e`.  |
//! |          | Trailing zeros of the fraction, then a trailing `.`, are removed in    |
//! |          | both styles. So `100000` → `100000`, `1000000` → `1e+06`,             |
//! |          | `0.0001` → `0.0001`, `0.00001` → `1e-05`, `999999.5` → `1e+06`.       |
//! | `%e`     | Exponent with a sign and at least two digits: `1e+06`, `1.5e-07`,     |
//! |          | `1e+100`.                                                             |
//! | `%.Pf`   | P digits after the point, rounded to nearest, ties to even, on the    |
//! |          | exact binary value: `0.125` → `0.12`, `0.375` → `0.38`, `2.5` → `2`,  |
//! |          | `-1.005` → `-1.00` (the double is just under −1.005).                 |
//! | `%d`     | Plain decimal.                                                        |
//! | specials | `-0` keeps its sign (`-0`, `-0.00`); `inf`, `-inf`, `nan`, `-nan`.    |
//!
//! Rust's `{:.P}` and `{:.Pe}` already round the exact binary value to nearest
//! even, like glibc, so the rounding is delegated to them and only the style
//! choice, the zero stripping, the exponent spelling and the specials are done
//! here (the tests pin the tie cases).
//!
//! Ported from PHD2 `src/guidinglog.cpp`, `src/event_server.cpp` — BSD-3-Clause, see THIRD_PARTY.md

/// C `%g`: six significant digits, `%e` style outside `1e-4 ..= 999999.5`.
pub fn fmt_g(v: f64) -> String {
    fmt_gp(v, 6)
}

/// C `%.<precision>g` (a precision of 0 is treated as 1, as `printf` does).
pub fn fmt_gp(v: f64, precision: usize) -> String {
    if let Some(s) = special(v) {
        return s;
    }
    let p = precision.max(1);
    // The style is chosen on the exponent *after* rounding to P significant
    // digits (C11 7.21.6.1p8), so 999999.5 → X = 6 → "1e+06", not "1000000".
    let e = format!("{:.*e}", p - 1, v);
    let (mantissa, exp) = e.split_once('e').expect("Rust `{:e}` always has an exponent");
    let x: i32 = exp.parse().expect("Rust `{:e}` exponent is a plain integer");
    if x >= -4 && x < p as i32 {
        // %f with P − 1 − X decimals: the same digit position %e rounded to,
        // so the two spellings never disagree.
        strip_zeros(format!("{:.*}", (p as i32 - 1 - x) as usize, v))
    } else {
        format!(
            "{}e{}{:02}",
            strip_zeros(mantissa.to_string()),
            if x < 0 { '-' } else { '+' },
            x.abs()
        )
    }
}

/// C `%.<prec>f`: `prec` decimals, ties to even on the binary value.
pub fn fmt_f(v: f64, prec: usize) -> String {
    special(v).unwrap_or_else(|| format!("{:.*}", prec, v))
}

/// C `%d`.
pub fn fmt_d(v: i64) -> String {
    v.to_string()
}

/// glibc spells the non-finite values in lowercase and keeps the sign of a
/// negative NaN; Rust prints `NaN`.
fn special(v: f64) -> Option<String> {
    if v.is_nan() {
        Some(if v.is_sign_negative() { "-nan" } else { "nan" }.to_string())
    } else if v.is_infinite() {
        Some(if v < 0.0 { "-inf" } else { "inf" }.to_string())
    } else {
        None
    }
}

/// `%g` without `#`: drop the fraction's trailing zeros, then a bare `.`.
fn strip_zeros(mut s: String) -> String {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}
