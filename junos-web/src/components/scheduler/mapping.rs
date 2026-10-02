//! Scheduler tab: maps a job's completion condition to KStars' form fields
//! (the start condition and constraints come from `form::JobOptions`).

pub fn resolve_completion_condition(
    condition: &str,
    completion_count: String,
    completion_at: String,
) -> (bool, bool, i64, bool, bool, String) {
    match condition {
        "repeat" => (
            false,
            true,
            completion_count.parse::<i64>().unwrap_or(1),
            false,
            false,
            String::new(),
        ),
        "loop" => (false, false, 1, true, false, String::new()),
        "at" => (false, false, 1, false, true, completion_at),
        _ => (true, false, 1, false, false, String::new()),
    }
}
