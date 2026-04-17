//! Centralized object search for the planetarium.
//!
//! Fault-tolerant matching across star and DSO catalogs. Combines three
//! strategies so that a single keystroke can resolve any of:
//!   - designation variants with/without separators ("M31" ≡ "M 31" ≡ "m-31"),
//!   - subsequence/prefix queries ("androm" → "Andromeda Galaxy"),
//!   - 1–2 character typos ("serius" → "Sirius", "andromida" → "Andromeda").
//!
//! Stars and DSOs are scored against the same scale and interleaved in the
//! final ranked list so the best match wins regardless of its origin catalog.

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::catalog::CatalogStar;
use crate::dso_catalog::Dso;

/// One search result. Flat on purpose: the click handler in `search.rs`
/// already discriminates star-vs-DSO by `size_arcmin > 1.0` to pick the FOV
/// auto-size, so wrapping this in an enum would just force a redundant
/// match-and-unpack on the caller side.
pub struct SearchHit {
    pub name: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    /// DSO major axis in arcminutes; `0.0` for stars (triggers the existing
    /// default-8° FOV branch in the click handler).
    pub size_arcmin: f32,
}

pub fn search_objects(
    query: &str,
    stars: &[CatalogStar],
    dsos: &[Dso],
    limit: usize,
) -> Vec<SearchHit> {
    let (q_spaced, q_compact) = normalize(query);
    if q_spaced.len() < 2 {
        return Vec::new();
    }

    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, SearchHit)> = Vec::new();

    for star in stars {
        let Some(name) = star.name.as_deref() else { continue };
        let (n_spaced, n_compact) = normalize(name);
        if let Some(s) = score(&matcher, &q_spaced, &q_compact, &n_spaced, &n_compact) {
            scored.push((s, SearchHit {
                name: name.to_string(),
                ra_deg: star.ra_deg as f64,
                dec_deg: star.dec_deg as f64,
                size_arcmin: 0.0,
            }));
        }
    }

    for dso in dsos {
        let (n_spaced, n_compact) = normalize(&dso.name);
        if let Some(s) = score(&matcher, &q_spaced, &q_compact, &n_spaced, &n_compact) {
            scored.push((s, SearchHit {
                name: dso.name.clone(),
                ra_deg: dso.ra_deg as f64,
                dec_deg: dso.dec_deg as f64,
                size_arcmin: dso.size_arcmin,
            }));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.truncate(limit);
    scored.into_iter().map(|(_, h)| h).collect()
}

/// Lowercase + drop non-ASCII-alphanumeric. Returns two forms:
///   - `spaced`: non-alnum runs collapsed to a single space — preserves word
///     boundaries for subsequence matching and per-token typo scoring.
///   - `compact`: spaces also removed — makes "m 31" ≡ "m-31" ≡ "m31" at the
///     string level, the key trick for designation-style queries.
fn normalize(s: &str) -> (String, String) {
    let mut spaced = String::with_capacity(s.len());
    let mut prev_alnum = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            spaced.push(c.to_ascii_lowercase());
            prev_alnum = true;
        } else if prev_alnum {
            spaced.push(' ');
            prev_alnum = false;
        }
    }
    if spaced.ends_with(' ') {
        spaced.pop();
    }
    let compact: String = spaced.chars().filter(|c| *c != ' ').collect();
    (spaced, compact)
}

fn score(
    matcher: &SkimMatcherV2,
    q_spaced: &str,
    q_compact: &str,
    n_spaced: &str,
    n_compact: &str,
) -> Option<i64> {
    // Fast path for designation queries: direct compact-form equality ranks
    // strictly above any prefix, which in turn ranks above any substring. This
    // is what makes "m3" prefer "M3" over "M30"/"M31".
    let compact_bonus = if n_compact == q_compact {
        1000
    } else if !q_compact.is_empty() && n_compact.starts_with(q_compact) {
        400
    } else if !q_compact.is_empty() && n_compact.contains(q_compact) {
        200
    } else {
        0
    };

    // SkimMatcherV2 handles partial/subsequence queries ("androm" →
    // "andromeda galaxy") but can't bridge character substitutions.
    // Argument order is (choice, pattern).
    let subseq_base = matcher.fuzzy_match(n_spaced, q_spaced).unwrap_or(0);

    // Edit-distance pass — the only strategy that catches true typos.
    let typo_bonus = typo_score(q_compact, n_compact, n_spaced);

    let total = compact_bonus + subseq_base + typo_bonus;
    if total > 0 { Some(total) } else { None }
}

/// Award a bonus if the query is within a small edit distance of the whole
/// compact name (handles multi-word typo queries) OR any single token of the
/// spaced name (handles short typo queries against long names like
/// "andromida" → token "andromeda" of "Andromeda Galaxy"). Queries shorter
/// than 3 chars skip this check to avoid spurious matches.
fn typo_score(q_compact: &str, n_compact: &str, n_spaced: &str) -> i64 {
    let q_len = q_compact.len();
    let max_dist = if q_len >= 8 { 2 } else if q_len >= 4 { 1 } else { 0 };
    if max_dist == 0 {
        return 0;
    }

    let q_bytes = q_compact.as_bytes();
    let mut best = usize::MAX;

    if let Some(d) = bounded_levenshtein(q_bytes, n_compact.as_bytes(), max_dist) {
        best = best.min(d);
    }

    for token in n_spaced.split(' ') {
        let t = token.as_bytes();
        if t.len() + max_dist < q_len || t.len() > q_len + max_dist {
            continue;
        }
        if let Some(d) = bounded_levenshtein(q_bytes, t, max_dist) {
            best = best.min(d);
            if best == 0 {
                break;
            }
        }
    }

    match best {
        0 => 300,
        1 => 150,
        2 => 50,
        _ => 0,
    }
}

/// Classic two-row DP with an early-exit when every cell on the current row
/// already exceeds `max_dist` — keeps the hot path cheap against long names.
fn bounded_levenshtein(a: &[u8], b: &[u8], max_dist: usize) -> Option<usize> {
    let m = a.len();
    let n = b.len();
    if m.abs_diff(n) > max_dist {
        return None;
    }
    if m == 0 {
        return if n <= max_dist { Some(n) } else { None };
    }
    if n == 0 {
        return if m <= max_dist { Some(m) } else { None };
    }

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        let mut row_min = curr[0];
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + cost);
            if curr[j] < row_min {
                row_min = curr[j];
            }
        }
        if row_min > max_dist {
            return None;
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    let d = prev[n];
    if d > max_dist { None } else { Some(d) }
}
