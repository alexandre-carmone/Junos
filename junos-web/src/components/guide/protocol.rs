//! junos-guider's side of the Guide tab: the `guider_rpc` calls the native
//! mode makes (PHD2's event-server methods and the daemon's extras,
//! `junos-guider/src/server/rpc.rs::METHODS`), PHD2's app states and the
//! profile keys the tab touches, the settle / dither defaults the browser
//! keeps (PHD2 has no key for them), and the guide algorithms with their
//! parameters as PHD2 2.6.14's dialogs bound them.
// The keys, the setters and the algorithm table are declared ahead of the
// settings and algorithms sheets that use them.
#![allow(dead_code)]

use std::sync::Arc;

use leptos::prelude::*;
use serde_json::{json, Map, Value};

use crate::components::sky::persisted;
use crate::i18n::Lang;
use crate::ws::{GuiderPending, GuiderReply, SendCmd};
use crate::ws_helpers::guider_rpc;

use super::rows::Rows;

// ── PHD2 app states (`get_app_state`, `guider_state.app_state`) ──────────
pub const STOPPED: &str = "Stopped";
pub const SELECTED: &str = "Selected";
pub const CALIBRATING: &str = "Calibrating";
pub const GUIDING: &str = "Guiding";
pub const LOST_LOCK: &str = "LostLock";
pub const PAUSED: &str = "Paused";
pub const LOOPING: &str = "Looping";

// ── Profile keys (PHD2's names, `guider_settings`) ───────────────────────
pub const KEY_RA_ALGORITHM: &str = "/scope/XGuideAlgorithm";
pub const KEY_DEC_ALGORITHM: &str = "/scope/YGuideAlgorithm";
pub const KEY_AO_X_ALGORITHM: &str = "/stepguider/XGuideAlgorithm";
pub const KEY_AO_Y_ALGORITHM: &str = "/stepguider/YGuideAlgorithm";
pub const KEY_MAX_RA_DURATION: &str = "/scope/MaxRaDuration";
pub const KEY_MAX_DEC_DURATION: &str = "/scope/MaxDecDuration";
pub const KEY_DEC_GUIDE_MODE: &str = "/scope/DecGuideMode";
pub const KEY_CAL_DURATION: &str = "/scope/CalibrationDuration";
pub const KEY_CAL_DISTANCE: &str = "/scope/CalibrationDistance";
pub const KEY_FOCAL_LENGTH: &str = "/frame/focalLength";
pub const KEY_PIXEL_SIZE: &str = "/camera/pixelsize";
pub const KEY_DITHER_MODE: &str = "/DitherMode";
pub const KEY_DITHER_RA_ONLY: &str = "/DitherRaOnly";
pub const KEY_DITHER_SCALE: &str = "/DitherScaleFactor";

/// A call's reply, `Some` once it lands (`ws_helpers::guider_rpc`).
pub type Reply = RwSignal<Option<GuiderReply>>;

/// The error of the latest call kept in `last`, once its reply lands.
pub fn call_error(last: RwSignal<Option<Reply>>) -> Option<String> {
    last.get().and_then(|r| r.get()).and_then(Result::err).map(|e| e.message)
}

/// Calls whose errors go to `err`. A reply is dropped once it lands, so an
/// old failure doesn't show again after a later call.
pub fn error_sink(err: RwSignal<Option<String>>) -> RwSignal<Vec<Reply>> {
    let calls = RwSignal::new(Vec::<Reply>::new());
    Effect::new(move |_| {
        if calls.with(|c| c.iter().any(|r| r.with(Option::is_some))) {
            calls.update(|c| {
                c.retain(|r| match r.get_untracked() {
                    Some(Err(e)) => {
                        err.set(Some(e.message));
                        false
                    }
                    Some(Ok(_)) => false,
                    None => true,
                })
            });
        }
    });
    calls
}

/// The calls to junos-guider, through the relay.
#[derive(Clone)]
pub struct Rpc {
    pub send: SendCmd,
    pub pending: GuiderPending,
}

impl Rpc {
    pub fn call(&self, method: &str, params: Value) -> Reply {
        guider_rpc(&self.send, self.pending, method, params)
    }

    /// `loop`: expose continuously (and stop guiding, if it was).
    pub fn start_loop(&self) -> Reply {
        self.call("loop", json!({}))
    }

