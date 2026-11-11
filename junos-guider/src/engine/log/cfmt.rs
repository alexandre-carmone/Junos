//! C `printf` number formatting: `%g`, `%.*g` and `%.*f`.
//!
//! PHD2 formats every number of its guide log (`guidinglog.cpp`) and of its
//! event server (`event_server.cpp`) through `wxString::Format`, i.e. the C
//! library's `printf`. Clients that parse those files and messages (PHD2 Log
//! Viewer, KStars' PHD2 driver, the junos-web Guide tab) see the exact glibc
//! spelling, and Rust's `{}` / `{:e}` spell numbers differently — there is no
//! `%g` at all. These helpers reproduce glibc byte for byte. The decimal
//! point is always `.`: PHD2 forces `LC_NUMERIC` to "C" (`phd.cpp:600`).
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
//! |          | `-1.005` → `-1.00` (its magnitude is just under 1.005).               |
//! | specials | `-0` keeps its sign (`-0`, `-0.00`); `inf`, `-inf`, `nan`, `-nan`.    |
//!
//! `%d` and `%u` need no helper: Rust's `{}` spells integers the same way.
//!
//! x86-64's runtime `0.0 / 0.0` is a *negative* NaN, so PHD2 prints `-nan`
//! there (e.g. a sigma over one sample, `guiding_stats.cpp`). Rust leaves the
//! sign of a computed NaN unspecified and constant folding may make it
//! positive: a golden containing a NaN must come from a runtime division.
//!
//! Rust's `{:.P}` and `{:.Pe}` already round the exact binary value to nearest
//! even, like glibc, so the rounding is delegated to them and only the style
//! choice, the zero stripping, the exponent spelling and the specials are done
//! here (the tests pin the tie cases).
//!
//! Reproduces the glibc `printf` behind PHD2's `wxString::Format`
//! (`guidinglog.cpp`, `event_server.cpp:189,209-210`); no PHD2 code is ported.

/// C `%g`: six significant digits, `%e` style when |v| rounded to six
/// significant digits is below 1e-4 or at least 1e6.
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

#[cfg(test)]
mod tests {
    use super::*;

    // Expected strings are glibc's `snprintf` (2.44), printed by a throwaway C
    // harness; CPython's `%g` / `%.*f` gives the same text for finite values.

    #[test]
    fn g_plan_vectors() {
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(100000.0), "100000");
        assert_eq!(fmt_g(1e6), "1e+06");
        assert_eq!(fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(fmt_g(1.234567891), "1.23457");
        assert_eq!(fmt_g(0.0001), "0.0001");
        assert_eq!(fmt_g(0.00001), "1e-05");
    }

    #[test]
    fn g_style_switch_uses_the_rounded_exponent() {
        // Rounds up to 7 digits: %e style, not "1000000".
        assert_eq!(fmt_g(999999.5), "1e+06");
        // 123456.5 is exactly representable; ties to even → 123456.
        assert_eq!(fmt_g(123456.5), "123456");
        // Just under 1e-4 but rounds up to it: %f style.
        assert_eq!(fmt_g(9.9999995e-5), "0.0001");
        assert_eq!(fmt_g(9.9999999e-5), "0.0001");
        // 1.3e-21 above the tie at 6 digits: rounds up to 1e-4, %f style.
        assert_eq!(fmt_g(9.999995e-5), "0.0001");
        // The carry to 10 stays in %f style (X = 1 < 6).
        assert_eq!(fmt_g(9.9999995), "10");
        // Rounds down to 99999.9 (the double is below the tie).
        assert_eq!(fmt_g(99999.95), "99999.9");
        assert_eq!(fmt_g(123456789.0), "1.23457e+08");
    }

    #[test]
    fn g_zero_and_trailing_zeros() {
        assert_eq!(fmt_g(0.0), "0");
        assert_eq!(fmt_g(-0.0), "-0");
        assert_eq!(fmt_g(100.0), "100");
        assert_eq!(fmt_g(2.0 / 3.0), "0.666667");
        assert_eq!(fmt_g(0.1 + 0.2), "0.3");
        assert_eq!(fmt_g(-1234.5678), "-1234.57");
        assert_eq!(fmt_g(0.000123456789), "0.000123457");
    }

