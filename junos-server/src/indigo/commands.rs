//! Browser → INDIGO: the Ekos Live commands the backend answers itself.
//!
//! Each handler returns what to send where. Replies mimic the shapes KStars
//! would produce (`kstars/ekos/ekoslive/message.cpp`), because the WASM
//! client parses them in `ws/store.rs::apply_ekos_event` and must not know
//! which backend it talks to. Commands the INDIGO backend can't honour get a
//! `device_message` so the user sees why nothing happened — except the ones
//! the client polls every few seconds, which are swallowed.

use std::time::Instant;

use serde_json::{json, Value};
use tracing::debug;

use super::agents::{AgentSet, Agents, Kind as AgentKind};
use super::align::{Align, Rig};
use super::alias;
use super::capture::{Capture, FrameType, Job};
use super::focus::Focus;
use super::guide::Guide;
use super::imaging::Imaging;
use super::mount;
use super::protocol::{interface, new_vector_msg, Kind, PropertyStore};
use super::roles::Roles;
use super::scansexa;
use super::scheduler::{Drivers, SchedCtx, SchedReaction, Scheduler};

/// What the backend knows besides the property mirror.
pub struct Ctx<'a> {
    pub store: &'a PropertyStore,
    pub roles: &'a Roles,
    /// Resolved captures dir — the browser's default save folder.
    pub captures_dir: &'a str,
    /// The relay's `$HOME` — what KStars answers to `file_default_path`,
    /// and what `scheduler_save_sequence_file` paths are relative to.
    pub home: &'a str,
    /// The INDIGO server, for the synthetic profile's host/port.
    pub indigo_host: &'a str,
    pub indigo_port: u16,
}

pub enum Out {
    ToBrowser(String),
    ToIndigo(String),
}

/// The per-session module state the commands act on.
pub struct Modules {
    pub imaging: Imaging,
    pub capture: Capture,
    pub focus: Focus,
    pub guide: Guide,
    pub align: Align,
    pub agents: Agents,
    pub scheduler: Scheduler,
}

impl Modules {
    pub fn new(now: Instant) -> Self {
        Self {
            imaging: Imaging::default(),
            capture: Capture::default(),
            focus: Focus::default(),
            guide: Guide::new(),
            align: Align::default(),
            agents: Agents::new(now),
            scheduler: Scheduler::default(),
        }
    }
}

/// What a solve or a polar alignment needs besides the solver.
pub fn rig<'a>(set: &AgentSet, store: &PropertyStore, roles: &'a Roles) -> Rig<'a> {
    let holds = |agent: Option<&str>, list: &str| agent.is_some_and(|a| AgentSet::selected(store, a, list).is_some());
    Rig {
        mount_ready: holds(set.mount.as_deref(), "FILTER_MOUNT_LIST"),
        camera_ready: holds(set.imager.as_deref(), "FILTER_CCD_LIST"),
        mount: roles.mount.as_deref(),
    }
}

/// The synthetic Ekos profile that stands in for "which INDIGO server".
pub const PROFILE_NAME: &str = "INDIGO";

/// Commands the client re-sends on a timer (`ws/mod.rs::BOOTSTRAP_CMDS` and
/// `ws/retry.rs`). Unsupported ones are dropped silently rather than nagging.
const POLLED: &[&str] = &[
    "get_states",
    "livestacker_get_all_settings",
    "get_drivers",
];