    pub fn stop_capture(&self) -> Reply {
        self.call("stop_capture", json!({}))
    }

    /// Auto-select a star.
    pub fn find_star(&self) -> Reply {
        self.call("find_star", json!({}))
    }

    pub fn deselect_star(&self) -> Reply {
        self.call("deselect_star", json!({}))
    }

    /// Guide, calibrating first when needed; `SettleDone` ends it.
    pub fn guide(&self, settle: &Settle, recalibrate: bool) -> Reply {
        self.call("guide", json!({ "settle": settle.json(), "recalibrate": recalibrate }))
    }

    pub fn dither(&self, amount: f64, ra_only: bool, settle: &Settle) -> Reply {
        self.call("dither", json!({ "amount": amount, "raOnly": ra_only, "settle": settle.json() }))
    }

    /// Pause the guide pulses (frames go on), or resume.
    pub fn set_paused(&self, paused: bool) -> Reply {
        self.call("set_paused", json!({ "paused": paused }))
    }

    /// Frame pixels; `exact: false` locks on the star found near (x, y).
    pub fn set_lock_position(&self, x: f64, y: f64, exact: bool) -> Reply {
        self.call("set_lock_position", json!({ "x": x, "y": y, "exact": exact }))
    }

    pub fn set_exposure(&self, ms: i64) -> Reply {
        self.call("set_exposure", json!({ "exposure": ms }))
    }

    /// `Off | Auto | North | South`.
    pub fn set_dec_guide_mode(&self, mode: &str) -> Reply {
        self.call("set_dec_guide_mode", json!({ "mode": mode }))
    }

    /// Any profile key; the daemon echoes `guider_settings` whole.
    pub fn set_profile_value(&self, key: &str, value: Value) -> Reply {
        self.call("set_profile_value", json!({ "key": key, "value": value }))
    }

    /// `axis`: `ra | dec` (an AO's X / Y once one is connected, as PHD2).
    pub fn set_algo_param(&self, axis: &str, name: &str, value: f64) -> Reply {
        self.call("set_algo_param", json!({ "axis": axis, "name": name, "value": value }))
    }

    pub fn set_algorithm(&self, axis: &str, kind: i64) -> Reply {
        self.call("set_algorithm", json!({ "axis": axis, "algorithm": kind }))
    }

    pub fn reset_algo_params(&self, axis: &str) -> Reply {
        self.call("reset_algo_params", json!({ "axis": axis }))
    }
}

/// Two numbers within rounding, or two equal values.
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
        _ => a == b,
    }
}

/// An edit on its way: shown until the echo carries it.
#[derive(Clone)]
struct Edit {
    key: &'static str,
    value: Value,
    reply: Reply,
    /// The reply was already `Ok` at an earlier look: the next settings
    /// change ends the edit, matching or not (the daemon may round it).
    answered: bool,
}

