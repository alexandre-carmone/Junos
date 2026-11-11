//! The Guide tab's native mode, while junos-guider is attached
//! (`guider_state` in the store): the daemon is driven straight through the
//! relay (`guider_rpc`, `protocol.rs`).
//!
//! Layout as Focus: a header (profile · state badge · alerts · Ekos
//! settings), then on phones one pinned pane — the frame or the drift plot,
//! a toggle remembered apart for guiding (graph by default) and for the rest
//! (frame) — with the numbers, the target plot and the other pane scrolling
//! beneath; from `md` the frame over the drift plot on the left and the
//! cards on the right. The pinned footer follows PHD2's app state.
//!
//! Ekos coexistence: when Ekos runs with its guider set to PHD2 and talks to
//! the daemon (its guide state is not Disconnected), Guide and Stop go
//! through Ekos (`guide_start` / `guide_stop`, "via Ekos") so its own
//! sequence logic stays in charge; the rest is always native. While Ekos'
//! Capture is active, Dither is off (Ekos dithers from its sequence) and
//! Stop takes a second tap, since it aborts the capture.

use leptos::prelude::*;

use crate::compat::{GuideSnapshot, GuiderSignals};
use crate::components::form::{sheet, CARD, CARD_TITLE, FOOTER};
use crate::components::imaging::status_is_active;
use crate::components::sky::persisted;
use crate::components::tab_wheel_icons::tab_icon;
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;
use crate::Tab;

use super::algorithms::algorithms_sheet;
use super::assistant::assistant_sheet;
use super::calibration::{calibration_sheet, CalSheet};
use super::darks::darks_sheet;
use super::equipment::{equipment_card, equipment_sheet, Equip};
use super::native_settings::native_settings_sheet;
use super::pa_tools::pa_tools_sheet;
use super::frame::{frame_pane, zoom, FramePane};
use super::logs::{alert_toast, logs_sheet};
use super::protocol::{Reply, Rpc, Settle, CALIBRATING, GUIDING, LOOPING, LOST_LOCK, PAUSED, SELECTED};
use super::settings::{settings_i64, GuideSettings};
use super::star_profile::star_profile;
use super::tools::tools_sheet;
use super::stats::{compute, stats_tiles};
use super::target::target_plot;
use super::timeline::{drift_plot, DriftSeries, WINDOW_MS};
use super::{tone, Sheet};

const CHIP: &str = "chip h-9 md:h-7 px-3 justify-center cursor-pointer";

/// The footer's buttons.
#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Loop,
    AutoSelect,
    Guide,
    Dither,
    Pause,
    Resume,
    Stop,
}

/// PHD2's app state → the footer, at most three buttons.
fn buttons(app: &str) -> &'static [Btn] {
    use Btn::*;
    match app {
        LOOPING | SELECTED => &[AutoSelect, Stop, Guide],
        CALIBRATING => &[Stop],
        GUIDING | LOST_LOCK => &[Dither, Pause, Stop],
        PAUSED => &[Resume, Stop],
        _ => &[Loop, Guide],
    }
}