pub fn handle(cmd: &Value, ctx: &Ctx, m: &mut Modules) -> Vec<Out> {
    let Some(ty) = cmd.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let payload = cmd.get("payload").cloned().unwrap_or(Value::Null);
    let reply = |ty: &str, payload: Value| vec![Out::ToBrowser(reply_msg(ty, payload))];

    match ty {
        // ── Devices tab ──────────────────────────────────────────────────
        "get_devices" => reply("get_devices", devices_payload(ctx.store)),
        "device_get" => {
            let Some(device) = payload.get("device").and_then(Value::as_str) else {
                return Vec::new();
            };
            let properties: Vec<Value> = ctx
                .store
                .properties(device)
                .filter_map(super::indi_shape::full)
                .collect();
            reply("device_get", json!({ "device": device, "properties": properties }))
        }
        "device_property_get" => {
            let (Some(device), Some(name)) = (
                payload.get("device").and_then(Value::as_str),
                payload.get("property").and_then(Value::as_str),
            ) else {
                return Vec::new();
            };
            let compact = payload.get("compact").and_then(Value::as_bool).unwrap_or(false);
            // The INDI name the client knows first (an alias also renames
            // the elements, even when INDIGO kept the property's name — e.g.
            // `CCD_COOLER {ON}` vs INDI's `{COOLER_ON}`); else INDIGO's own.
            let aliased = alias::lookup(ctx.store, device, name);
            let prop = aliased.as_ref().or_else(|| ctx.store.get(device, name));
            match prop {
                Some(p) => {
                    let shaped = if compact {
                        super::indi_shape::compact(p)
                    } else {
                        super::indi_shape::full(p)
                    };
                    shaped.map(|v| reply("device_property_get", v)).unwrap_or_default()
                }
                // Like KStars (`processDeviceCommands`): no such property, no
                // reply — the client's retry loop handles it.
                None => Vec::new(),
            }
        }
        // Every INDIGO property is pushed to every client; nothing to record.
        "device_property_subscribe" | "device_property_unsubscribe" => Vec::new(),
        "device_property_set" => property_set(&payload, ctx.store).into_iter().collect(),

        // ── Session / profiles ───────────────────────────────────────────
        "get_profiles" => reply("get_profiles", profiles_payload(ctx)),
        // Starting the profile fills the agents' device lists (which connects
        // the devices) and connects the rest; stopping releases everything.
        "profile_start" | "profile_stop" => {
            let mut out = Vec::new();
            // The queue's preparation phases aren't Busy on the agent, so the
            // plan's abort wouldn't reach them.
            if ty == "profile_stop" {
                if m.scheduler.is_running() {
                    let (sctx, scheduler, mut drivers) = sched(ctx, m);
                    let r = scheduler.stop(&sctx, &mut drivers);
                    out.extend(sched_outs(r));
                }
                if m.capture.is_running() {
                    if let Some(agent) = m.agents.set.imager.clone() {
                        out.extend(outs(m.capture.stop(&agent)));
                    }
                }
            }
            m.agents.set_active(ty == "profile_start");
            let plan = m.agents.plan(ctx.store, ctx.roles, Instant::now());
            out.extend(plan.to_indigo.into_iter().map(Out::ToIndigo));
            out.extend(plan.to_browser.into_iter().map(Out::ToBrowser));
            out
        }
        // KStars answers its home dir (`KSPaths::writableLocation`, type 8);
        // the captures dir reaches the browser through `/api/config`.
        "file_default_path" => reply("file_default_path", json!(ctx.home)),
        "file_directory_operation" => {
            let op = payload.get("operation").and_then(Value::as_str).unwrap_or("");
            let path = payload.get("path").and_then(Value::as_str).unwrap_or("");
            let result = match op {
                "create" if !path.is_empty() => std::fs::create_dir_all(path).is_ok(),
                "exists" => std::path::Path::new(path).is_dir(),
                _ => return reply("device_message", unsupported_payload(&format!("file_directory_operation {op}"))),
            };
            reply("file_directory_operation", json!({ "result": result, "operation": op }))
        }
        "astro_get_location" => site_payload(ctx.store)
            .map(|p| reply("astro_get_location", p))
            .unwrap_or_default(),

        // ── Trains / scopes: one synthetic train over the device roles ───
        "get_scopes" => reply("get_scopes", ctx.roles.scopes_payload(ctx.store)),
        "train_get_all" => reply("train_get_all", ctx.roles.train_payload()),
        "train_get_profiles" => reply("train_get_profiles", ctx.roles.train_profiles_payload()),
        "train_set" => Vec::new(),

        // ── Mount tab ────────────────────────────────────────────────────
        "mount_get_all_settings" | "mount_set_all_settings" => reply(
            "mount_get_all_settings",
            json!({ "executeMeridianFlip": false, "meridianFlipOffsetDegrees": 0.0 }),
        ),
        "mount_park" | "mount_unpark" | "mount_set_tracking" | "mount_set_slew_rate"
        | "mount_set_motion" | "mount_abort" | "mount_goto_rade" | "mount_sync_rade" => {
            let Some(device) = ctx.roles.mount.as_deref() else {
                return reply("device_message", json!({ "device": PROFILE_NAME, "message": "No mount found on the INDIGO server" }));
            };
            mount_command(ty, &payload, device, ctx.store)
                .into_iter()
                .map(Out::ToIndigo)
                .collect()
        }
        "mount_goto_target" => reply("device_message", unsupported_payload(ty)),

        // ── Imaging tab: one-shot preview / loop, and the capture queue ──
        "capture_get_all_settings" => reply("capture_get_all_settings", capture_settings(ctx, m)),
        "capture_set_all_settings" => {
            let mut out: Vec<Out> = payload
                .as_object()
                .into_iter()
                .flat_map(|map| map.iter())
                .flat_map(|(k, v)| {
                    if m.capture.apply_setting(k, v) {
                        Vec::new()
                    } else {
                        m.imaging.apply_setting(k, v, ctx.store, ctx.roles.camera.as_deref(), ctx.roles.wheel.as_deref())
                    }
                })
                .map(Out::ToIndigo)
                .collect();
            // Echo, as KStars does: the form pins an edit until it comes back.
            out.push(Out::ToBrowser(reply_msg("capture_get_all_settings", capture_settings(ctx, m))));
            out
        }
        "capture_preview" | "capture_loop" => {
            let Some(camera) = ctx.roles.camera.as_deref() else {
                return reply("device_message", json!({ "device": PROFILE_NAME, "message": "No camera found on the INDIGO server" }));
            };
            if m.capture.is_running() {
                return reply("device_message", json!({ "device": PROFILE_NAME, "message": "The camera is busy with the capture queue" }));
            }
            // The Imager Agent owns the camera while it focuses or frames.
            if m.agents.set.imager.as_deref().is_some_and(|a| AgentSet::busy(ctx.store, a)) {
                return reply("device_message", json!({ "device": PROFILE_NAME, "message": "The Imager Agent is busy" }));
            }
            m.imaging.start(ty == "capture_loop", camera, ctx.store, ctx.captures_dir).into_iter().map(Out::ToIndigo).collect()
        }
        "capture_stop" => {
            let mut out = Vec::new();
            if let Some(agent) = m.agents.set.imager.clone().filter(|_| m.capture.is_running()) {
                out.extend(outs(m.capture.stop(&agent)));
            }
            if let Some(camera) = ctx.roles.camera.as_deref().filter(|_| m.imaging.is_active()) {
                out.extend(m.imaging.stop(camera).into_iter().map(Out::ToIndigo));
            }
            out
        }
        "capture_start" => {
            let Some(agent) = m.agents.set.imager.clone() else {
                return vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Imager))];
            };
            let mut out = Vec::new();
            // The agent and a direct Imaging loop can't share the camera.
            if let Some(camera) = ctx.roles.camera.as_deref().filter(|_| m.imaging.is_active()) {
                out.extend(m.imaging.stop(camera).into_iter().map(Out::ToIndigo));
            }
            match m.capture.start(ctx.store, &agent, ctx.captures_dir, Instant::now()) {
                Ok(r) => out.extend(outs(r)),
                Err(why) => out.push(Out::ToBrowser(reply_msg("device_message", json!({ "device": agent, "message": format!("Cannot start the capture queue: {why}") })))),
            }
            out
        }
        "capture_get_sequences" => vec![Out::ToBrowser(m.capture.sequences_msg())],
        "capture_load_sequence_file" => {
            let (result, path) = match (payload.get("filedata").and_then(Value::as_str), payload.get("filepath").and_then(Value::as_str)) {
                (Some(xml), _) => (m.capture.load_xml(xml, None), "<inline>".to_string()),
                (None, Some(path)) => (m.capture.load_file(path, None), path.to_string()),
                _ => (Err("no filedata or filepath".to_string()), String::new()),
            };
            let mut out = vec![Out::ToBrowser(reply_msg("capture_load_sequence_file", json!({ "result": result.is_ok(), "path": path })))];
            match result {
                Ok(()) => {
                    out.push(Out::ToBrowser(m.capture.sequences_msg()));
                    out.push(Out::ToBrowser(reply_msg("new_capture_state", json!({ "status": "Idle", "seqv": 0, "seqr": 0, "ovp": 0, "expv": 0.0 }))));
                }
                Err(why) => out.push(Out::ToBrowser(reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("Sequence not loaded: {why}") })))),
            }
            out
        }
        "capture_save_sequence_file" => {
            let Some(path) = payload.get("filepath").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
                return Vec::new();
            };
            match m.capture.save_file(path) {
                Ok(text) => reply("capture_save_sequence_file", json!(text)),
                Err(why) => reply("device_message", json!({ "device": PROFILE_NAME, "message": format!("Sequence not saved: {why}") })),
            }
        }
        "capture_remove_sequence" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
            let mut out = Vec::new();
            if let Err(why) = m.capture.remove(index) {
                out.push(Out::ToBrowser(reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("Job not removed: {why}") }))));
            }
            out.push(Out::ToBrowser(m.capture.sequences_msg()));
            out
        }
        "capture_clear_sequences" => {
            let agent = m.agents.set.imager.clone().unwrap_or_default();
            outs(m.capture.clear(&agent))
        }
        // The browser never sends it (it loads a whole file instead), but
        // Ekos' one-shot form makes a job too.
        "capture_add_sequence" => {
            let mut job = Job::new(FrameType::from_label(&m.imaging.settings.frame_type), m.imaging.settings.exposure, 1);
            job.filter = ctx.roles.wheel.as_deref().and_then(|w| super::imaging::current_filter(ctx.store, w));
            job.gain = ctx.roles.camera.as_deref().and_then(|c| ctx.store.get(c, "CCD_GAIN")).and_then(|g| g.number("GAIN"));
            let mut out = Vec::new();
            if let Err(why) = m.capture.add(job) {
                out.push(Out::ToBrowser(reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("Job not added: {why}") }))));
            }
            out.push(Out::ToBrowser(m.capture.sequences_msg()));
            out
        }

        // ── Focus tab: the Imager Agent ──────────────────────────────────
        "focus_get_all_settings" | "focus_set_all_settings" => {
            let Some(agent) = m.agents.set.imager.clone() else {
                return if ty == "focus_get_all_settings" { Vec::new() } else { vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Imager))] };
            };
            let mut out: Vec<Out> = payload
                .as_object()
                .filter(|_| ty == "focus_set_all_settings")
                .into_iter()
                .flat_map(|map| map.iter())
                .flat_map(|(k, v)| m.focus.apply_setting(k, v, ctx.store, &agent))
                .map(Out::ToIndigo)
                .collect();
            out.push(Out::ToBrowser(reply_msg("focus_get_all_settings", m.focus.settings_payload(ctx.store, &agent))));
            out
        }
        "focus_start" | "focus_stop" | "focus_capture" | "focus_loop" | "focus_reset" | "focus_in" | "focus_out"
        | "focus_set_crosshair" => {
            let Some(agent) = m.agents.set.imager.clone() else {
                return vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Imager))];
            };
            let busy_msg = || reply("device_message", json!({ "device": agent, "message": "The Imager Agent is busy" }));
            let mut out = Vec::new();
            let writes = match ty {
                "focus_start" | "focus_capture" | "focus_loop" => {
                    // The queue's preparation isn't Busy on the agent yet.
                    if m.capture.is_running() {
                        return reply("device_message", json!({ "device": agent, "message": "The Imager Agent is busy with the capture queue" }));
                    }
                    // The agent and a direct Imaging loop can't share the camera.
                    if let Some(camera) = ctx.roles.camera.as_deref().filter(|_| m.imaging.is_active()) {
                        out.extend(m.imaging.stop(camera).into_iter().map(Out::ToIndigo));
                    }
                    let w = match ty {
                        "focus_start" => m.focus.start(ctx.store, &agent),
                        _ => m.focus.preview(ctx.store, &agent, ty == "focus_loop"),
                    };
                    match w {
                        Some(w) => w,
                        None => return busy_msg(),
                    }
                }
                "focus_stop" => m.focus.stop(&agent),
                "focus_reset" => m.focus.reset(ctx.store, &agent),
                "focus_in" | "focus_out" => {
                    let steps = payload.get("steps").and_then(Value::as_f64).unwrap_or(0.0);
                    m.focus.step(ctx.store, &agent, ty == "focus_in", steps)
                }
                _ => {
                    let x = payload.get("x").and_then(Value::as_f64).unwrap_or(0.5);
                    let y = payload.get("y").and_then(Value::as_f64).unwrap_or(0.5);
                    m.focus.crosshair(ctx.store, &agent, x, y)
                }
            };
            out.extend(writes.into_iter().map(Out::ToIndigo));
            out
        }

        // ── Guide tab: the Guider Agent ──────────────────────────────────
        "guide_get_all_settings" | "guide_set_all_settings" => {
            let Some(agent) = m.agents.set.guider.clone() else {
                return if ty == "guide_get_all_settings" { Vec::new() } else { vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Guider))] };
            };
            let mut out: Vec<Out> = payload
                .as_object()
                .filter(|_| ty == "guide_set_all_settings")
                .into_iter()
                .flat_map(|map| map.iter())
                .flat_map(|(k, v)| m.guide.apply_setting(k, v, ctx.store, &agent))
                .map(Out::ToIndigo)
                .collect();
            out.push(Out::ToBrowser(reply_msg("guide_get_all_settings", m.guide.settings_payload(ctx.store, &agent))));
            out
        }
        "guide_set_calibration_settings" => {
            let Some(agent) = m.agents.set.guider.clone() else {
                return vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Guider))];
            };
            let mut out: Vec<Out> = m.guide.apply_calibration(&payload, ctx.store, &agent).into_iter().map(Out::ToIndigo).collect();
            out.push(Out::ToBrowser(reply_msg("guide_get_all_settings", m.guide.settings_payload(ctx.store, &agent))));
            out
        }
        "guide_start" | "guide_stop" | "guide_capture" | "guide_loop" | "guide_clear" => {
            let Some(agent) = m.agents.set.guider.clone() else {
                return vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Guider))];
            };
            if ctx.roles.guider.is_none() {
                return reply("device_message", json!({ "device": agent, "message": "No guide camera: the Guider Agent has no camera selected" }));
            }
            let writes = match ty {
                "guide_start" => m.guide.start(ctx.store, &agent),
                "guide_capture" | "guide_loop" => m.guide.preview(ctx.store, &agent, ty == "guide_loop"),
                "guide_stop" => Some(m.guide.stop(&agent)),
                _ => Some(m.guide.clear_calibration(ctx.store, &agent)),
            };
            match writes {
                Some(w) => w.into_iter().map(Out::ToIndigo).collect(),
                None => reply("device_message", json!({ "device": agent, "message": "The Guider Agent is busy" })),
            }
        }
        // Only the internal guider exists here.
        "option_get" => reply("option_get", json!([{ "name": "GuiderType", "value": 0 }])),
        "option_set" => {
            let external = payload
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|o| o.get("name").and_then(Value::as_str) == Some("GuiderType") && o.get("value").and_then(Value::as_i64).unwrap_or(0) != 0);
            if external {
                reply("device_message", json!({ "device": PROFILE_NAME, "message": "Only the internal guider (the INDIGO Guider Agent) is available" }))
            } else {
                Vec::new()
            }
        }

        // ── Align / Polar Align: the Astrometry Agent ────────────────────
        "align_get_all_settings" | "align_set_all_settings" | "align_set_astrometry_settings" => {
            let Some(agent) = m.agents.set.astrometry.clone() else {
                return if ty == "align_get_all_settings" { Vec::new() } else { vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Astrometry))] };
            };
            let imager = m.agents.set.imager.clone();
            let writes: Vec<String> = match ty {
                "align_set_all_settings" => payload
                    .as_object()
                    .into_iter()
                    .flat_map(|map| map.iter())
                    .flat_map(|(k, v)| m.align.apply_setting(k, v, ctx.store, &agent, imager.as_deref()))
                    .collect(),
                "align_set_astrometry_settings" => m.align.apply_astrometry(&payload, ctx.store, &agent),
                _ => Vec::new(),
            };
            let mut out: Vec<Out> = writes.into_iter().map(Out::ToIndigo).collect();
            out.push(Out::ToBrowser(reply_msg("align_get_all_settings", m.align.settings_payload(ctx.store, &agent, imager.as_deref()))));
            out
        }
        "align_solve" | "align_stop" | "polar_start" | "polar_stop" | "polar_refresh" => {
            let Some(agent) = m.agents.set.astrometry.clone() else {
                return vec![Out::ToBrowser(m.agents.missing_msg(AgentKind::Astrometry))];
            };
            let fail = |why: String| reply("device_message", json!({ "device": agent, "message": why }));
            match ty {
                "align_stop" | "polar_stop" => m.align.stop(&agent).into_iter().map(Out::ToIndigo).collect(),
                "align_solve" => {
                    let rig = rig(&m.agents.set, ctx.store, ctx.roles);
                    match m.align.solve(ctx.store, &agent, &rig) {
                        Ok((writes, warning)) => {
                            let mut out: Vec<Out> = writes.into_iter().map(Out::ToIndigo).collect();
                            if let Some(w) = warning {
                                out.push(Out::ToBrowser(reply_msg("device_message", json!({ "device": agent, "message": w }))));
                            }
                            out
                        }
                        Err(why) => fail(why),
                    }
                }
                "polar_start" => {
                    let rig = rig(&m.agents.set, ctx.store, ctx.roles);
                    match m.align.polar_start(ctx.store, &agent, &rig) {
                        Ok(writes) => writes.into_iter().map(Out::ToIndigo).collect(),
                        Err(why) => fail(why),
                    }
                }
                _ => {
                    let exposure = payload.get("value").and_then(Value::as_f64);
                    match m.align.polar_refresh(ctx.store, &agent, exposure) {
                        Ok(writes) => writes.into_iter().map(Out::ToIndigo).collect(),
                        Err(why) => fail(why),
                    }
                }
            }
        }
        // The agent rotates the mount itself; nothing to acknowledge.
        "polar_slew_done" | "polar_reset_view" => Vec::new(),

        // ── Scheduler: run by the relay ──────────────────────────────────
        "scheduler_get_all_settings" => reply("scheduler_get_all_settings", m.scheduler.settings.payload()),
        "scheduler_set_all_settings" => {
            if let Some(map) = payload.as_object() {
                m.scheduler.apply_settings(map);
            }
            Vec::new()
        }
        "scheduler_get_jobs" => {
            let (sctx, scheduler, _) = sched(ctx, m);
            vec![Out::ToBrowser(scheduler.jobs_msg(&sctx))]
        }
        "scheduler_add_jobs" => {
            let (sctx, scheduler, _) = sched(ctx, m);
            let mut r = SchedReaction::default();
            if let Err(why) = scheduler.add_job(&sctx, &mut r) {
                r.to_browser.push(reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("Job not added: {why}") })));
            }
            sched_outs(r)
        }
        "scheduler_remove_jobs" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
            let (sctx, scheduler, mut drivers) = sched(ctx, m);
            let mut r = SchedReaction::default();
            if let Err(why) = scheduler.remove_job(index, &sctx, &mut drivers, &mut r) {
                r.to_browser.push(reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("Job not removed: {why}") })));
            }
            sched_outs(r)
        }
        // KStars' toggle: stop while running, else start.
        "scheduler_start_job" => {
            let mut out = Vec::new();
            // The queue's camera can't be shared with a preview loop.
            if !m.scheduler.is_running() {
                if let Some(camera) = ctx.roles.camera.as_deref().filter(|_| m.imaging.is_active()) {
                    out.extend(m.imaging.stop(camera).into_iter().map(Out::ToIndigo));
                }
            }
            let (sctx, scheduler, mut drivers) = sched(ctx, m);
            let r = if scheduler.is_running() { scheduler.stop(&sctx, &mut drivers) } else { scheduler.start(&sctx, &mut drivers) };
            out.extend(sched_outs(r));
            out
        }
        "scheduler_save_sequence_file" => {
            let (Some(rel), Some(data)) = (payload.get("path").and_then(Value::as_str), payload.get("filedata").and_then(Value::as_str)) else {
                return reply("scheduler_save_sequence_file", json!({ "result": false, "path": "" }));
            };
            let rel = rel.trim_start_matches('/');
            let abs = std::path::Path::new(ctx.home).join(rel);
            let ok = !rel.split('/').any(|c| c == "..")
                && abs.parent().is_some_and(|p| std::fs::create_dir_all(p).is_ok())
                && std::fs::write(&abs, data).is_ok();
            reply("scheduler_save_sequence_file", json!({ "result": ok, "path": abs.to_string_lossy() }))
        }
        "scheduler_save_file" => {
            let Some(path) = payload.get("filepath").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
                return Vec::new();
            };
            let mut r = SchedReaction::default();
            let saved = m.scheduler.save_file(std::path::Path::new(path), PROFILE_NAME, &mut r);
            let mut out = sched_outs(r);
            match saved {
                Ok(text) => out.push(Out::ToBrowser(reply_msg("scheduler_save_file", json!(text)))),
                Err(why) => out.push(Out::ToBrowser(reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("Schedule not saved: {why}") })))),
            }
            out
        }
        "scheduler_load_file" => {
            let (sctx, scheduler, mut drivers) = sched(ctx, m);
            let mut r = SchedReaction::default();
            let (result, path) = match (payload.get("filedata").and_then(Value::as_str), payload.get("filepath").and_then(Value::as_str)) {
                (Some(xml), _) => (scheduler.load_xml(xml, &sctx, &mut drivers, &mut r), "<inline>".to_string()),
                (None, Some(p)) => (scheduler.load_file(std::path::Path::new(p), &sctx, &mut drivers, &mut r), p.to_string()),
                _ => (Err("no filedata or filepath".to_string()), String::new()),
            };
            let mut out = sched_outs(r);
            out.push(Out::ToBrowser(reply_msg("scheduler_load_file", json!({ "result": result.is_ok(), "path": path }))));
            if let Err(why) = result {
                out.push(Out::ToBrowser(reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("Schedule not loaded: {why}") }))));
            }
            out
        }

        other if POLLED.contains(&other) => Vec::new(),
        other => {
            debug!(command = other, "unsupported with the INDIGO backend");
            reply("device_message", unsupported_payload(other))
        }
    }
}