    #[test]
    fn g_exponent_spelling() {
        assert_eq!(fmt_g(1.5e-7), "1.5e-07");
        assert_eq!(fmt_g(-2.5e-5), "-2.5e-05");
        assert_eq!(fmt_g(1e15), "1e+15");
        assert_eq!(fmt_g(1e16), "1e+16");
        assert_eq!(fmt_g(1e21), "1e+21");
        assert_eq!(fmt_g(1e100), "1e+100");
        assert_eq!(fmt_g(1e-100), "1e-100");
        assert_eq!(fmt_g(5e-324), "4.94066e-324");
        assert_eq!(fmt_g(f64::MAX), "1.79769e+308");
    }

    #[test]
    fn g_specials() {
        assert_eq!(fmt_g(f64::NAN), "nan");
        assert_eq!(fmt_g(-f64::NAN), "-nan");
        assert_eq!(fmt_g(f64::INFINITY), "inf");
        assert_eq!(fmt_g(f64::NEG_INFINITY), "-inf");
        assert_eq!(fmt_f(f64::NAN, 2), "nan");
        assert_eq!(fmt_f(f64::INFINITY, 2), "inf");
        assert_eq!(fmt_f(-f64::NAN, 2), "-nan");
        assert_eq!(fmt_f(f64::NEG_INFINITY, 1), "-inf");
    }

    #[test]
    fn gp_precision() {
        // Precision 0 behaves as 1.
        assert_eq!(fmt_gp(1234.5, 0), "1e+03");
        assert_eq!(fmt_gp(1234.5, 1), "1e+03");
        // An exact tie at P = 4, %f style: ties to even → 1234.
        assert_eq!(fmt_gp(1234.5, 4), "1234");
        assert_eq!(fmt_gp(9.5, 1), "1e+01");
        assert_eq!(fmt_gp(0.95, 1), "0.9");
        assert_eq!(fmt_gp(0.25, 1), "0.2");
        assert_eq!(fmt_gp(2.5, 2), "2.5");
        assert_eq!(fmt_gp(0.000123, 2), "0.00012");
        assert_eq!(fmt_gp(1e6, 3), "1e+06");
        assert_eq!(fmt_gp(123.456, 3), "123");
        assert_eq!(fmt_gp(1234567.891, 8), "1234567.9");
        assert_eq!(fmt_gp(1.0 / 3.0, 10), "0.3333333333");
        assert_eq!(fmt_gp(0.0, 3), "0");
        assert_eq!(fmt_gp(-0.0, 2), "-0");
    }

    #[test]
    fn f_ties_to_even_on_the_binary_value() {
        assert_eq!(fmt_f(0.125, 2), "0.12");
        assert_eq!(fmt_f(0.375, 2), "0.38");
        assert_eq!(fmt_f(-1.005, 2), "-1.00");
        assert_eq!(fmt_f(2.0005, 3), "2.001");
        assert_eq!(fmt_f(0.25, 1), "0.2");
        assert_eq!(fmt_f(0.05, 1), "0.1");
        assert_eq!(fmt_f(2.5, 0), "2");
        assert_eq!(fmt_f(0.5, 0), "0");
        assert_eq!(fmt_f(1.5, 0), "2");
        assert_eq!(fmt_f(-0.5, 0), "-0");
        assert_eq!(fmt_f(-0.4, 0), "-0");
        assert_eq!(fmt_f(-0.0, 2), "-0.00");
        assert_eq!(fmt_f(1e-10, 3), "0.000");
        assert_eq!(fmt_f(1e20, 2), "100000000000000000000.00");
        // The exact binary value, all 309 digits.
        assert_eq!(
            fmt_f(f64::MAX, 0),
            concat!(
                "17976931348623157081452742373170435679807056752584499659891747680315726078002853876058955",
                "86327668781715404589535143824642343213268894641827684675467035375169860499105765512820762",
                "45490090389328944075868508455133942304583236903222948165808559332123348274797826204144723",
                "168738177180919299881250404026184124858368"
            )
        );
    }
}
