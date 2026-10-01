//! Bottom time bar: shows the simulated local date/time and steps, plays
//! (time-lapse at one step per second) or jumps it — date and hour pickers,
//! Now, and the current night's astronomical dusk / dawn.

use leptos::prelude::*;

use crate::compat::SiteSnapshot;
use crate::dom::event_target_value;
use crate::i18n::{Lang, t};

use super::clock::{self, SkyClock};

/// (seconds, label). One tap moves one step; playback runs one step per second.
const STEPS: [(f64, &str); 4] = [(60.0, "1m"), (600.0, "10m"), (3600.0, "1h"), (86_400.0, "1d")];

const BTN: &str = "btn-icon shrink-0 text-text-blue text-lg md:w-7 md:h-7 md:min-w-[28px] md:min-h-[28px]";
const CHIP: &str = "chip shrink-0 min-w-0 h-8 md:h-6 cursor-pointer font-mono disabled:opacity-50 disabled:cursor-not-allowed";
const INPUT: &str = "input input--sm font-mono w-full min-w-0 max-md:h-9";

fn date(ms: f64) -> js_sys::Date {
    js_sys::Date::new(&ms.into())
}

fn hhmm(d: &js_sys::Date) -> String {
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

/// "Thu 3 Oct" / "jeu. 3 oct." via the browser's Intl.
fn day_label(d: &js_sys::Date, lang: Lang) -> String {
    let opts = js_sys::Object::new();
    for (k, v) in [("weekday", "short"), ("day", "numeric"), ("month", "short")] {
        let _ = js_sys::Reflect::set(&opts, &k.into(), &v.into());
    }
    let locale = match lang { Lang::En => "en-GB", Lang::Fr => "fr-FR" };
    d.to_locale_date_string(locale, &opts).into()
}

/// "+2h15m", "-3d04h", "+5m".
fn fmt_offset(s: f64) -> String {
    let sign = if s < 0.0 { '-' } else { '+' };
    let m = (s.abs() / 60.0).round() as i64;
    let (d, h, mm) = (m / 1440, (m / 60) % 24, m % 60);
    if d > 0 {
        format!("{sign}{d}d{h:02}h")
    } else if h > 0 {
        format!("{sign}{h}h{mm:02}m")
    } else {
        format!("{sign}{mm}m")
    }
}

#[component]
pub fn TimeBar(
    clock: RwSignal<SkyClock>,
    /// Render tick — drives the displayed time while the clock runs.
    tick: ReadSignal<u32>,
    #[prop(into)] site: Signal<SiteSnapshot>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let expanded = RwSignal::new(
        web_sys::window()
            .and_then(|w| w.inner_width().ok())
            .and_then(|v| v.as_f64())
            .is_some_and(|w| w >= 768.0),
    );
    let step = RwSignal::new(2_usize); // 1h

    // Simulated time to the minute: the DOM only updates when the displayed
    // minute changes, not on every render tick.
    let sim_ms = Memo::new(move |_| {
        let _ = tick.get();
        (clock.get().now_ms() / 60_000.0).floor() * 60_000.0
    });
    let live = move || clock.get().is_live();
    let playing = move || clock.get().rate != 1.0;

    let night = Memo::new(move |_| clock::night_start(sim_ms.get()));
    let twilights = Memo::new(move |_| {
        let s = site.get();
        clock::night_twilights(night.get(), s.latitude, s.longitude)
    });

    let jump = move |ms: f64| clock.update(|c| *c = c.at(ms));
    let step_by = move |dir: f64| {
        let ms = clock.get_untracked().now_ms() + dir * STEPS[step.get_untracked()].0 * 1000.0;
        jump(ms);
    };
    let set_step = move |i: usize| {
        step.set(i);
        if clock.get_untracked().rate != 1.0 {
            clock.update(|c| *c = c.with_rate(STEPS[i].0));
        }
    };
    let toggle_play = move |_| {
        let rate = if playing() { 1.0 } else { STEPS[step.get_untracked()].0 };
        clock.update(|c| *c = c.with_rate(rate));
    };

    let twilight_btn = move |dusk: bool| {
        let at = move || {
            let (dusk_ms, dawn_ms) = twilights.get();
            if dusk { dusk_ms } else { dawn_ms }
        };
        view! {
            <button class=format!("{CHIP} justify-center")
                    prop:disabled=move || at().is_none()
                    title=move || if at().is_none() { tr().time_no_night } else { "" }
                    on:click=move |_| if let Some(ms) = at() { jump(ms) }>
                {move || {
                    let name = if dusk { tr().time_dusk } else { tr().time_dawn };
                    match at() {
                        Some(ms) => format!("{name} {}", hhmm(&date(ms))),
                        None => name.to_string(),
                    }
                }}
            </button>
        }
    };

    view! {
        <div class=move || {
                 let base = "panel-glass panel-glass--strong pointer-events-auto w-full md:w-[360px] p-1.5 \
                             flex flex-col gap-1.5 font-mono text-sm text-text-blue select-none";
                 if live() { base.to_string() } else { format!("{base} border-accent-amber") }
             }
             on:click=move |ev| ev.stop_propagation()>

            <Show when=move || expanded.get()>
                <div class="flex items-center gap-1 flex-wrap">
                    {STEPS.iter().enumerate().map(|(i, (_, label))| view! {
                        <button class=move || if step.get() == i { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                                on:click=move |_| set_step(i)>
                            {*label}
                        </button>
                    }).collect_view()}
                    <Show when=move || !live()>
                        <button class=format!("{CHIP} ml-auto text-accent-amber")
                                on:click=move |_| clock.set(SkyClock::live())>
                            {move || tr().now}
                        </button>
                    </Show>
                </div>
                <div class="grid grid-cols-2 gap-1">
                    <input type="date" class=INPUT title=move || tr().time_date
                           prop:value=move || {
                               let d = date(sim_ms.get());
                               format!("{:04}-{:02}-{:02}", d.get_full_year(), d.get_month() + 1, d.get_date())
                           }
                           on:change=move |ev| {
                               let p: Vec<u32> = event_target_value(&ev).split('-').filter_map(|x| x.parse().ok()).collect();
                               if let &[y, mo, d] = p.as_slice() {
                                   jump(clock::on_date(clock.get_untracked().now_ms(), y, mo, d));
                               }
                           } />
                    <input type="time" class=INPUT title=move || tr().time_hour
                           prop:value=move || hhmm(&date(sim_ms.get()))
                           on:change=move |ev| {
                               let p: Vec<u32> = event_target_value(&ev).split(':').filter_map(|x| x.parse().ok()).collect();
                               if let &[h, m, ..] = p.as_slice() {
                                   jump(clock::night_hour(clock.get_untracked().now_ms(), h, m));
                               }
                           } />
                    {twilight_btn(true)}
                    {twilight_btn(false)}
                </div>
            </Show>

            <div class="flex items-center gap-1">
                <button class=BTN title=move || tr().time_back on:click=move |_| step_by(-1.0)>"‹"</button>
                <button class="flex-1 min-w-0 min-h-0 h-9 md:h-7 px-2 flex items-center justify-center gap-2 \
                               bg-transparent border-0 rounded-md hover:bg-bg-elev-3 whitespace-nowrap overflow-hidden"
                        on:click=move |_| expanded.update(|v| *v = !*v)>
                    <span class="text-text-blue-bright font-semibold">{move || hhmm(&date(sim_ms.get()))}</span>
                    <span class="text-text-muted truncate">{move || day_label(&date(sim_ms.get()), lang.get())}</span>
                    {move || if live() {
                        view! { <span class="text-accent-green">{format!("● {}", tr().time_live)}</span> }.into_any()
                    } else {
                        let _ = sim_ms.get();
                        view! { <span class="text-accent-amber">{fmt_offset(clock.get_untracked().offset_s())}</span> }.into_any()
                    }}
                </button>
                <button class=BTN title=move || tr().time_forward on:click=move |_| step_by(1.0)>"›"</button>
                <button class=move || if playing() { format!("{BTN} btn--active") } else { BTN.to_string() }
                        title=move || if playing() { tr().time_pause } else { tr().time_play }
                        on:click=toggle_play>
                    <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor">
                        {move || if playing() {
                            view! { <path d="M6 5h4v14H6zM14 5h4v14h-4z" /> }.into_any()
                        } else {
                            view! { <path d="M3 6v12l8.5-6zM12.5 6v12L21 12z" /> }.into_any()
                        }}
                    </svg>
                </button>
            </div>
        </div>
    }
}