pub fn reply_msg(ty: &str, payload: Value) -> String {
    json!({ "type": ty, "payload": payload }).to_string()
}

/// A module reaction as command outputs (a command can't ask for a preview
/// fetch; none does).
fn outs(r: super::process::Reaction) -> Vec<Out> {
    r.to_indigo
        .into_iter()
        .map(Out::ToIndigo)
        .chain(r.to_browser.into_iter().map(Out::ToBrowser))
        .collect()
}

/// The scheduler with what it drives, out of the module set (disjoint
/// borrows). The wall clock is read here: the runner takes it as data.
pub fn sched<'a>(ctx: &'a Ctx, m: &'a mut Modules) -> (SchedCtx<'a>, &'a mut Scheduler, Drivers<'a>) {
    let Modules { capture, focus, guide, align, agents, scheduler, .. } = m;
    let sctx = SchedCtx {
        store: ctx.store,
        roles: ctx.roles,
        set: &agents.set,
        captures_dir: ctx.captures_dir,
        now: Instant::now(),
        jd: super::astro::now_jd(),
        local: chrono::Local::now(),
    };
    (sctx, scheduler, Drivers { capture, focus, guide, align })
}

/// A scheduler reaction as command outputs. A procedure's scripts only start
/// from the session tick, never from a command.
fn sched_outs(r: SchedReaction) -> Vec<Out> {
    debug_assert!(r.scripts.is_empty());
    r.to_indigo
        .into_iter()
        .map(Out::ToIndigo)
        .chain(r.to_browser.into_iter().map(Out::ToBrowser))
        .collect()
}

