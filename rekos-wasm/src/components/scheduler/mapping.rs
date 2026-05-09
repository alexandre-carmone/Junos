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

pub fn resolve_startup_condition(condition: &str, startup_at: String) -> (bool, bool, String) {
    if condition == "at" {
        (false, true, startup_at)
    } else {
        (true, false, String::new())
    }
}