/// Rows on junos-guider's profile (`guider_settings`, PHD2's keys).
/// `defaults` stand for the keys the profile never stored (PHD2's
/// defaults); an edit goes out through `write` and stays shown until the
/// echo carries it, or its call fails — `err` then takes the message.
pub fn profile_rows(
    settings: RwSignal<Value>,
    defaults: Vec<(&'static str, Value)>,
    lang: RwSignal<Lang>,
    err: RwSignal<Option<String>>,
    write: impl Fn(&'static str, Value) -> Reply + Send + Sync + 'static,
) -> Rows {
    let edits = RwSignal::new(Vec::<Edit>::new());
    Effect::new(move |_| {
        let s = settings.get();
        let mut changed = false;
        let mut next = edits.get();
        next.retain_mut(|e| match e.reply.get() {
            Some(Err(x)) => {
                err.set(Some(x.message));
                changed = true;
                false
            }
            _ if s.get(e.key).is_some_and(|v| same(v, &e.value)) => {
                changed = true;
                false
            }
            Some(Ok(_)) if e.answered => {
                changed = true;
                false
            }
            Some(Ok(_)) => {
                e.answered = true;
                true
            }
            None => true,
        });
        if changed {
            edits.set(next);
        } else {
            // `answered` flags only: no redraw.
            edits.update_untracked(|e| *e = next);
        }
    });
    let values = Memo::new(move |_| {
        let mut m: Map<String, Value> = settings.with(|s| s.as_object().cloned().unwrap_or_default());
        for (k, v) in &defaults {
            m.entry(k.to_string()).or_insert_with(|| v.clone());
        }
        edits.with(|e| {
            for e in e {
                m.insert(e.key.to_string(), e.value.clone());
            }
        });
        Value::Object(m)
    });
    let set = move |key: &'static str, value: Value| {
        err.set(None);
        let reply = write(key, value.clone());
        edits.update(|e| {
            e.retain(|e| e.key != key);
            e.push(Edit { key, value, reply, answered: false });
        });
    };
    Rows { values: values.into(), set: Arc::new(set), lang }
}

/// A destructive button: the first tap arms it for 4 s, the second (true)
/// runs it.
pub fn second_tap(armed: RwSignal<bool>) -> bool {
    if armed.get_untracked() {
        armed.set(false);
        return true;
    }
    armed.set(true);
    gloo_timers::callback::Timeout::new(4000, move || {
        armed.try_set(false);
    })
    .forget();
    false
}

/// The settle block the tab's Guide and Dither send, and the dither
/// amount: browser-side (localStorage), since PHD2 keeps none of them —
/// each client brings its own.
#[derive(Clone, Copy)]
pub struct Settle {
    /// Settled within this distance of the lock, px.
    pub pixels: RwSignal<f64>,
    /// … for this long, s.
    pub time: RwSignal<f64>,
    /// Give up after, s.
    pub timeout: RwSignal<f64>,
    /// Dither amount, px.
    pub dither_amount: RwSignal<f64>,
}

impl Settle {
    pub fn load() -> Self {
        Self {
            pixels: persisted("guider_settle_px", 1.5),
            time: persisted("guider_settle_time", 10.0),
            timeout: persisted("guider_settle_timeout", 60.0),
            dither_amount: persisted("guider_dither_amount", 3.0),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "pixels": self.pixels.get_untracked(),
            "time": self.time.get_untracked(),
            "timeout": self.timeout.get_untracked(),
        })
    }
}

// ── Guide algorithms (report_0 §0 and §4; PHD2's dialog spinners) ─────────

/// One parameter of an algorithm.
pub struct AlgoParam {
    /// Its `set_algo_param` / `algo_params` name, when the event server
    /// knows it; else it is a profile key only.
    pub rpc: Option<&'static str>,
    /// Its profile key, under `/<scope|stepguider>/GuideAlgorithm/<X|Y>/<class>/`.
    pub key: &'static str,
    pub default: f64,
    /// PHD2's spinner, in UI units (the value × `scale`).
    pub min: f64,
    pub max: f64,
    pub step: f64,
    /// 100 where PHD2 shows a 0..1 fraction as a percentage.
    pub scale: f64,
    pub unit: &'static str,
    /// A checkbox (0 / 1).
    pub flag: bool,
}

const fn num(rpc: Option<&'static str>, key: &'static str, default: f64, min: f64, max: f64, step: f64, scale: f64, unit: &'static str) -> AlgoParam {
    AlgoParam { rpc, key, default, min, max, step, scale, unit, flag: false }
}

const fn flag(rpc: Option<&'static str>, key: &'static str, default: bool) -> AlgoParam {
    AlgoParam { rpc, key, default: if default { 1.0 } else { 0.0 }, min: 0.0, max: 1.0, step: 1.0, scale: 1.0, unit: "", flag: true }
}

/// The min-move every algorithm but Predictive PEC has (default 0.2 px,
/// ZFilter's 0.1; reset to the smart default, `GuideAlgorithm::SmartDefaultMinMove`).
const fn min_move(default: f64, step: f64) -> AlgoParam {
    num(Some("minMove"), "minMove", default, 0.0, 20.0, step, 1.0, "px")
}

/// A guide algorithm: PHD2's enum value (`/scope/XGuideAlgorithm`), its
/// class name (`algorithmName`, the profile group) and its UI name.
pub struct Algo {
    pub kind: i64,
    pub class: &'static str,
    pub name: &'static str,
    pub params: &'static [AlgoParam],
}

