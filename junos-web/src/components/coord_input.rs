use leptos::html::Input;
use leptos::prelude::*;

#[derive(Copy, Clone, PartialEq)]
pub enum CoordMode {
    Hms,
    DmsSigned,
}

fn parse_u32(s: &str) -> u32 {
    s.parse::<u32>().unwrap_or(0)
}

pub fn parse_canonical(s: &str) -> (bool, u32, u32, u32) {
    let mut t = s.trim();
    let mut sign = true;
    if let Some(rest) = t.strip_prefix('-').or_else(|| t.strip_prefix('−')) {
        sign = false;
        t = rest;
    } else if let Some(rest) = t.strip_prefix('+') {
        t = rest;
    }
    let parts: Vec<&str> = t
        .split(|c: char| c == ':' || c.is_whitespace() || c == '°' || c == '\'' || c == '"' || c == 'h' || c == 'm' || c == 's')
        .filter(|p| !p.is_empty())
        .collect();
    let a = parts.first().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0);
    let b = parts.get(1).and_then(|p| p.parse::<u32>().ok()).unwrap_or(0);
    let c = parts.get(2).and_then(|p| p.parse::<u32>().ok()).unwrap_or(0);
    (sign, a, b, c)
}

/// Decimal hours → canonical "HH MM SS" string.
pub fn hours_to_hms_string(h: f64) -> String {
    let h = h.rem_euclid(24.0);
    let total = (h * 3600.0).round() as i64;
    let hh = ((total / 3600).rem_euclid(24)) as u32;
    let mm = ((total % 3600) / 60) as u32;
    let ss = (total % 60) as u32;
    format!("{:02} {:02} {:02}", hh, mm, ss)
}

/// Decimal degrees → canonical "+DD MM SS" / "-DD MM SS" string.
pub fn degrees_to_dms_string(d: f64) -> String {
    let sign = if d < 0.0 { "-" } else { "+" };
    let abs = d.abs();
    let total = (abs * 3600.0).round() as i64;
    let dd = (total / 3600) as u32;
    let mm = ((total % 3600) / 60) as u32;
    let ss = (total % 60) as u32;
    format!("{}{:02} {:02} {:02}", sign, dd, mm, ss)
}

/// Canonical HMS string → decimal hours.
pub fn hms_string_to_hours(s: &str) -> f64 {
    let (_, h, m, sec) = parse_canonical(s);
    h as f64 + m as f64 / 60.0 + sec as f64 / 3600.0
}

/// Canonical DMS string → decimal degrees (signed).
pub fn dms_string_to_degrees(s: &str) -> f64 {
    let (sign, d, m, sec) = parse_canonical(s);
    let v = d as f64 + m as f64 / 60.0 + sec as f64 / 3600.0;
    if sign { v } else { -v }
}

fn format_canonical(sign: bool, a: u32, b: u32, c: u32, mode: CoordMode) -> String {
    match mode {
        CoordMode::Hms => format!("{:02} {:02} {:02}", a, b, c),
        CoordMode::DmsSigned => format!(
            "{}{:02} {:02} {:02}",
            if sign { "+" } else { "-" },
            a,
            b,
            c
        ),
    }
}

const FIELD_CLS: &str =
    "input font-mono tabular-nums text-center !w-12 !px-1 !py-1";
const SEP_CLS: &str = "font-mono text-text-faint select-none";