/// `capture_get_all_settings`: the one-shot form plus the queue's limits.
fn capture_settings(ctx: &Ctx, m: &Modules) -> Value {
    let mut p = m.imaging.settings_payload(ctx.store, ctx.roles.camera.as_deref(), ctx.roles.wheel.as_deref(), ctx.captures_dir);
    if let (Some(map), Some(limits)) = (p.as_object_mut(), m.capture.limits_payload().as_object()) {
        for (k, v) in limits {
            map.insert(k.clone(), v.clone());
        }
    }
    p
}

pub fn unsupported_payload(command: &str) -> Value {
    json!({
        "device": PROFILE_NAME,
        "message": format!("{command} is not available with the INDIGO backend"),
    })
}

/// `get_devices` reply: `[{name, connected, interface}]`. Agents are left out
/// — they are INDIGO's own and would confuse the train/device pickers; their
/// properties still arrive through `device_get` if asked by name.
pub fn devices_payload(store: &PropertyStore) -> Value {
    let devices: Vec<Value> = store
        .device_names()
        .filter(|d| store.interface(d) & interface::AGENT == 0)
        .map(|d| {
            json!({
                "name": d,
                "connected": store.connected(d),
                "interface": store.interface(d),
            })
        })
        .collect();
    Value::Array(devices)
}