pub const ALGOS: &[Algo] = &[
    Algo { kind: 0, class: "Identity", name: "None", params: &[] },
    Algo {
        kind: 1,
        class: "Hysteresis",
        name: "Hysteresis",
        params: &[
            num(Some("hysteresis"), "hysteresis", 0.1, 0.0, 99.0, 1.0, 100.0, "%"),
            num(Some("aggression"), "aggression", 0.7, 0.0, 200.0, 1.0, 100.0, "%"),
            min_move(0.2, 0.01),
        ],
    },
    Algo {
        kind: 2,
        class: "Lowpass",
        name: "Lowpass",
        params: &[num(Some("slopeWeight"), "SlopeWeight", 5.0, 0.0, 20.0, 0.5, 1.0, ""), min_move(0.2, 0.01)],
    },
    Algo {
        kind: 3,
        class: "Lowpass2",
        name: "Lowpass2",
        params: &[num(Some("aggressiveness"), "Aggressiveness", 80.0, 0.0, 100.0, 1.0, 1.0, "%"), min_move(0.2, 0.01)],
    },
    Algo {
        kind: 4,
        class: "ResistSwitch",
        name: "Resist Switch",
        params: &[
            num(Some("aggression"), "aggression", 1.0, 1.0, 100.0, 1.0, 100.0, "%"),
            min_move(0.2, 0.01),
            flag(Some("fastSwitch"), "fastSwitch", true),
        ],
    },
    Algo {
        kind: 5,
        class: "Predictive PEC",
        name: "Predictive PEC",
        params: &[
            num(Some("predictiveWeight"), "gp_prediction_gain", 0.5, 0.0, 100.0, 1.0, 100.0, "%"),
            num(Some("reactiveWeight"), "gp_control_gain", 0.6, 0.0, 100.0, 1.0, 100.0, "%"),
            num(Some("minMove"), "gp_min_move", 0.2, 0.0, 5.0, 0.01, 1.0, "px"),
            num(Some("periodLength"), "gp_period_per_kern", 200.0, 10.0, 2000.0, 1.0, 1.0, "s"),
            flag(None, "gp_compute_period", true),
            // The expert dialog.
            num(None, "gp_points_for_approximation", 100.0, 0.0, 2000.0, 1.0, 1.0, ""),
            num(None, "gp_period_lengths_inference", 2.0, 0.0, 10.0, 0.1, 1.0, ""),
            num(None, "gp_period_lengths_period_estimation", 2.0, 0.0, 10.0, 0.1, 1.0, ""),
            num(None, "gp_length_scale_se0_kern", 700.0, 100.0, 5000.0, 10.0, 1.0, "s"),
            num(None, "gp_sigvar_se0_kern", 20.0, 0.0, 100.0, 1.0, 1.0, "px"),
            num(None, "gp_length_scale_per_kern", 10.0, 1.0, 50.0, 5.0, 1.0, "s"),
            num(None, "gp_sigvar_per_kern", 20.0, 0.0, 100.0, 0.1, 1.0, "px"),
            num(None, "gp_length_scale_se1_kern", 25.0, 1.0, 100.0, 1.0, 1.0, "s"),
            num(None, "gp_sigvar_se1_kern", 10.0, 0.0, 100.0, 0.1, 1.0, "px"),
            num(None, "noreset_max_pct_period", 40.0, 0.0, 80.0, 1.0, 1.0, "%"),
        ],
    },
    Algo {
        kind: 6,
        class: "ZFilter",
        name: "ZFilter",
        params: &[num(Some("expFactor"), "expFactor", 2.0, 1.0, 20.0, 1.0, 1.0, ""), min_move(0.1, 0.05)],
    },
];

/// The algorithms each axis offers (`mount.cpp:227-240`): Predictive PEC is
/// RA only, an AO has neither it nor Resist Switch.
pub const RA_ALGOS: &[i64] = &[1, 2, 3, 4, 5, 6];
pub const DEC_ALGOS: &[i64] = &[1, 2, 3, 4, 6];
pub const AO_ALGOS: &[i64] = &[1, 2, 3, 6];

/// The algorithm of an enum value or a class name (`algo_params.*.algorithmName`).
pub fn algo(kind: i64) -> Option<&'static Algo> {
    ALGOS.iter().find(|a| a.kind == kind)
}

pub fn algo_by_class(class: &str) -> Option<&'static Algo> {
    ALGOS.iter().find(|a| a.class == class)
}