fn state_label(app: &str, tr: &'static Translations) -> &'static str {
    match app {
        LOOPING => tr.guider_state_looping,
        SELECTED => tr.guider_state_selected,
        CALIBRATING => tr.guider_state_calibrating,
        GUIDING => tr.guider_state_guiding,
        LOST_LOCK => tr.guider_state_lostlock,
        PAUSED => tr.guider_state_paused,
        _ => tr.guider_state_stopped,
    }
}

#[component]
pub fn NativeGuide(
    guide: Signal<GuideSnapshot>,
    guider: GuiderSignals,
    capture_status: Signal<String>,
    online: Signal<bool>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let rpc = Rpc { send: send.clone(), pending: guider.pending };
    let settle = Settle::load();
    let state = guider.state;

    let app = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.app_state.clone()).unwrap_or_default()));
    let profile = Memo::new(move |_| state.with(|s| s.as_ref().map(|s| s.profile.name.clone()).unwrap_or_default()));
    let pixel_scale = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.pixel_scale)));
    let ra_only = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.dec_guide_mode.clone())).as_deref() == Some("Off"));
    let guiding = Memo::new(move |_| matches!(app.get().as_str(), GUIDING | LOST_LOCK | PAUSED));

    // The latest call; its error shows in the footer until the state moves.
    let last = RwSignal::new(None::<Reply>);
    let note = RwSignal::new(None::<(String, bool)>);
    Effect::new(move |_| {
        if let Some(Some(Err(e))) = last.get().map(|r| r.get()) {
            note.set(Some((e.message, true)));
        }
    });
    Effect::new(move |prev: Option<String>| {
        let now = app.get();
        if prev.is_some_and(|p| p != now) {
            note.set(None);
        }
        now
    });

    // Ekos drives Guide / Stop when its guider is PHD2 and attached.
    let snap = Memo::new(move |_| guide.get());
    let ekos_drives = Memo::new(move |_| {
        online.get()
            && snap.with(|g| {
                settings_i64(&g.options, "guiderType") == Some(1) && !g.status.is_empty() && g.status != "Disconnected"
            })
    });
    let capture_active = Memo::new(move |_| capture_status.with(|s| status_is_active(s)));

    // Drift window and unit (″ needs the pixel scale), the pinned pane.
    let five_min = persisted("guider_drift_5min", false);
    let in_px = persisted("guider_drift_px", false);
    let pin_graph_guiding = persisted("guider_pin_graph_guiding", true);
    let pin_graph_idle = persisted("guider_pin_graph_idle", false);
    let window_ms = Memo::new(move |_| if five_min.get() { 300_000.0 } else { WINDOW_MS });
    let scale = Memo::new(move |_| pixel_scale.get().filter(|_| !in_px.get()));
    let unit = Memo::new(move |_| if scale.get().is_some() { "\u{2033}" } else { " px" });
    let (steps, markers, states) = (guider.steps, guider.markers, guider.states);
    let series = Memo::new(move |_| {
        let (w, k) = (window_ms.get(), scale.get());
        steps.with(|s| markers.with(|m| states.with(|st| DriftSeries::native(s, m, st, w, k))))
    });
    let stats = Memo::new(move |_| {
        let (w, k) = (window_ms.get(), scale.get().unwrap_or(1.0));
        steps.with(|s| compute(s, w, k))
    });
    let accuracy = Memo::new(move |_| {
        scale.get().map(|_| snap.with(|g| g.settings.get("guiderAccuracyThreshold").and_then(|v| v.as_f64()).unwrap_or(1.0)))
    });
    let pin_graph = Memo::new(move |_| if guiding.get() { pin_graph_guiding.get() } else { pin_graph_idle.get() });
    let swap = move || {
        let pin = if guiding.get_untracked() { pin_graph_guiding } else { pin_graph_idle };
        pin.update(|v| *v = !*v);
    };

    // The open sheet; Escape closes it.
    let open = RwSignal::new(None::<Sheet>);
    {
        let cb = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" {
                open.try_set(None);
            }
        });
        if let Some(win) = web_sys::window() {
            use wasm_bindgen::JsCast;
            let _ = win.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        }
        cb.forget();
    }
    // The Guider card's buttons; a tool that runs with its sheet closed
    // shows as active.
    let tool = move |s: Sheet, label: fn(&'static Translations) -> &'static str, busy: Signal<bool>| view! {
        <button class=move || if busy.get() { "btn btn-ghost btn--active h-9" } else { "btn btn-ghost h-9" }
                on:click=move |_| open.set(Some(s))>{move || label(tr())}</button>
    };
    let idle = || Signal::stored(false);
    let busy = move |f: fn(&crate::ws::GuiderStatusData) -> bool| Signal::derive(move || state.with(|s| s.as_ref().is_some_and(f)));
    let darks = guider.darks;
    let equip = Equip {
        state,
        settings: guider.settings,
        profiles: guider.profiles,
        devices: guider.devices,
        rpc: rpc.clone(),
        lang,
        sheet: open,
    };

    let fp = FramePane {
        frame: guider.frame,
        state,
        settle,
        rpc: rpc.clone(),
        last,
        zoom_open: RwSignal::new(false),
        natural: RwSignal::new((0.0, 0.0)),
        pending: RwSignal::new(None),
        lang,
    };

    let chip = move |on: Signal<bool>, label: &'static str, set: Box<dyn Fn() + Send + Sync>| view! {
        <button type="button" class=move || if on.get() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| set()>{label}</button>
    };
    let graph_pane = move |pinned: bool| {
        let cls = if pinned { "shrink-0 h-[40dvh] min-h-[200px] rounded-none md:rounded-lg" } else { "h-[260px]" };
        view! {
            <div class=format!("{CARD} {cls} md:h-auto md:min-h-0 md:col-start-1 md:row-start-2")>
                <div class="flex items-center gap-1.5 flex-wrap">
                    <span class=format!("{CARD_TITLE} flex-1")>{move || tr().guide_drift_title}</span>
                    {chip(Signal::derive(move || !five_min.get()), "2 min", Box::new(move || five_min.set(false)))}
                    {chip(five_min.into(), "5 min", Box::new(move || five_min.set(true)))}
                    <Show when=move || pixel_scale.with(Option::is_some)>
                        {chip(Signal::derive(move || !in_px.get()), "\u{2033}", Box::new(move || in_px.set(false)))}
                        {chip(in_px.into(), "px", Box::new(move || in_px.set(true)))}
                    </Show>
                    {pinned.then(|| view! {
                        <button class=format!("{CHIP} md:hidden") on:click=move |_| swap()>
                            {move || tr().guider_show_frame}
                        </button>
                    })}
                </div>
                {move || series.with(|s| drift_plot(s, "flex-1 min-h-[80px]", tr()))}
            </div>
        }
    };
    let pane = {
        let fp = fp.clone();
        move |graph: bool, pinned: bool| {
            if graph { graph_pane(pinned).into_any() } else { frame_pane(fp.clone(), pinned, swap).into_any() }
        }
    };
    let pane2 = pane.clone();

    // ── Footer ────────────────────────────────────────────────────────────
    let stop_armed = RwSignal::new(false);
    let act = {
        let (rpc, send) = (rpc.clone(), send.clone());
        move |b: Btn| {
            let call = match b {
                Btn::Loop => rpc.start_loop(),
                Btn::AutoSelect => rpc.find_star(),
                Btn::Pause => rpc.set_paused(true),
                Btn::Resume => rpc.set_paused(false),
                Btn::Dither => {
                    let ra_only = state.with_untracked(|s| s.as_ref().is_some_and(|s| s.dither.ra_only));
                    rpc.dither(settle.dither_amount.get_untracked(), ra_only, &settle)
                }
                Btn::Guide if ekos_drives.get_untracked() => return send_cmd(&send, "guide_start", serde_json::json!({})),
                Btn::Guide => rpc.guide(&settle, false),
                Btn::Stop => {
                    if capture_active.get_untracked() && !stop_armed.get_untracked() {
                        stop_armed.set(true);
                        gloo_timers::callback::Timeout::new(4000, move || {
                            stop_armed.try_set(false);
                        })
                        .forget();
                        return;
                    }
                    stop_armed.set(false);
                    if ekos_drives.get_untracked() {
                        return send_cmd(&send, "guide_stop", serde_json::json!({}));
                    }
                    if app.get_untracked() == PAUSED {
                        rpc.set_paused(false);
                    }
                    rpc.stop_capture()
                }
            };
            last.set(Some(call));
        }
    };
    let button = move |b: Btn| {
        let act = act.clone();
        let primary = matches!(b, Btn::Guide) || (b == Btn::Stop && app.get_untracked() != LOOPING && app.get_untracked() != SELECTED);
        let class = match (b, primary) {
            (Btn::Guide, _) => "btn btn-primary h-11 px-5 font-semibold max-md:flex-1",
            (Btn::Stop, true) => "btn btn-danger h-11 px-5 font-semibold max-md:flex-1",
            _ => "btn btn-ghost h-11 px-4 max-md:flex-1",
        };
        let label = move || {
            let tr = tr();
            match b {
                Btn::Loop => tr.guider_btn_loop,
                Btn::AutoSelect => tr.guider_btn_autoselect,
                Btn::Guide => tr.guider_btn_guide,
                Btn::Dither => tr.guider_btn_dither,
                Btn::Pause => tr.guider_btn_pause,
                Btn::Resume => tr.guider_btn_resume,
                Btn::Stop if stop_armed.get() => tr.guider_hint_stop_aborts_capture,
                Btn::Stop => tr.guider_btn_stop,
            }
        };
        let via_ekos = matches!(b, Btn::Guide | Btn::Stop);
        view! {
            <button class=class
                    disabled=move || b == Btn::Dither && capture_active.get()
                    title=move || if b == Btn::Dither && capture_active.get() { tr().guider_hint_capture_active } else { "" }
                    on:click=move |_| act(b)>
                <span class="flex flex-col items-center min-w-0 max-w-full leading-tight">
                    <span class="truncate max-w-full">{label}</span>
                    <Show when=move || via_ekos && ekos_drives.get()>
                        <span class="text-[10px] font-normal opacity-80">{move || tr().guider_via_ekos}</span>
                    </Show>
                </span>
            </button>
        }
    };

    // ── Header badge ──────────────────────────────────────────────────────
    let cal_steps = guider.cal_steps;
    let badge = move || {
        let tr = tr();
        let a = app.get();
        let label = state_label(&a, tr);
        let detail = match a.as_str() {
            CALIBRATING => cal_steps.with(|c| c.last().map(|c| format!(" \u{00b7} {} {}", c.dir, c.step))),
            GUIDING => state.with(|s| {
                s.as_ref().and_then(|s| s.settling.as_ref()).map(|st| {
                    format!(" \u{00b7} {} {:.2} px {:.0}/{:.0} s", tr.guider_settling, st.distance, st.time, st.settle_time)
                })
            }),
            _ => None,
        };
        format!("{label}{}", detail.unwrap_or_default())
    };
    let alerts = guider.alerts;
    let log = guider.log;

    // Ekos' settings sheet stays reachable (its guider choice, PHD2 port).
    let settings = Memo::new(move |_| snap.with(|g| g.settings.clone()));
    let options = Memo::new(move |_| snap.with(|g| g.options.clone()));
    let status = Memo::new(move |_| snap.with(|g| g.status.clone()));
    let send_settings = send.clone();

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-4 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Guide)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().tab_guide}</span>
                <span class="min-w-0 truncate text-sm text-text-muted">{move || profile.get()}</span>
                <Show when=move || alerts.with(|a| !a.is_empty())>
                    <button class="chip h-7 px-2 ml-auto shrink-0 text-state-warn font-mono"
                            title=move || alerts.with(|a| a.first().map(|a| a.msg.clone()).unwrap_or_default())
                            on:click=move |_| open.set(Some(Sheet::Logs))>
                        {move || format!("\u{26a0} {}", alerts.with(Vec::len))}
                    </button>
                </Show>
                <span class=move || format!("{} {} min-w-0 truncate", tone(&app.get()).0,
                                            if alerts.with(Vec::is_empty) { "ml-auto" } else { "" })>
                    {badge}
                </span>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().guide_settings_title
                        on:click=move |_| open.set(Some(Sheet::Ekos))>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Profiles)></span>
                </button>
            </div>

            // Body — a column on phones (one pane pinned), from md a grid:
            // frame over drift on the left, the cards on the right.
            <div class="flex-1 min-h-0 flex flex-col \
                        md:grid md:grid-cols-[minmax(0,1fr)_300px] lg:grid-cols-[minmax(0,1fr)_340px] \
                        md:grid-rows-[minmax(0,1fr)_minmax(160px,30dvh)] md:gap-3 md:p-3 md:pr-6">
                {move || pane(pin_graph.get(), true)}
                <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 \
                            p-4 pb-[max(1rem,env(safe-area-inset-bottom))] md:contents">
                    <div class="flex flex-col gap-3 md:col-start-2 md:row-start-1 md:row-span-2 md:min-h-0 md:overflow-y-auto">
                        <div class=CARD>{stats_tiles(stats, unit, accuracy, lang)}</div>
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().guide_target_title}</span>
                            {move || series.with(|s| target_plot(&s.points, accuracy.get().unwrap_or(1.0), unit.get(),
                                                                ra_only.get(), tr().guide_no_drift))}
                        </div>
                        {star_profile(guider.frame, rpc.clone(), last, pixel_scale, lang)}
                        {equipment_card(equip.clone())}
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().guider_tools_title}</span>
                            <div class="grid grid-cols-2 gap-2">
                                {tool(Sheet::Algorithms, |tr| tr.guider_algorithms_title, idle())}
                                {tool(Sheet::Calibration, |tr| tr.guider_calibration_title, idle())}
                                {tool(Sheet::Settings, |tr| tr.guider_settings_title, idle())}
                                {tool(Sheet::Darks, |tr| tr.guider_darks_title,
                                      Signal::derive(move || darks.with(|d| !d["build"].is_null())))}
                                {tool(Sheet::Assistant, |tr| tr.guider_ga_title, busy(|s| s.ga_running))}
                                {tool(Sheet::PolarTools, |tr| tr.guider_pa_title, busy(|s| s.pa_running))}
                                {tool(Sheet::Logs, |tr| tr.guider_logs_title, idle())}
                                {tool(Sheet::Tools, |tr| tr.guider_tools, Signal::derive(move || guider.tools.with(|t| {
                                    t["starcross"]["running"] == true || !t["pierflip"].is_null() || t["comet"]["training"] == true
                                })))}
                            </div>
                        </div>
                    </div>
                    {move || pane2(!pin_graph.get(), false)}
                </div>
            </div>

            <div class=format!("{FOOTER} px-4 md:pl-4 md:pr-6")>
                {move || match note.get() {
                    Some((text, is_err)) => view! {
                        <span class=format!("flex-1 min-w-0 truncate text-xs {}", if is_err { "text-state-err" } else { "text-text-muted" })
                              title=text.clone()>{text.clone()}</span>
                    }.into_any(),
                    None => view! {
                        <span class="max-md:hidden flex-1 min-w-0 truncate font-mono text-xs text-text-muted">
                            {move || log.with(|l| l.first().cloned().unwrap_or_default())}
                        </span>
                    }.into_any(),
                }}
                {move || buttons(&app.get()).iter().map(|b| button(*b)).collect_view()}
            </div>

            {alert_toast(alerts)}
            {zoom(fp.clone())}
            {move || {
                let close = move || open.set(None);
                match open.get()? {
                    Sheet::Ekos => Some(sheet(move || tr().guide_settings_title, close, view! {
                        <GuideSettings settings=settings options=options status=status
                                       send=send_settings.clone() lang=lang />
                    }).into_any()),
                    Sheet::Equipment => Some(sheet(move || tr().guider_equipment_title, close,
                                                   equipment_sheet(equip.clone())).into_any()),
                    Sheet::Algorithms => Some(sheet(move || tr().guider_algorithms_title, close,
                                                    algorithms_sheet(state, guider.settings, rpc.clone(), lang)).into_any()),
                    Sheet::Calibration => Some(sheet(move || tr().guider_calibration_title, close, calibration_sheet(CalSheet {
                        state,
                        calibration: guider.calibration,
                        cal_steps: guider.cal_steps,
                        settings: guider.settings,
                        tools: guider.tools,
                        rpc: rpc.clone(),
                        settle,
                        lang,
                    })).into_any()),
                    Sheet::Settings => Some(sheet(move || tr().guider_settings_title, close,
                                                  native_settings_sheet(state, guider.settings, guider.tools, rpc.clone(), settle, lang)).into_any()),
                    Sheet::Darks => Some(sheet(move || tr().guider_darks_title, close,
                                               darks_sheet(state, darks, guider.settings, rpc.clone(), lang)).into_any()),
                    Sheet::Assistant => Some(sheet(move || tr().guider_ga_title, close,
                                                   assistant_sheet(state, guider.ga, rpc.clone(), lang)).into_any()),
                    Sheet::PolarTools => Some(sheet(move || tr().guider_pa_title, close,
                                                    pa_tools_sheet(guider.pa, guider.settings, rpc.clone(), lang)).into_any()),
                    Sheet::Logs => Some(sheet(move || tr().guider_logs_title, close,
                                              logs_sheet(guider.alerts, rpc.clone(), lang)).into_any()),
                    Sheet::Tools => Some(sheet(move || tr().guider_tools, close,
                                               tools_sheet(guider.tools, rpc.clone(), lang)).into_any()),
                }
            }}
        </div>
    }
}

