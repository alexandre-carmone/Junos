//! Where a job's frames go: Ekos' `<FITSDirectory>` + `<PlaceholderFormat>`
//! turned into INDIGO's `CCD_LOCAL_MODE {DIR, PREFIX}`.
//!
//! Ekos expands the whole pattern itself (`PlaceholderPath`); the INDIGO
//! camera driver expands its own placeholders in the file name only
//! (`CCD_DRIVER_SAVED_IMAGES.md`: `%o` object, `%F` frame type, `%C` filter,
//! `%E` exposure, `%-D`/`%-H` date and time, `%nI` next free index — which
//! must be the last token). So the directory part is expanded here, token by
//! token, and the file part is translated into the driver's dialect. The
//! relay creates the directory on *its* host, like `imaging.rs` assumes the
//! captures dir is local to the camera driver.

use super::Job;

/// Ekos' `%t`: a target name safe for a path.
pub fn sanitize(target: &str) -> String {
    let s: String = target
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    s.trim_matches('_').to_string()
}

fn exposure_text(e: f64) -> String {
    let s = format!("{e:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Expand one `_`-separated segment of the pattern. `file` selects the
/// driver's dialect (file name) over literal expansion (directory). An empty
/// result drops the segment with its separator.
fn expand(segment: &str, job: &Job, file: bool, now: &str) -> String {
    let target = sanitize(&job.target);
    let mut out = String::new();
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => {
                if !target.is_empty() {
                    out.push_str(if file { "%o" } else { &target });
                }
            }
            Some('T') => out.push_str(if file { "%F" } else { job.frame_type.label() }),
            Some('F') => match &job.filter {
                Some(f) if file => out.push_str(if f.is_empty() { "" } else { "%C" }),
                Some(f) => out.push_str(&sanitize(f)),
                None => {}
            },
            Some('e') => {
                if file {
                    out.push_str("%E_secs");
                } else {
                    out.push_str(&exposure_text(job.exposure));
                    out.push_str("_secs");
                }
            }
            Some('D') => out.push_str(if file { "%-DT%-H" } else { now }),
            // `%s` (the frame counter) is appended at the end by `local_mode`;
            // `%d` `%p` `%f` and anything else have no counterpart.
            Some(_) => {}
            None => out.push('_'),
        }
    }
    out
}

fn expand_part(part: &str, job: &Job, file: bool, now: &str) -> String {
    part.split('_')
        .map(|seg| expand(seg, job, file, now))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// `(DIR, PREFIX)` for a job. `now` is the timestamp `%D` expands to in the
/// directory part (`yyyy-MM-ddThh-mm-ss`, as Ekos writes it).
pub fn local_mode(job: &Job, captures_dir: &str, now: &str) -> (String, String) {
    let base = if job.fits_dir.trim().is_empty() { captures_dir } else { job.fits_dir.trim() };
    let base = base.trim_end_matches('/');
    let pattern = job.placeholder_format.trim();
    let (dir_part, file_part) = match pattern.rfind('/') {
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
        None => ("", pattern),
    };
    let mut dir = base.to_string();
    for seg in dir_part.split('/') {
        let s = expand_part(seg, job, false, now);
        if !s.is_empty() {
            dir.push('/');
            dir.push_str(&s);
        }
    }
    let mut prefix = expand_part(file_part, job, true, now);
    if prefix.is_empty() {
        prefix = "%F".to_string();
    }
    let digits = job.placeholder_suffix.clamp(1, 5);
    prefix.push_str(&format!("_%{digits}I"));
    (dir, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::capture::{FrameType, Job};

    fn job() -> Job {
        let mut j = Job::new(FrameType::Light, 120.0, 10);
        j.filter = Some("Ha".into());
        j.target = "M 31".into();
        j.placeholder_format = "/%t/%t_%F_%T_%e_%D".into();
        j.placeholder_suffix = 4;
        j
    }

    #[test]
    fn target_folder_pattern() {
        let (dir, prefix) = local_mode(&job(), "/caps", "2026-10-15T22-30-00");
        assert_eq!(dir, "/caps/M_31");
        assert_eq!(prefix, "%o_%C_%F_%E_secs_%-DT%-H_%4I");
    }

    #[test]
    fn type_filter_pattern_and_filterless_rows() {
        let mut j = job();
        j.placeholder_format = "/%T/%F/%T_%F_%e".into();
        j.fits_dir = "/data/night/".into();
        let (dir, prefix) = local_mode(&j, "/caps", "now");
        assert_eq!(dir, "/data/night/Light/Ha");
        assert_eq!(prefix, "%F_%C_%E_secs_%4I");
        j.filter = None;
        j.frame_type = FrameType::Dark;
        let (dir, prefix) = local_mode(&j, "/caps", "now");
        assert_eq!(dir, "/data/night/Dark");
        assert_eq!(prefix, "%F_%E_secs_%4I");
    }

    #[test]
    fn empty_target_default_pattern_and_suffix_clamp() {
        let mut j = job();
        j.target = String::new();
        j.placeholder_format = super::super::esq::DEFAULT_PLACEHOLDER.into();
        j.placeholder_suffix = 9;
        let (dir, prefix) = local_mode(&j, "/caps", "2026-10-15T22-30-00");
        assert_eq!(dir, "/caps/Light/Ha");
        assert_eq!(prefix, "%F_%C_%E_secs_%-DT%-H_%5I");
        j.placeholder_format = "%t".into();
        j.placeholder_suffix = 0;
        assert_eq!(local_mode(&j, "/caps", "x").1, "%F_%1I");
        j.target = "a/b%c".into();
        assert_eq!(sanitize(&j.target), "a_b_c");
        assert_eq!(local_mode(&j, "/caps", "x").1, "%o_%1I");
    }

    #[test]
    fn directory_timestamp_and_exposure() {
        let mut j = job();
        j.placeholder_format = "/%D/%e_%T/frame".into();
        j.exposure = 0.5;
        let (dir, prefix) = local_mode(&j, "/caps", "2026-10-15T22-30-00");
        assert_eq!(dir, "/caps/2026-10-15T22-30-00/0.5_secs_Light");
        assert_eq!(prefix, "frame_%4I");
    }
}