fn profiles_payload(ctx: &Ctx) -> Value {
    json!({
        "selectedProfile": PROFILE_NAME,
        "profiles": [{
            "name": PROFILE_NAME,
            "auto_connect": true,
            "port_selector": false,
            "mode": "remote",
            "remote_host": ctx.indigo_host,
            "remote_port": ctx.indigo_port,
            "guiding": 0,
            "remote_guiding_host": "",
            "remote_guiding_port": 0,
            "use_web_manager": false,
            "mount": "", "ccd": "", "guider": "", "focuser": "", "filter": "",
            "ao": "", "dome": "", "weather": "",
            "aux1": "", "aux2": "", "aux3": "", "aux4": "",
            "remote": "", "driver_source": "",
        }],
    })
}

/// `astro_get_location` from the first device that knows where it is (the
/// mount first — it is what the sky needs to agree with).
pub fn site_payload(store: &PropertyStore) -> Option<Value> {
    let mut names: Vec<&str> = store.device_names().collect();
    names.sort_by_key(|d| store.interface(d) & interface::MOUNT == 0);
    let geo = names
        .iter()
        .find_map(|d| store.get(d, "GEOGRAPHIC_COORDINATES"))?;
    let lat = geo.number("LATITUDE")?;
    let mut lon = geo.number("LONGITUDE")?;
    // INDIGO longitudes are 0–360 east; the browser expects −180..180.
    if lon > 180.0 {
        lon -= 360.0;
    }
    Some(json!({
        "name": PROFILE_NAME,
        "latitude": lat,
        "longitude": lon,
        "elevation": geo.number("ELEVATION").unwrap_or(0.0),
    }))
}