#[component]
pub fn CoordInput(
    mode: CoordMode,
    value: RwSignal<String>,
    #[prop(optional, into)] aria_label: Option<String>,
) -> impl IntoView {
    // Parse the externally-supplied initial value into pieces.
    let initial = value.get_untracked();
    let (init_sign, ia, ib, ic) = parse_canonical(&initial);
    let empty_init = initial.trim().is_empty();

    let sign = RwSignal::new(init_sign);
    let s_a = RwSignal::new(if empty_init { String::new() } else { format!("{:02}", ia) });
    let s_b = RwSignal::new(if empty_init { String::new() } else { format!("{:02}", ib) });
    let s_c = RwSignal::new(if empty_init { String::new() } else { format!("{:02}", ic) });

    let ref_b: NodeRef<Input> = NodeRef::new();
    let ref_c: NodeRef<Input> = NodeRef::new();

    // External → internal: reflect outside writes (catalog lookup, prefill,
    // form clear) into the three pieces, but skip if it matches what we
    // ourselves would write next.
    Effect::new(move |_| {
        let ext = value.get();
        let cur = format_canonical(
            sign.get_untracked(),
            parse_u32(&s_a.get_untracked()),
            parse_u32(&s_b.get_untracked()),
            parse_u32(&s_c.get_untracked()),
            mode,
        );
        if ext == cur {
            return;
        }
        let trimmed_empty = ext.trim().is_empty();
        let (sg, a, b, c) = parse_canonical(&ext);
        sign.set(sg);
        if trimmed_empty {
            s_a.set(String::new());
            s_b.set(String::new());
            s_c.set(String::new());
        } else {
            s_a.set(format!("{:02}", a));
            s_b.set(format!("{:02}", b));
            s_c.set(format!("{:02}", c));
        }
    });

    // Internal → external: rewrite the canonical string whenever any piece
    // changes.
    Effect::new(move |_| {
        let new_v = format_canonical(
            sign.get(),
            parse_u32(&s_a.get()),
            parse_u32(&s_b.get()),
            parse_u32(&s_c.get()),
            mode,
        );
        if new_v != value.get_untracked() {
            value.set(new_v);
        }
    });

    // Digit-only sanitiser; clamps each field to two characters.
    let sanitize = |raw: &str| -> String {
        raw.chars().filter(|c| c.is_ascii_digit()).take(2).collect()
    };

    let aria = aria_label.unwrap_or_default();
    let aria_a = aria.clone();
    let aria_b = aria.clone();
    let aria_c = aria.clone();

    let on_input_a = move |ev: web_sys::Event| {
        let v = sanitize(&event_target_value(&ev));
        let len = v.len();
        s_a.set(v);
        if len >= 2 {
            if let Some(el) = ref_b.get() {
                let _ = el.focus();
            }
        }
    };
    let on_input_b = move |ev: web_sys::Event| {
        let v = sanitize(&event_target_value(&ev));
        let len = v.len();
        s_b.set(v);
        if len >= 2 {
            if let Some(el) = ref_c.get() {
                let _ = el.focus();
            }
        }
    };
    let on_input_c = move |ev: web_sys::Event| {
        s_c.set(sanitize(&event_target_value(&ev)));
    };

    let toggle_sign = move |_| {
        sign.update(|s| *s = !*s);
    };

    view! {
        <div class="inline-flex items-center gap-sp-1">
            {move || (mode == CoordMode::DmsSigned).then(|| view! {
                <button
                    type="button"
                    class=move || {
                        let base = "btn btn--sm font-mono font-bold !w-9 !min-w-0 !px-0 !py-1 leading-none";
                        if sign.get() {
                            format!("{base} text-state-ok")
                        } else {
                            format!("{base} text-state-warn")
                        }
                    }
                    on:click=toggle_sign
                    aria-label="toggle sign"
                >{move || if sign.get() { "+" } else { "−" }}</button>
            })}
            <input
                class=FIELD_CLS
                type="text"
                inputmode="numeric"
                maxlength="2"
                placeholder=move || match mode { CoordMode::Hms => "HH", CoordMode::DmsSigned => "DD" }
                aria-label=aria_a
                prop:value=move || s_a.get()
                on:input=on_input_a
            />
            <span class=SEP_CLS>":"</span>
            <input
                class=FIELD_CLS
                type="text"
                inputmode="numeric"
                maxlength="2"
                placeholder="MM"
                aria-label=aria_b
                node_ref=ref_b
                prop:value=move || s_b.get()
                on:input=on_input_b
            />
            <span class=SEP_CLS>":"</span>
            <input
                class=FIELD_CLS
                type="text"
                inputmode="numeric"
                maxlength="2"
                placeholder="SS"
                aria-label=aria_c
                node_ref=ref_c
                prop:value=move || s_c.get()
                on:input=on_input_c
            />
        </div>
    }
}