/// `device_property_set {device, property, elements:[…]}` → one `new*Vector`.
/// Elements come in the Ekos Live shape (`{name, value}` numbers — possibly a
/// sexagesimal string —, `{name, text}`, `{name, state: 0|1}`).
fn property_set(payload: &Value, store: &PropertyStore) -> Option<Out> {
    let device = payload.get("device").and_then(Value::as_str)?;
    let requested = payload.get("property").and_then(Value::as_str)?;
    let elements = payload.get("elements").and_then(Value::as_array)?;
    // An INDI name the client knows (`CCD_COOLER`, `FILTER_SLOT`…) is written
    // to its INDIGO counterpart with its elements renamed; INDIGO's own names
    // and elements pass through untouched.
    let (name, rename) = alias::write_target(requested);
    let name = name.as_str();
    let kind = store.get(device, name).map(|p| p.kind).or_else(|| {
        // Unknown property: infer the kind from the element keys.
        let first = elements.first()?;
        Some(if first.get("text").is_some() {
            Kind::Text
        } else if first.get("state").is_some() {
            Kind::Switch
        } else {
            Kind::Number
        })
    })?;
    let items: Vec<(String, Value)> = elements
        .iter()
        .filter_map(|e| {
            let n = rename(e.get("name").and_then(Value::as_str)?);
            let v = match kind {
                Kind::Number => match e.get("value")? {
                    Value::Number(x) => json!(x),
                    Value::String(s) => json!(scansexa::parse(s)?),
                    _ => return None,
                },
                Kind::Switch => {
                    let on = match e.get("state")? {
                        Value::Number(x) => x.as_f64()? != 0.0,
                        Value::Bool(b) => *b,
                        Value::String(s) => s == "On" || s == "1",
                        _ => return None,
                    };
                    json!(on)
                }
                Kind::Text => json!(e.get("text").and_then(Value::as_str).unwrap_or("")),
                Kind::Light | Kind::Blob => return None,
            };
            Some((n, v))
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(Out::ToIndigo(new_vector_msg(kind, device, name, &items)))
}

/// The Mount tab's commands as INDIGO writes (`components/mount.rs` for the
/// payloads, `PROPERTIES.md` for the targets).
pub(super) fn mount_command(ty: &str, payload: &Value, device: &str, store: &PropertyStore) -> Vec<String> {
    let switch = |name: &str, items: &[(&str, bool)]| {
        let items: Vec<(String, Value)> = items.iter().map(|(n, v)| (n.to_string(), json!(v))).collect();
        new_vector_msg(Kind::Switch, device, name, &items)
    };
    let one_of = |name: &str, on: &str| switch(name, &[(on, true)]);
    match ty {
        "mount_park" => vec![one_of("MOUNT_PARK", "PARKED")],
        "mount_unpark" => vec![one_of("MOUNT_PARK", "UNPARKED")],
        "mount_set_tracking" => {
            let on = payload.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            vec![one_of("MOUNT_TRACKING", if on { "ON" } else { "OFF" })]
        }
        "mount_set_slew_rate" => {
            let rate = payload.get("rate").and_then(Value::as_i64).unwrap_or(7);
            vec![one_of("MOUNT_SLEW_RATE", mount::slew_rate_item(rate))]
        }
        "mount_set_motion" => {
            let action = payload.get("action").and_then(Value::as_bool).unwrap_or(false);
            let (prop, item) = match payload.get("direction").and_then(Value::as_str) {
                Some("N") => ("MOUNT_MOTION_DEC", "NORTH"),
                Some("S") => ("MOUNT_MOTION_DEC", "SOUTH"),
                Some("E") => ("MOUNT_MOTION_RA", "EAST"),
                Some("W") => ("MOUNT_MOTION_RA", "WEST"),
                _ => return Vec::new(),
            };
            vec![switch(prop, &[(item, action)])]
        }
        "mount_abort" => vec![one_of("MOUNT_ABORT_MOTION", "ABORT_MOTION")],
        "mount_goto_rade" | "mount_sync_rade" => {
            let Some(ra_h) = payload.get("ra").and_then(Value::as_str).and_then(scansexa::parse) else {
                return Vec::new();
            };
            let Some(mut dec) = payload.get("de").and_then(Value::as_str).and_then(scansexa::parse) else {
                return Vec::new();
            };
            let mut ra = ra_h * 15.0;
            // The target goes out in the mount's epoch (`MOUNT_EPOCH`: 0 =
            // epoch of date, else J2000 — the simulator's default).
            let target_j2000 = payload.get("isJ2000").and_then(Value::as_bool).unwrap_or(false);
            match (target_j2000, mount::epoch_j2000(store, device)) {
                (true, false) => (ra, dec) = super::astro::j2000_to_jnow(ra, dec, super::astro::now_jd()),
                (false, true) => (ra, dec) = super::astro::jnow_to_j2000(ra, dec, super::astro::now_jd()),
                _ => {}
            }
            let mode = if ty == "mount_goto_rade" { "TRACK" } else { "SYNC" };
            vec![
                one_of("MOUNT_ON_COORDINATES_SET", mode),
                new_vector_msg(
                    Kind::Number,
                    device,
                    "MOUNT_EQUATORIAL_COORDINATES",
                    &[("RA".to_string(), json!(ra / 15.0)), ("DEC".to_string(), json!(dec))],
                ),
            ]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defTextVector":{"device":"Mount","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"1"}]}}"#,
            r#"{"defSwitchVector":{"device":"Mount","name":"CONNECTION","perm":"rw","state":"Ok","items":[{"name":"CONNECTED","value":true},{"name":"DISCONNECTED","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Mount","name":"GEOGRAPHIC_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"LATITUDE","value":48.5},{"name":"LONGITUDE","value":357.7},{"name":"ELEVATION","value":120}]}}"#,
            r#"{"defTextVector":{"device":"Imager Agent","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"16384"}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"CONNECTION","perm":"rw","state":"Ok","items":[{"name":"CONNECTED","value":true}]}}"#,
            r#"{"defNumberVector":{"device":"Mount","name":"MOUNT_EQUATORIAL_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"RA","value":1},{"name":"DEC","value":2}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    fn ctx(store: &PropertyStore) -> Ctx<'_> {
        let roles = Box::leak(Box::new(Roles::derive(store, &super::super::agents::AgentSet::default())));
        Ctx { store, roles, captures_dir: "/tmp/caps", home: "/home/t", indigo_host: "127.0.0.1", indigo_port: 7624 }
    }

    fn handle(cmd: &Value, ctx: &Ctx) -> Vec<Out> {
        super::handle(cmd, ctx, &mut Modules::new(Instant::now()))
    }

    fn browser(outs: &[Out]) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                Out::ToBrowser(s) => serde_json::from_str(s).ok(),
                _ => None,
            })
            .collect()
    }

    fn indigo(outs: &[Out]) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                Out::ToIndigo(s) => serde_json::from_str(s).ok(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn get_devices_skips_agents_and_reports_interface() {
        let s = store();
        let out = handle(&json!({"type":"get_devices","payload":{}}), &ctx(&s));
        let b = browser(&out);
        assert_eq!(b[0]["type"], "get_devices");
        let list = b[0]["payload"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "Mount");
        assert_eq!(list[0]["connected"], true);
        assert_eq!(list[0]["interface"], 1);
    }

    #[test]
    fn device_get_and_property_get() {
        let s = store();
        let out = handle(&json!({"type":"device_get","payload":{"device":"Mount","compact":false}}), &ctx(&s));
        let b = browser(&out);
        assert_eq!(b[0]["payload"]["device"], "Mount");
        assert_eq!(b[0]["payload"]["properties"].as_array().unwrap().len(), 4);
        let out = handle(
            &json!({"type":"device_property_get","payload":{"device":"Mount","property":"GEOGRAPHIC_COORDINATES","compact":true}}),
            &ctx(&s),
        );
        let b = browser(&out);
        assert_eq!(b[0]["payload"]["numbers"][0]["value"], 48.5);
        assert!(b[0]["payload"].get("label").is_none());
        let out = handle(&json!({"type":"device_property_get","payload":{"device":"Mount","property":"NOPE"}}), &ctx(&s));
        assert!(out.is_empty());
    }

    #[test]
    fn property_set_maps_each_kind() {
        let s = store();
        let out = handle(
            &json!({"type":"device_property_set","payload":{"device":"Mount","property":"CONNECTION","elements":[{"name":"DISCONNECTED","state":1}]}}),
            &ctx(&s),
        );
        let i = indigo(&out);
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["value"], true);
        let out = handle(
            &json!({"type":"device_property_set","payload":{"device":"Mount","property":"MOUNT_EQUATORIAL_COORDINATES","elements":[{"name":"RA","value":"05 35 17.3"},{"name":"DEC","value":-5.39}]}}),
            &ctx(&s),
        );
        let i = indigo(&out);
        let items = i[0]["newNumberVector"]["items"].as_array().unwrap();
        assert!((items[0]["value"].as_f64().unwrap() - 5.5881).abs() < 1e-3);
        assert_eq!(items[1]["value"], -5.39);
        // Unknown property, text elements: kind inferred.
        let out = handle(
            &json!({"type":"device_property_set","payload":{"device":"Mount","property":"DEVICE_PORT","elements":[{"name":"PORT","text":"/dev/ttyUSB0"}]}}),
            &ctx(&s),
        );
        assert_eq!(indigo(&out)[0]["newTextVector"]["items"][0]["value"], "/dev/ttyUSB0");
    }

    #[test]
    fn profile_stop_disconnects_only_connected_non_agents() {
        let s = store();
        let out = handle(&json!({"type":"profile_stop","payload":{}}), &ctx(&s));
        let i = indigo(&out);
        assert_eq!(i.len(), 1);
        assert_eq!(i[0]["newSwitchVector"]["device"], "Mount");
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["name"], "DISCONNECTED");
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["value"], true);
        // Already connected (and no agents to fill): profile_start has nothing to do.
        assert!(handle(&json!({"type":"profile_start","payload":{}}), &ctx(&s)).is_empty());
    }

    #[test]
    fn site_wraps_longitude_and_profiles_name_the_server() {
        let s = store();
        let site = site_payload(&s).unwrap();
        assert!((site["longitude"].as_f64().unwrap() - (-2.3)).abs() < 1e-9);
        assert_eq!(site["elevation"], 120.0);
        let b = browser(&handle(&json!({"type":"get_profiles","payload":{}}), &ctx(&s)));
        assert_eq!(b[0]["payload"]["selectedProfile"], PROFILE_NAME);
        assert_eq!(b[0]["payload"]["profiles"][0]["remote_port"], 7624);
    }

    #[test]
    fn aliased_get_and_set() {
        let mut s = store();
        s.apply(parse_frame(r#"{"defSwitchVector":{"device":"Cam","name":"CCD_COOLER","perm":"rw","state":"Ok","items":[{"name":"ON","value":false},{"name":"OFF","value":true}]}}"#).unwrap());
        let out = handle(&json!({"type":"device_property_get","payload":{"device":"Cam","property":"CCD_COOLER","compact":true}}), &ctx(&s));
        let b = browser(&out);
        assert_eq!(b[0]["payload"]["switches"][0]["name"], "COOLER_ON");
        let out = handle(
            &json!({"type":"device_property_set","payload":{"device":"Cam","property":"CCD_COOLER","elements":[{"name":"COOLER_ON","state":1},{"name":"COOLER_OFF","state":0}]}}),
            &ctx(&s),
        );
        let i = indigo(&out);
        assert_eq!(i[0]["newSwitchVector"]["name"], "CCD_COOLER");
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["name"], "ON");
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["value"], true);
        // The mount's INDIGO-named coordinates answer the INDI poll too.
        let out = handle(&json!({"type":"device_property_get","payload":{"device":"Mount","property":"EQUATORIAL_EOD_COORD","compact":true}}), &ctx(&s));
        let b = browser(&out);
        assert_eq!(b[0]["payload"]["name"], "EQUATORIAL_EOD_COORD");
        assert_eq!(b[0]["payload"]["numbers"][0]["value"], 1.0);
    }

    #[test]
    fn mount_commands() {
        let s = store();
        let c = ctx(&s);
        let i = indigo(&handle(&json!({"type":"mount_park","payload":{}}), &c));
        assert_eq!(i[0]["newSwitchVector"]["name"], "MOUNT_PARK");
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["name"], "PARKED");
        let i = indigo(&handle(&json!({"type":"mount_set_motion","payload":{"direction":"W","action":true}}), &c));
        assert_eq!(i[0]["newSwitchVector"]["name"], "MOUNT_MOTION_RA");
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["value"], true);
        let i = indigo(&handle(&json!({"type":"mount_set_slew_rate","payload":{"rate":1}}), &c));
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["name"], "CENTERING");
        let i = indigo(&handle(&json!({"type":"mount_goto_rade","payload":{"ra":"06 00 00","de":"+20 00 00","isJ2000":false}}), &c));
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["name"], "TRACK");
        assert_eq!(i[1]["newNumberVector"]["items"][0]["value"], 6.0);
        let i = indigo(&handle(&json!({"type":"mount_sync_rade","payload":{"ra":"06 00 00","de":"+20 00 00","isJ2000":true}}), &c));
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["name"], "SYNC");
        let ra = i[1]["newNumberVector"]["items"][0]["value"].as_f64().unwrap();
        assert!(ra > 6.0 && ra < 6.05, "{ra}");
        // A J2000 mount takes a J2000 target as is and converts a JNow one.
        let mut s2 = store();
        s2.apply(parse_frame(r#"{"defNumberVector":{"device":"Mount","name":"MOUNT_EPOCH","perm":"rw","state":"Ok","items":[{"name":"EPOCH","value":2000}]}}"#).unwrap());
        let c2 = ctx(&s2);
        let i = indigo(&handle(&json!({"type":"mount_goto_rade","payload":{"ra":"06 00 00","de":"+20 00 00","isJ2000":true}}), &c2));
        assert_eq!(i[1]["newNumberVector"]["items"][0]["value"], 6.0);
        let i = indigo(&handle(&json!({"type":"mount_goto_rade","payload":{"ra":"06 00 00","de":"+20 00 00","isJ2000":false}}), &c2));
        let ra = i[1]["newNumberVector"]["items"][0]["value"].as_f64().unwrap();
        assert!(ra < 6.0 && ra > 5.95, "{ra}");
        let t = browser(&handle(&json!({"type":"train_get_all","payload":{}}), &c));
        assert_eq!(t[0]["payload"][0]["mount"], "Mount");
    }

    #[test]
    fn capture_queue_and_file_commands() {
        let s = store();
        let c = ctx(&s);
        let mut m = Modules::new(Instant::now());
        let b = browser(&super::handle(&json!({"type":"capture_get_sequences","payload":{}}), &c, &mut m));
        assert_eq!(b[0]["type"], "capture_get_sequences");
        assert_eq!(b[0]["payload"], json!([]));
        let b = browser(&super::handle(&json!({"type":"file_default_path","payload":{"type":8}}), &c, &mut m));
        assert_eq!(b[0]["payload"], "/home/t");
        let dir = std::env::temp_dir().join(format!("junos-cmd-{}", std::process::id())).join("sub");
        let b = browser(&super::handle(&json!({"type":"file_directory_operation","payload":{"operation":"create","path":dir}}), &c, &mut m));
        assert_eq!(b[0]["payload"], json!({ "result": true, "operation": "create" }));
        assert!(dir.is_dir());
        let b = browser(&super::handle(&json!({"type":"capture_load_sequence_file","payload":{"filedata":crate::indigo::capture::esq::tests::BROWSER_XML}}), &c, &mut m));
        assert_eq!(b[0]["type"], "capture_load_sequence_file");
        assert_eq!(b[0]["payload"]["result"], true);
        assert_eq!(b[1]["payload"].as_array().unwrap().len(), 2);
        assert_eq!(b[2]["payload"]["status"], "Idle");
        // The limits come back with the one-shot settings.
        let b = browser(&super::handle(&json!({"type":"capture_get_all_settings","payload":{}}), &c, &mut m));
        assert_eq!(b[0]["payload"]["enforceRefocusEveryN"], true);
        assert_eq!(b[0]["payload"]["captureExposureN"], 1.0);
        let b = browser(&super::handle(&json!({"type":"capture_set_all_settings","payload":{"enforceRefocusEveryN":false}}), &c, &mut m));
        assert_eq!(b[0]["payload"]["enforceRefocusEveryN"], false);
        let b = browser(&super::handle(&json!({"type":"capture_remove_sequence","payload":{"index":5}}), &c, &mut m));
        assert_eq!(b[0]["type"], "device_message");
        assert_eq!(b[1]["payload"].as_array().unwrap().len(), 2);
        let b = browser(&super::handle(&json!({"type":"capture_remove_sequence","payload":{"index":0}}), &c, &mut m));
        assert_eq!(b[0]["payload"].as_array().unwrap().len(), 1);
        // No Imager Agent in this rig: start explains.
        let b = browser(&super::handle(&json!({"type":"capture_start","payload":{}}), &c, &mut m));
        assert_eq!(b[0]["type"], "device_message");
        assert!(b[0]["payload"]["message"].as_str().unwrap().contains("Imager Agent"));
        let b = browser(&super::handle(&json!({"type":"capture_clear_sequences","payload":{}}), &c, &mut m));
        assert!(b.iter().any(|v| v["type"] == "capture_get_sequences" && v["payload"] == json!([])));
    }

    #[test]
    fn scheduler_commands() {
        let s = store();
        let c = ctx(&s);
        let mut m = Modules::new(Instant::now());
        let b = browser(&super::handle(&json!({"type":"scheduler_get_jobs","payload":{}}), &c, &mut m));
        assert_eq!(b[0]["type"], "scheduler_get_jobs");
        assert_eq!(b[0]["payload"]["jobs"], json!([]));
        let b = browser(&super::handle(&json!({"type":"scheduler_get_all_settings","payload":{}}), &c, &mut m));
        assert_eq!(b[0]["payload"]["kcfg_GreedyScheduling"], true);
        assert!(super::handle(&json!({"type":"scheduler_set_all_settings","payload":{"schedulerStartupEnabled":true,"nameEdit":"X"}}), &c, &mut m).is_empty());
        assert!(m.scheduler.settings.startup_enabled);
        assert_eq!(m.scheduler.form.0["nameEdit"], "X");
        // The sequence file lands under the relay's home.
        let home = std::env::temp_dir().join(format!("junos-home-{}", std::process::id()));
        let c2 = Ctx { home: &home.to_string_lossy(), ..c };
        let b = browser(&super::handle(&json!({"type":"scheduler_save_sequence_file","payload":{"path":".junos-sequences/x.esq","filedata":"<SequenceQueue version='2.1'/>"}}), &c2, &mut m));
        assert_eq!(b[0]["payload"]["result"], true);
        assert!(home.join(".junos-sequences/x.esq").is_file());
        let b = browser(&super::handle(&json!({"type":"scheduler_save_sequence_file","payload":{"path":"../x.esq","filedata":""}}), &c2, &mut m));
        assert_eq!(b[0]["payload"]["result"], false);
        // Adding without a name explains; the schedule still saves (empty) with its text as the reply.
        let b = browser(&super::handle(&json!({"type":"scheduler_add_jobs","payload":{}}), &c, &mut m));
        assert_eq!(b[0]["type"], "device_message");
        let esl = home.join(".junos-schedules/night.esl");
        let b = browser(&super::handle(&json!({"type":"scheduler_save_file","payload":{"filepath":esl}}), &c, &mut m));
        assert!(b.iter().any(|v| v["type"] == "scheduler_save_file" && v["payload"].as_str().unwrap().contains("<SchedulerList")));
        assert!(esl.is_file());
        let b = browser(&super::handle(&json!({"type":"scheduler_load_file","payload":{"filepath":esl}}), &c, &mut m));
        assert!(b.iter().any(|v| v["type"] == "scheduler_load_file" && v["payload"]["result"] == true));
        // Start with nothing to run stays idle; a remove out of range explains.
        let b = browser(&super::handle(&json!({"type":"scheduler_start_job","payload":{}}), &c, &mut m));
        assert!(b.iter().all(|v| v["type"] == "new_scheduler_state" || v["type"] == "scheduler_get_jobs"));
        let b = browser(&super::handle(&json!({"type":"scheduler_remove_jobs","payload":{"index":3}}), &c, &mut m));
        assert_eq!(b[0]["type"], "device_message");
    }

    #[test]
    fn polled_commands_are_silent_and_others_explain() {
        let s = store();
        assert!(handle(&json!({"type":"get_states","payload":{}}), &ctx(&s)).is_empty());
        // No agents: the polled settings stay silent, the actions explain.
        assert!(handle(&json!({"type":"focus_get_all_settings","payload":{}}), &ctx(&s)).is_empty());
        let b = browser(&handle(&json!({"type":"focus_start","payload":{}}), &ctx(&s)));
        assert_eq!(b[0]["type"], "device_message");
        assert!(b[0]["payload"]["message"].as_str().unwrap().contains("Imager Agent"));
        let b = browser(&handle(&json!({"type":"option_get","payload":{}}), &ctx(&s)));
        assert_eq!(b[0]["payload"][0]["value"], 0);
        let b = browser(&handle(&json!({"type":"scheduler_import_mosaic","payload":{}}), &ctx(&s)));
        assert_eq!(b[0]["type"], "device_message");
        assert!(b[0]["payload"]["message"].as_str().unwrap().contains("scheduler_import_mosaic"));
    }
}
