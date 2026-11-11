//! The JSON-RPC side of PHD2's event server, without the sockets: what a
//! client's bytes become, which method runs with which parameters, and the
//! text that goes back.
//!
//! **Framing.** A client's input is cut on `\n` into a 1024-byte buffer
//! ([`LineReader`]). A line is handed to the parser as is: a `\r` before the
//! `\n` stays (whitespace to the parser, visible only in a parse-error text)
//! and a NUL ends it, the parser reading a C string. When 1024 bytes hold no
//! newline and more input comes, PHD2 throws away everything readable at that
//! moment and answers `-32603 too big` with a null id.
//!
//! **Requests.** A line is one request object, or an array of them (a batch,
//! answered by one array holding the replies, sent only if there is one). A
//! request without a string `method` is answered `-32600` with a null id,
//! even without an id of its own; so are `params` that are neither an array
//! nor an object (`null` included). An unknown method is answered `-32601`.
//! A request without `id` is a notification: a known method runs, nothing is
//! answered. A reply echoes the request's `id` through PHD2's `json_format`:
//! an integer as `%d` of the 32-bit value PHD2's parser stores, a float as
//! `%g`, a string escaped as PHD2 escapes (`\\`, `"`, CR and LF only; any
//! other control character goes out raw).
//!
//! **Running.** [`parse`] gives a [`Parsed`] line: its [`Parsed::calls`] are
//! run in order by the caller — synchronously or not — and their outcomes
//! handed back to [`Parsed::reply`], which builds the text to write (the
//! transport appends PHD2's `\r\n`). A call left without an outcome is
//! answered `-32603 internal error`, so every id gets its one reply.
//!
//! **Parameters** are positional (an array, matched to the method's names in
//! order, extra values ignored) or named (an object); [`Params`] looks both
//! up, the getters type them the way PHD2's handlers do: a bool is a JSON
//! bool or an integer, a float an integer or a float.
//!
//! **Numbers** in results and events are PHD2's `printf` spellings (`%g`,
//! `%.*f`, `[%.2f,%.2f]` points) turned back into JSON numbers ([`num_g`],
//! [`num_f`], [`num_point`]): the value is the rounded one PHD2 prints, but
//! `serde_json` writes it its own way — `1.500` as `1.5`, `2.00` as `2.0`,
//! `1e+06` as `1000000.0` — and a non-finite value becomes `null` where PHD2
//! writes a bare `nan`/`inf` no JSON parser accepts. `serde_json`'s
//! `arbitrary_precision` would keep the spelling but is not needed: clients
//! (Ekos' `QJsonDocument`, NINA's Newtonsoft) parse numbers, so only the value
//! matters. Object members inside a result come out in `serde_json`'s sorted
//! key order rather than PHD2's writing order, for the same reason; the reply
//! envelope keeps PHD2's `{"jsonrpc","result"|"error","id"}`.
//!
//! **Departures from PHD2's parser** (vjson, `json_parser.cpp`), all on input
//! no client sends: requests are parsed by `serde_json`, which refuses what
//! vjson lets through — missing commas and colons, trailing commas,
//! `{"a":}`, leading zeros, a lone `-`, exponents past `f64`, lone UTF-16
//! surrogates, invalid UTF-8 — with `-32700` where PHD2 would go on. The
//! `-32700` text keeps PHD2's template, `invalid JSON request: %s on line %d
//! at "%.12s..."`, with vjson's wording and position for the errors both
//! parsers see (an unclosed object, a second root, a bad identifier, escape
//! or control character, a scalar or closing bracket first); the others say
//! `Unexpected character` where `serde_json` stopped. The line number is
//! always 1 (a line holds no newline; vjson's count of `\n` escapes cancels
//! against the decoded ones). vjson keeps a JSON float as a C `float`
//! (`0.7` → 0.699999988); here it stays a `double`, a difference below every
//! precision PHD2 prints back. Duplicate keys: the last wins, where PHD2's
//! `Params` keeps the first. An integer past `u64` is a float to `serde_json`
//! (vjson wraps any digit string into a 32-bit int).
//!
//! Ported from PHD2 src/event_server.cpp (the parse-error wording from
//! src/json_parser.cpp) — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::{Number, Value};

use crate::engine::control::SettleParams;
use crate::engine::log::cfmt::{fmt_f, fmt_g};
use crate::engine::traits::{Point, Rect};

// ------------------------------------------------------------------ errors

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

/// A JSON-RPC error: PHD2's own codes (1, 2, 3) or the JSON-RPC ones above.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> RpcError {
        RpcError { code, message: message.into() }
    }

    /// `-32602`, PHD2's answer to a missing or mistyped parameter.
    pub fn invalid_params(message: impl Into<String>) -> RpcError {
        RpcError::new(INVALID_PARAMS, message)
    }
}

// ----------------------------------------------------------------- methods

/// A method the server answers: its name, its parameter names in positional
/// order, and whether only the relay's websocket may call it.
#[derive(Debug, PartialEq, Eq)]
pub struct Method {
    pub name: &'static str,
    /// An empty list takes named parameters only.
    pub params: &'static [&'static str],
    /// Not a PHD2 method: the TCP server answers `-32601`, like PHD2 does for
    /// any name it does not know.
    pub ws_only: bool,
}

const fn phd2(name: &'static str, params: &'static [&'static str]) -> Method {
    Method { name, params, ws_only: false }
}

const fn ws(name: &'static str, params: &'static [&'static str]) -> Method {
    Method { name, params, ws_only: true }
}

/// PHD2's dispatch table (`event_server.cpp:2383`), in its order, with the
/// names each handler gives `Params`; then the daemon's own methods.
pub static METHODS: &[Method] = &[
    phd2("clear_calibration", &["which"]),
    phd2("deselect_star", &[]),
    phd2("get_exposure", &[]),
    phd2("set_exposure", &["exposure"]),
    phd2("get_exposure_durations", &[]),
    phd2("get_profiles", &[]),
    phd2("get_profile", &[]),
    phd2("set_profile", &["id"]),
    phd2("get_connected", &[]),
    phd2("set_connected", &["connected"]),
    phd2("get_calibrated", &[]),
    phd2("get_paused", &[]),
    phd2("set_paused", &["paused", "type"]),
    phd2("get_lock_position", &[]),
    phd2("set_lock_position", &["x", "y", "exact"]),
    phd2("loop", &[]),
    phd2("stop_capture", &[]),
    phd2("guide", &["settle", "recalibrate", "roi"]),
    phd2("dither", &["amount", "raOnly", "settle"]),
    phd2("find_star", &["roi"]),
    phd2("get_pixel_scale", &[]),
    phd2("get_app_state", &[]),
    phd2("flip_calibration", &[]),
    phd2("get_lock_shift_enabled", &[]),
    phd2("set_lock_shift_enabled", &["enabled"]),
    phd2("get_lock_shift_params", &["axes"]),
    // `[{…}]` is read as the object inside (`parse_lock_shift_params`).
    phd2("set_lock_shift_params", &["rate", "units", "axes"]),
    phd2("save_image", &[]),
    phd2("get_star_image", &["size"]),
    phd2("get_use_subframes", &[]),
    phd2("get_search_region", &[]),
    phd2("shutdown", &[]),
    phd2("get_camera_binning", &[]),
    phd2("get_camera_frame_size", &[]),
    phd2("get_current_equipment", &[]),
    phd2("get_guide_output_enabled", &[]),
    phd2("set_guide_output_enabled", &["enabled"]),
    phd2("get_algo_param_names", &["axis"]),
    phd2("get_algo_param", &["axis", "name"]),
    phd2("set_algo_param", &["axis", "name", "value"]),
    phd2("get_dec_guide_mode", &[]),
    phd2("set_dec_guide_mode", &["mode"]),
    phd2("get_settling", &[]),
    phd2("guide_pulse", &["amount", "direction", "which"]),
    phd2("get_calibration_data", &["which"]),
    phd2("capture_single_frame", &["exposure", "binning", "gain", "subframe", "path", "save"]),
    phd2("get_cooler_status", &[]),
    phd2("set_cooler_state", &["enabled"]),
    phd2("get_ccd_temperature", &[]),
    phd2("export_config_settings", &[]),
    phd2("get_variable_delay_settings", &[]),
    phd2("set_variable_delay_settings", &["Enabled", "ShortDelaySeconds", "LongDelaySeconds"]),
    phd2("get_limit_frame", &[]),
    phd2("set_limit_frame", &["roi"]),
    // The junos-web Guide tab's extras, reached through the relay's
    // `guider_rpc` only: one row per engine command the UI asks for
    // (`Command::name`), and the names each handler in `extras.rs` reads.
    // The tools the engine does not have yet start with start/stop.
    ws("select_star", &["x", "y"]),
    ws("set_profile_value", &["key", "value"]),
    ws("new_profile", &["name"]),
    ws("rename_profile", &["id", "name"]),
    ws("delete_profile", &["id"]),
    ws("import_profile", &["text", "name"]),
    ws("export_profile", &["id"]),
    ws("export_settings", &[]),
    ws(
        "set_equipment",
        &[
            "camera",
            "mount",
            "aux_mount",
            "ao",
            "rotator",
            "camera_device",
            "camera_ccd",
            "mount_device",
            "ao_device",
            "rotator_device",
            "indi_host",
            "indi_port",
        ],
    ),
    ws("connect_device", &["role", "connected"]),
    ws("list_devices", &[]),
    ws("set_algorithm", &["axis", "algorithm"]),
    ws("reset_algo_params", &["axis"]),
    ws("set_manual_pointing", &["latitude", "longitude", "ra", "dec", "pier_side"]),
    ws("set_calibration_data", &["which", "xRate", "yRate", "xAngle", "yAngle", "declination", "binning"]),
    ws("darks_build", &["min_ms", "max_ms", "count", "new_library", "note"]),
    ws("darks_stop", &[]),
    ws("darks_use", &["dark", "defect"]),
    ws("bpm_build", &["exposure_ms", "count", "note"]),
    ws("bpm_set", &["hot", "cold"]),
    ws("bpm_save", &[]),
    ws("ga_open", &[]),
    ws("ga_close", &[]),
    ws("ga_start", &[]),
    ws("ga_stop", &[]),
    ws("ga_end_wait", &["accept"]),
    ws("ga_set_measure_backlash", &["on"]),
    ws("ga_apply", &["kind"]),
    ws("ga_history", &[]),
    ws("ga_review", &["timestamp"]),
    ws("pa_drift_open", &[]),
    ws("pa_drift_start", &[]),
    ws("pa_drift_stop", &[]),
    ws("pa_drift_close", &[]),
    ws("pa_static_start", &[]),
    ws("pa_static_rotate", &[]),
    ws("pa_static_mark", &[]),
    ws("pa_static_stop", &[]),
    ws("drift_align_start", &[]),
    ws("drift_align_stop", &[]),
    ws("cal_assistant_start", &[]),
    ws("cal_assistant_stop", &[]),
    ws("pierflip_start", &[]),
    ws("pierflip_stop", &[]),
    ws("starcross_start", &[]),
    ws("starcross_stop", &[]),
    ws("nudge_lock", &["dx", "dy"]),
    ws("save_lock", &[]),
    ws("restore_lock", &[]),
    ws("comet_train_start", &[]),
    ws("comet_train_stop", &[]),
    ws("set_dither_mode", &["mode", "scale", "ra_only"]),
    ws("set_auto_exposure", &["min", "max", "target_snr"]),
    ws("set_image_logger", &[]),
    ws("set_frame_filter", &["frames", "profile", "kinds", "max_width", "quality"]),
    ws("get_history", &[]),
    ws("list_logs", &[]),
    ws("read_log", &["name", "tail"]),
];

/// The method of that name; a `ws_only` one only when `ws`.
pub fn method(name: &str, ws: bool) -> Option<&'static Method> {
    METHODS.iter().find(|m| m.name == name && (ws || !m.ws_only))
}

// ----------------------------------------------------------------- framing

/// PHD2's per-client read buffer (`ClientReadBuf::SIZE`).
pub const READ_BUF_SIZE: usize = 1024;

/// What the read buffer gives out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// A line, without its `\n`.
    Line(Vec<u8>),
    /// 1024 bytes without a newline, and more coming.
    TooBig,
}

/// A client's read buffer (`handle_cli_input`).
#[derive(Debug, Default)]
pub struct LineReader {
    buf: Vec<u8>,
}

impl LineReader {
    /// Takes what one socket read returned — what PHD2 finds readable at that
    /// moment — and gives the complete lines in it.
    pub fn feed(&mut self, mut data: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        while !data.is_empty() {
            if self.buf.len() == READ_BUF_SIZE {
                // drain_input: the rest of what is readable goes too, good
                // lines included, and the buffer starts afresh.
                self.buf.clear();
                out.push(Input::TooBig);
                break;
            }
            let n = data.len().min(READ_BUF_SIZE - self.buf.len());
            self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                line.pop();
                out.push(Input::Line(line));
            }
        }
        out
    }
}

// ---------------------------------------------------------------- requests

/// A request to run.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub method: &'static Method,
    /// An array or an object.
    pub params: Option<Value>,
    /// `None` for a notification: run, never answered.
    pub id: Option<Value>,
}

impl Call {
    /// The parameters under the method's names.
    pub fn params(&self) -> Params<'_> {
        Params::new(self.method.params, self.params.as_ref())
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Item {
    Call(Call),
    /// Answered without running anything; `None`: not answered at all.
    Reply(Option<String>),
}

/// One line of input, parsed.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    batch: bool,
    items: Vec<Item>,
}

impl Parsed {
    /// The requests to run, in order.
    pub fn calls(&self) -> impl Iterator<Item = &Call> {
        self.items.iter().filter_map(|item| match item {
            Item::Call(c) => Some(c),
            Item::Reply(_) => None,
        })
    }

    /// The text to send back, given the outcomes of [`Parsed::calls`] in
    /// their order; `None` when nothing is to be sent.
    pub fn reply(self, outcomes: Vec<Result<Value, RpcError>>) -> Option<String> {
        let mut outcomes = outcomes.into_iter();
        let mut replies = self.items.into_iter().filter_map(|item| match item {
            Item::Call(c) => {
                let out = outcomes.next().unwrap_or_else(|| Err(RpcError::new(INTERNAL_ERROR, "internal error")));
                c.id.map(|id| reply(&out, &id))
            }
            Item::Reply(r) => r,
        });
        if !self.batch {
            return replies.next();
        }
        let all: Vec<String> = replies.collect();
        (!all.is_empty()).then(|| format!("[{}]", all.join(",")))
    }
}

/// What a client sent, ready to run (`handle_cli_input_complete`).
pub fn parse(input: &Input) -> Parsed {
    let root = match input {
        Input::TooBig => return answered(reply(&Err(RpcError::new(INTERNAL_ERROR, "too big")), &Value::Null)),
        Input::Line(line) => match parse_json(line) {
            Ok(root) => root,
            Err(msg) => return answered(reply(&Err(RpcError::new(PARSE_ERROR, msg)), &Value::Null)),
        },
    };
    match root {
        Value::Array(reqs) => Parsed { batch: true, items: reqs.into_iter().map(request).collect() },
        req => Parsed { batch: false, items: vec![request(req)] },
    }
}

fn answered(text: String) -> Parsed {
    Parsed { batch: false, items: vec![Item::Reply(Some(text))] }
}

/// One request of a line (`handle_request`): a `-32600` is answered with a
/// null id whatever the request's own, a `-32601` with its id, and not at
/// all for a notification.
fn request(req: Value) -> Item {
    let id = req.get("id").cloned();
    match call(req, false) {
        Ok(c) => Item::Call(c),
        Err(e) => {
            let id = if e.code == METHOD_NOT_FOUND { id } else { Some(Value::Null) };
            Item::Reply(id.map(|id| reply(&Err(e), &id)))
        }
    }
}

/// A request object as PHD2 checks it (`parse_request`, `handle_request`):
/// a string `method` first, then `params` an array or an object, then a
/// known method — `ws`, a relay call, knows the junos UI's methods too.
pub fn call(req: Value, ws: bool) -> Result<Call, RpcError> {
    let (name, params, id) = match req {
        Value::Object(mut m) => (m.remove("method"), m.remove("params"), m.remove("id")),
        _ => (None, None, None),
    };
    let Some(Value::String(name)) = name else {
        return Err(RpcError::new(INVALID_REQUEST, "invalid request - missing method"));
    };
    if params.as_ref().is_some_and(|p| !p.is_array() && !p.is_object()) {
        return Err(RpcError::new(INVALID_REQUEST, "invalid request - params must be an array or object"));
    }
    match method(&name, ws) {
        Some(method) => Ok(Call { method, params, id }),
        None => Err(RpcError::new(METHOD_NOT_FOUND, "method not found")),
    }
}

/// The line as JSON, or the `-32700` message.
fn parse_json(line: &[u8]) -> Result<Value, String> {
    // A C string to PHD2's parser.
    let line = &line[..line.iter().position(|&b| b == 0).unwrap_or(line.len())];
    // vjson only takes an object or an array as the root; serde_json takes
    // any value, so the first character is checked here.
    let (desc, pos) = match line.iter().position(|b| !b" \t\r\n".contains(b)) {
        None => ("empty string", line.len()),
        Some(i) => match line[i] {
            b'{' | b'[' => match serde_json::from_slice(line) {
                Ok(root) => return Ok(root),
                Err(e) => vjson_error(line, &e),
            },
            b'}' | b']' => ("Mismatch closing brace/bracket", i),
            _ => ("Unexpected character", i),
        },
    };
    let at = &line[pos..];
    let at = String::from_utf8_lossy(&at[..at.len().min(12)]);
    Err(format!("invalid JSON request: {desc} on line 1 at \"{at}...\""))
}

/// vjson's description and position for a `serde_json` error.
fn vjson_error(line: &[u8], e: &serde_json::Error) -> (&'static str, usize) {
    if e.is_eof() {
        return ("Not all objects/arrays have been properly closed", line.len());
    }
    // serde_json's column is 1-based and counts bytes.
    let i = e.column().saturating_sub(1).min(line.len());
    let msg = e.to_string();
    let code = msg.rsplit_once(" at line ").map_or(msg.as_str(), |(code, _)| code);
    let back = |from: usize, keep: fn(u8) -> bool| (0..from).rev().take_while(|&k| keep(line[k])).last().unwrap_or(from);
    match code {
        "trailing characters" => match line.get(i) {
            // vjson stops after the second opening bracket.
            Some(b'{' | b'[') => ("Second root. Only one root allowed", i + 1),
            Some(b'}' | b']') => ("Mismatch closing brace/bracket", i),
            _ => ("Unexpected character", i),
        },
        "expected ident" => ("Unknown identifier", back(i, |b| b.is_ascii_alphabetic())),
        "key must be a string" => ("Missing name", i),
        "invalid number" | "number out of range" => {
            let start = back(i, |b| b"0123456789+-.eE".contains(&b));
            let end = line[start..].iter().position(|b| b" \t\r\n,]}".contains(b)).map_or(line.len(), |n| start + n);
            if line[start..end].iter().any(|b| b".eE".contains(b)) {
                ("Bad float number", start)
            } else {
                ("Bad integer number", start)
            }
        }
        "invalid escape" | "invalid unicode code point" | "unexpected end of hex escape" | "lone leading surrogate in hex escape" => {
            let bs = line[..i].iter().rposition(|&b| b == b'\\').unwrap_or(i);
            match line.get(bs + 1) {
                Some(b'u') => ("Bad unicode codepoint", bs),
                _ => ("Unrecognized escape sequence", string_start(line, bs)),
            }
        }
        c if c.starts_with("control character") => ("Control characters not allowed in strings", string_start(line, i)),
        _ => ("Unexpected character", i),
    }
}

/// Where the content of the string holding byte `i` starts: vjson reports
/// string errors there.
fn string_start(line: &[u8], i: usize) -> usize {
    let (mut start, mut in_string, mut k) = (0, false, 0);
    while k < i {
        match line[k] {
            b'\\' if in_string => k += 1,
            b'"' => {
                in_string = !in_string;
                start = k + 1;
            }
            _ => {}
        }
        k += 1;
    }
    start
}

// -------------------------------------------------------------- parameters

/// A request's parameters under a method's names (PHD2's `Params`).
#[derive(Clone, Copy, Debug)]
pub struct Params<'a> {
    names: &'a [&'a str],
    params: Option<&'a Value>,
}

impl<'a> Params<'a> {
    pub fn new(names: &'a [&'a str], params: Option<&'a Value>) -> Params<'a> {
        Params { names, params }
    }

    /// The value given for `name`: by position in an array, by key in an
    /// object.
    pub fn param(&self, name: &str) -> Option<&'a Value> {
        match self.params? {
            Value::Array(values) => values.get(self.names.iter().position(|n| *n == name)?),
            Value::Object(map) => map.get(name),
            _ => None,
        }
    }
}

/// A JSON integer (no `.`, `e` or `E` in it) as the C `int` PHD2's parser
/// keeps: wrapped to 32 bits, so 4294967301 is 5.
fn int_value(n: &Number) -> Option<i32> {
    n.as_i64().map(|i| i as i32).or_else(|| n.as_u64().map(|u| u as i32))
}

/// `JSON_INT`.
pub fn int_param(v: &Value) -> Option<i32> {
    v.as_number().and_then(int_value)
}

/// `float_param`: an integer or a float.
pub fn float_param(v: &Value) -> Option<f64> {
    let n = v.as_number()?;
    int_value(n).map(f64::from).or_else(|| n.as_f64())
}

/// `bool_param`: a JSON bool, or an integer (non-zero is true).
pub fn bool_param(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        _ => int_param(v).map(|i| i != 0),
    }
}

/// `JSON_STRING`.
pub fn string_param(v: &Value) -> Option<&str> {
    v.as_str()
}

/// `parse_rect`: `[x, y, width, height]`, four integers exactly.
pub fn rect_param(v: &Value) -> Option<Rect> {
    match v.as_array()?.as_slice() {
        [x, y, w, h] => Some(Rect::new(int_param(x)?, int_param(y)?, int_param(w)?, int_param(h)?)),
        _ => None,
    }
}

/// `parse_point`: `[x, y]`, two numbers exactly.
pub fn point_param(v: &Value) -> Option<Point> {
    match v.as_array()?.as_slice() {
        [x, y] => Some(Point { x: float_param(x)?, y: float_param(y)? }),
        _ => None,
    }
}

/// The optional `roi` of `guide` and `find_star`: none when absent.
pub fn roi_param(v: Option<&Value>) -> Result<Option<Rect>, RpcError> {
    match v {
        None => Ok(None),
        Some(v) => rect_param(v).map(Some).ok_or_else(|| RpcError::invalid_params("invalid ROI param")),
    }
}

/// The `settle` object of `guide` and `dither` (`parse_settle`): `pixels`,
/// `time` and `timeout` all required, numbers; the two times floored to whole
/// seconds; `frames` always 99999, so only the distance and time settle.
pub fn settle_param(v: Option<&Value>) -> Result<SettleParams, RpcError> {
    let Some(Value::Object(settle)) = v else {
        return Err(RpcError::invalid_params("expected settle object param"));
    };
    let num = |key| settle.get(key).and_then(float_param);
    match (num("pixels"), num("time"), num("timeout")) {
        (Some(px), Some(time), Some(timeout)) => Ok(SettleParams {
            tolerance_px: px,
            settle_time_s: time.floor() as i32,
            timeout_s: timeout.floor() as i32,
            frames: 99999,
        }),
        _ => Err(RpcError::invalid_params("invalid settle params")),
    }
}

// ----------------------------------------------------------------- replies

/// One reply: `{"jsonrpc":"2.0","result":…,"id":…}` or
/// `{"jsonrpc":"2.0","error":{"code":…,"message":"…"},"id":…}`.
pub fn reply(outcome: &Result<Value, RpcError>, id: &Value) -> String {
    let mut s = String::from("{\"jsonrpc\":\"2.0\",");
    match outcome {
        Ok(result) => {
            s.push_str("\"result\":");
            write_json(&mut s, result, false);
        }
        Err(e) => {
            s.push_str(&format!("\"error\":{{\"code\":{},\"message\":\"", e.code));
            json_escape(&mut s, &e.message);
            s.push_str("\"}");
        }
    }
    s.push_str(",\"id\":");
    write_json(&mut s, id, true);
    s.push('}');
    s
}

/// A value as PHD2 writes the ones it builds: numbers as given, strings
/// escaped as PHD2 escapes them (the events' values).
pub fn json_text(v: &Value) -> String {
    let mut s = String::new();
    write_json(&mut s, v, false);
    s
}

/// PHD2's `json_format`, which writes what it parsed back (`echo`: integers
/// as `%d`, floats as `%g`), and the writer of the values it builds (numbers
/// as given). Strings are escaped as PHD2 escapes; member names never are.
fn write_json(out: &mut String, v: &Value, echo: bool) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) if echo => match int_value(n) {
            Some(i) => out.push_str(&i.to_string()),
            None => out.push_str(&fmt_g(n.as_f64().unwrap_or(0.0))),
        },
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            out.push('"');
            json_escape(out, s);
            out.push('"');
        }
        Value::Array(values) => {
            out.push('[');
            for (k, x) in values.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                write_json(out, x, echo);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (k, (name, x)) in map.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(name);
                out.push_str("\":");
                write_json(out, x, echo);
            }
            out.push('}');
        }
    }
}

/// PHD2's `json_escape`: backslash, double quote, CR and LF only.
fn json_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

// ----------------------------------------------------------------- numbers

/// A number as PHD2 printed it: an integer when the text has no point or
/// exponent, so `%g` of 2.0 still reads `2`.
fn from_text(s: &str) -> Value {
    match s.parse::<i64>() {
        Ok(i) => Value::from(i),
        Err(_) => s.parse::<f64>().ok().and_then(Number::from_f64).map_or(Value::Null, Value::Number),
    }
}

/// `NV(name, double)`: `%g`, six significant digits.
pub fn num_g(v: f64) -> Value {
    from_text(&fmt_g(v))
}

/// `NV(name, double, prec)`: `%.*f`.
pub fn num_f(v: f64, prec: usize) -> Value {
    from_text(&fmt_f(v, prec))
}

/// `NV(name, PHD_Point)`: `[%.2f,%.2f]`.
pub fn num_point(x: f64, y: f64) -> Value {
    Value::Array(vec![num_f(x, 2), num_f(y, 2)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Runs a line through `dispatch` the way the server will.
    fn run(line: &[u8], dispatch: &mut dyn FnMut(&Call) -> Result<Value, RpcError>) -> Option<String> {
        let parsed = parse(&Input::Line(line.to_vec()));
        let outcomes = parsed.calls().map(|c| dispatch(c)).collect();
        parsed.reply(outcomes)
    }

    /// The replies of a server answering every method with `0`.
    fn zero(line: &str) -> Option<String> {
        run(line.as_bytes(), &mut |_| Ok(json!(0)))
    }

    /// The `-32700` reply PHD2 sends for that vjson message.
    fn parse_error(msg: &str) -> String {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":-32700,\"message\":\"invalid JSON request: {msg}\"}},\"id\":null}}"
        )
    }

    const TOO_BIG: &str = r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"too big"},"id":null}"#;

    #[test]
    fn framing_splits_on_newline_and_keeps_partial_lines() {
        let mut r = LineReader::default();
        assert_eq!(r.feed(b"{\"a\":1}\r\n{\"b\""), vec![Input::Line(b"{\"a\":1}\r".to_vec())]);
        assert_eq!(r.feed(b":2}\n\n"), vec![Input::Line(b"{\"b\":2}".to_vec()), Input::Line(Vec::new())]);
        assert_eq!(r.feed(b""), vec![]);
    }

    #[test]
    fn a_1023_byte_line_fits_the_buffer() {
        let mut r = LineReader::default();
        let mut data = vec![b' '; 1023];
        data.push(b'\n');
        data.extend_from_slice(b"x\n");
        assert_eq!(r.feed(&data), vec![Input::Line(vec![b' '; 1023]), Input::Line(b"x".to_vec())]);
    }

    #[test]
    fn too_big_drains_what_is_readable() {
        // 1024 bytes and a newline: the newline and the good line after it
        // are thrown away with the rest of the read.
        let mut r = LineReader::default();
        let mut data = vec![b'a'; 1024];
        data.extend_from_slice(b"\n{\"method\":\"loop\",\"id\":1}\n");
        assert_eq!(r.feed(&data), vec![Input::TooBig]);
        assert_eq!(r.feed(b"{\"x\":1}\n"), vec![Input::Line(b"{\"x\":1}".to_vec())]);

        // A full buffer is only refused when more input comes.
        let mut r = LineReader::default();
        assert_eq!(r.feed(&[b'a'; 1024]), vec![]);
        assert_eq!(r.feed(b"\n{\"x\":1}\n"), vec![Input::TooBig]);
        assert_eq!(r.feed(b"\n"), vec![Input::Line(Vec::new())]);

        assert_eq!(parse(&Input::TooBig).reply(vec![]).as_deref(), Some(TOO_BIG));
    }

    #[test]
    fn parse_errors_word_as_vjson() {
        // Each message is what PHD2's own parser (a harness over
        // json_parser.cpp) says for the line.
        let cases: &[(&str, &str)] = &[
            ("", r#"empty string on line 1 at \"...\""#),
            ("  \r", r#"empty string on line 1 at \"...\""#),
            ("{", r#"Not all objects/arrays have been properly closed on line 1 at \"...\""#),
            ("[1,", r#"Not all objects/arrays have been properly closed on line 1 at \"...\""#),
            (r#"{"a":"abc"#, r#"Not all objects/arrays have been properly closed on line 1 at \"...\""#),
            ("5", r#"Unexpected character on line 1 at \"5...\""#),
            (r#""abc""#, r#"Unexpected character on line 1 at \"\"abc\"...\""#),
            ("null", r#"Unexpected character on line 1 at \"null...\""#),
            ("}", r#"Mismatch closing brace/bracket on line 1 at \"}...\""#),
            ("]", r#"Mismatch closing brace/bracket on line 1 at \"]...\""#),
            ("x", r#"Unexpected character on line 1 at \"x...\""#),
            (r#"{"a":1}}"#, r#"Mismatch closing brace/bracket on line 1 at \"}...\""#),
            (r#"{"a":1}{"#, r#"Second root. Only one root allowed on line 1 at \"...\""#),
            (r#"{"a":1}["#, r#"Second root. Only one root allowed on line 1 at \"...\""#),
            (r#"{"a":1} 5"#, r#"Unexpected character on line 1 at \"5...\""#),
            (r#"{"a":1},"#, r#"Unexpected character on line 1 at \",...\""#),
            (r#"{"a":x}"#, r#"Unexpected character on line 1 at \"x}...\""#),
            (r#"{"a":nul}"#, r#"Unknown identifier on line 1 at \"nul}...\""#),
            (r#"{"a":tru}"#, r#"Unknown identifier on line 1 at \"tru}...\""#),
            (r#"{"a":"\q"}"#, r#"Unrecognized escape sequence on line 1 at \"\\q\"}...\""#),
            (r#"{"a":"\u12g4"}"#, r#"Bad unicode codepoint on line 1 at \"\\u12g4\"}...\""#),
            (r#"{1:2}"#, r#"Missing name on line 1 at \"1:2}...\""#),
            (r#"{"a":"x\ny","b":?}"#, r#"Unexpected character on line 1 at \"?}...\""#),
            (r#"{"a":"é","b":?}"#, r#"Unexpected character on line 1 at \"?}...\""#),
            // The rest of the line is cut at 12 bytes, a `\r` kept (escaped
            // by json_escape), a raw tab sent raw.
            ("{\"a\":1}}  abcdefghijk", r#"Mismatch closing brace/bracket on line 1 at \"}  abcdefghi...\""#),
            ("{\"a\":1}}\r", r#"Mismatch closing brace/bracket on line 1 at \"}\r...\""#),
            ("{\"a\":\"tab\tx\"}", "Control characters not allowed in strings on line 1 at \\\"tab\tx\\\"}...\\\""),
        ];
        for (line, msg) in cases {
            assert_eq!(zero(line), Some(parse_error(msg)), "line {line:?}");
        }
    }

    #[test]
    fn parse_errors_where_the_parsers_part() {
        // vjson reads these on (01 is 1, `-` is 0, 1e400 is inf, a missing
        // comma is skipped); serde_json refuses them.
        assert_eq!(zero(r#"{"a":01}"#), Some(parse_error(r#"Bad integer number on line 1 at \"01}...\""#)));
        assert_eq!(zero(r#"{"a":1e400}"#), Some(parse_error(r#"Bad float number on line 1 at \"1e400}...\""#)));
        assert_eq!(zero(r#"{"a":-}"#), Some(parse_error(r#"Bad integer number on line 1 at \"-}...\""#)));
        assert_eq!(
            zero(r#"{"method":"loop" "id":5}"#),
            Some(parse_error(r#"Unexpected character on line 1 at \"\"id\":5}...\""#))
        );
        // vjson: "Bad float number" at "1.2.3}"; serde_json stops at the
        // second point.
        assert_eq!(zero(r#"{"a":1.2.3}"#), Some(parse_error(r#"Unexpected character on line 1 at \".3}...\""#)));
    }

    #[test]
    fn a_nul_ends_the_line() {
        assert_eq!(zero("{\"method\":\"loop\",\"id\":1}\0{garbage"), Some(r#"{"jsonrpc":"2.0","result":0,"id":1}"#.into()));
    }

    #[test]
    fn invalid_requests_are_answered_with_a_null_id() {
        let missing = r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"invalid request - missing method"},"id":null}"#;
        let bad_params = r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"invalid request - params must be an array or object"},"id":null}"#;
        assert_eq!(zero(r#"{"id":3}"#).as_deref(), Some(missing));
        assert_eq!(zero(r#"{"method":5,"id":3}"#).as_deref(), Some(missing));
        // Even a notification is answered.
        assert_eq!(zero(r#"{"params":[]}"#).as_deref(), Some(missing));
        assert_eq!(zero(r#"{"method":"loop","params":3,"id":1}"#).as_deref(), Some(bad_params));
        assert_eq!(zero(r#"{"method":"loop","params":null,"id":1}"#).as_deref(), Some(bad_params));
        assert_eq!(zero(r#"{"method":"nope","params":"x"}"#).as_deref(), Some(bad_params));
    }

    #[test]
    fn unknown_methods() {
        assert_eq!(
            zero(r#"{"method":"nope","id":"a"}"#).as_deref(),
            Some(r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"method not found"},"id":"a"}"#)
        );
        assert_eq!(zero(r#"{"method":"nope"}"#), None);
        // The daemon's own methods are not served over TCP.
        assert_eq!(
            zero(r#"{"method":"select_star","params":[1,2],"id":7}"#).as_deref(),
            Some(r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"method not found"},"id":7}"#)
        );
        assert_eq!(method("select_star", true).map(|m| m.params), Some(&["x", "y"][..]));
        assert_eq!(method("select_star", false), None);
        assert_eq!(method("loop", true).map(|m| m.ws_only), Some(false));
    }

    #[test]
    fn notifications_run_without_reply() {
        let mut ran = Vec::new();
        let r = run(br#"{"method":"loop"}"#, &mut |c| {
            ran.push(c.method.name);
            Ok(json!(0))
        });
        assert_eq!(r, None);
        assert_eq!(ran, ["loop"]);
        // `"id":null` is an id.
        assert_eq!(zero(r#"{"method":"loop","id":null}"#).as_deref(), Some(r#"{"jsonrpc":"2.0","result":0,"id":null}"#));
    }

    #[test]
    fn batches() {
        let mut ran = Vec::new();
        let line = br#"[{"method":"get_app_state","id":1},{"method":"loop"},{"method":"nope"},5,{"method":"get_exposure","id":2},{"method":"nope","id":3}]"#;
        let r = run(line, &mut |c| {
            ran.push(c.method.name);
            Ok(match c.method.name {
                "get_app_state" => json!("Guiding"),
                "get_exposure" => json!(2000),
                _ => json!(0),
            })
        });
        assert_eq!(ran, ["get_app_state", "loop", "get_exposure"]);
        assert_eq!(
            r.as_deref(),
            Some(concat!(
                r#"[{"jsonrpc":"2.0","result":"Guiding","id":1},"#,
                r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"invalid request - missing method"},"id":null},"#,
                r#"{"jsonrpc":"2.0","result":2000,"id":2},"#,
                r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"method not found"},"id":3}]"#
            ))
        );
        // Nothing to answer: nothing sent, not even `[]`.
        assert_eq!(zero(r#"[{"method":"loop"},{"method":"nope"}]"#), None);
        assert_eq!(zero("[]"), None);
        assert_eq!(
            zero(r#"[{"method":"loop","id":9}]"#).as_deref(),
            Some(r#"[{"jsonrpc":"2.0","result":0,"id":9}]"#)
        );
    }

    #[test]
    fn a_call_without_outcome_gets_internal_error() {
        let parsed = parse(&Input::Line(br#"[{"method":"loop","id":1},{"method":"guide","id":2}]"#.to_vec()));
        assert_eq!(
            parsed.reply(vec![Ok(json!(0))]).as_deref(),
            Some(r#"[{"jsonrpc":"2.0","result":0,"id":1},{"jsonrpc":"2.0","error":{"code":-32603,"message":"internal error"},"id":2}]"#)
        );
    }

    #[test]
    fn ids_are_echoed_as_phd2_formats_them() {
        let id = |line: &str| {
            let r = zero(line).unwrap();
            r[r.find(",\"id\":").unwrap() + 6..r.len() - 1].to_string()
        };
        assert_eq!(id(r#"{"method":"loop","id":5}"#), "5");
        assert_eq!(id(r#"{"method":"loop","id":-12}"#), "-12");
        assert_eq!(id(r#"{"method":"loop","id":"abc"}"#), r#""abc""#);
        assert_eq!(id(r#"{"method":"loop","id":1.0}"#), "1");
        assert_eq!(id(r#"{"method":"loop","id":0.1}"#), "0.1");
        assert_eq!(id(r#"{"method":"loop","id":1e10}"#), "1e+10");
        assert_eq!(id(r#"{"method":"loop","id":4294967301}"#), "5");
        assert_eq!(id(r#"{"method":"loop","id":true}"#), "true");
        assert_eq!(id(r#"{"method":"loop","id":[1,"a",{"b":2.5}]}"#), r#"[1,"a",{"b":2.5}]"#);
        assert_eq!(id(r#"{"method":"loop","id":"a\"b\\c\r\n\/"}"#), r#""a\"b\\c\r\n/""#);
        assert_eq!(id(r#"{"method":"loop","id":"a\tb\u00e9"}"#), "\"a\tbé\"");
    }

    #[test]
    fn reply_shapes() {
        assert_eq!(reply(&Ok(json!(0)), &json!(42)), r#"{"jsonrpc":"2.0","result":0,"id":42}"#);
        assert_eq!(
            reply(&Ok(json!({"name": "Sim \"1\"", "id": 1})), &json!(3)),
            r#"{"jsonrpc":"2.0","result":{"id":1,"name":"Sim \"1\""},"id":3}"#
        );
        assert_eq!(reply(&Ok(Value::Null), &json!(1)), r#"{"jsonrpc":"2.0","result":null,"id":1}"#);
        assert_eq!(
            reply(&Err(RpcError::new(1, "all equipment must be connected first")), &json!(8)),
            r#"{"jsonrpc":"2.0","error":{"code":1,"message":"all equipment must be connected first"},"id":8}"#
        );
        assert_eq!(
            reply(&Err(RpcError::invalid_params("expected param \"mount\", \"ao\", or \"both\"")), &json!(2)),
            r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"expected param \"mount\", \"ao\", or \"both\""},"id":2}"#
        );
    }

    #[test]
    fn params_positional_and_named() {
        const NAMES: &[&str] = &["x", "y", "exact"];
        let pos = json!([1, 2.5]);
        let p = Params::new(NAMES, Some(&pos));
        assert_eq!(p.param("x"), Some(&json!(1)));
        assert_eq!(p.param("y"), Some(&json!(2.5)));
        assert_eq!(p.param("exact"), None);
        assert_eq!(p.param("z"), None);
        let extra = json!([1, 2, true, 9]);
        assert_eq!(Params::new(NAMES, Some(&extra)).param("exact"), Some(&json!(true)));
        let named = json!({"y": 2, "exact": false, "x": 1, "other": 3});
        let p = Params::new(NAMES, Some(&named));
        assert_eq!((p.param("x"), p.param("exact")), (Some(&json!(1)), Some(&json!(false))));
        // Named lookups need no name list.
        assert_eq!(Params::new(&[], Some(&named)).param("other"), Some(&json!(3)));
        assert_eq!(Params::new(NAMES, None).param("x"), None);
        // Through a parsed call.
        let parsed = parse(&Input::Line(br#"{"method":"set_lock_position","params":[100.5,200,true],"id":1}"#.to_vec()));
        let call = parsed.calls().next().unwrap();
        assert_eq!(call.params().param("y").and_then(float_param), Some(200.0));
        assert_eq!(call.params().param("exact").and_then(bool_param), Some(true));
    }

    #[test]
    fn typed_getters() {
        assert_eq!(bool_param(&json!(true)), Some(true));
        assert_eq!(bool_param(&json!(0)), Some(false));
        assert_eq!(bool_param(&json!(2)), Some(true));
        assert_eq!(bool_param(&json!(4294967296u64)), Some(false)); // wraps to 0
        assert_eq!(bool_param(&json!(1.0)), None);
        assert_eq!(bool_param(&json!("true")), None);
        assert_eq!(float_param(&json!(3)), Some(3.0));
        assert_eq!(float_param(&json!(-1.5)), Some(-1.5));
        assert_eq!(float_param(&json!(4294967301u64)), Some(5.0));
        assert_eq!(float_param(&json!("1")), None);
        assert_eq!(int_param(&json!(5)), Some(5));
        assert_eq!(int_param(&json!(-7)), Some(-7));
        assert_eq!(int_param(&json!(5.0)), None);
        assert_eq!(int_param(&json!(-4294967297i64)), Some(-1));
        assert_eq!(string_param(&json!("mount")), Some("mount"));
        assert_eq!(string_param(&json!(1)), None);
        assert_eq!(rect_param(&json!([1, 2, 30, 40])), Some(Rect::new(1, 2, 30, 40)));
        assert_eq!(rect_param(&json!([1, 2, 30])), None);
        assert_eq!(rect_param(&json!([1, 2, 30, 40, 5])), None);
        assert_eq!(rect_param(&json!([1, 2, 30.0, 40])), None);
        assert_eq!(rect_param(&json!({"x": 1})), None);
        assert_eq!(point_param(&json!([1, 2.5])), Some(Point { x: 1.0, y: 2.5 }));
        assert_eq!(point_param(&json!([1])), None);
        assert_eq!(point_param(&json!([1, 2, 3])), None);
        assert_eq!(point_param(&json!([1, "2"])), None);
    }

    #[test]
    fn roi() {
        assert_eq!(roi_param(None), Ok(None));
        assert_eq!(roi_param(Some(&json!([0, 0, 0, 0]))), Ok(Some(Rect::default())));
        assert_eq!(roi_param(Some(&json!([10, 20, 100, 80]))), Ok(Some(Rect::new(10, 20, 100, 80))));
        assert_eq!(roi_param(Some(&json!(null))), Err(RpcError::new(-32602, "invalid ROI param")));
        assert_eq!(roi_param(Some(&json!([1, 2, 3]))), Err(RpcError::new(-32602, "invalid ROI param")));
    }

    #[test]
    fn settle() {
        let expected_object = Err(RpcError::new(-32602, "expected settle object param"));
        let invalid = Err(RpcError::new(-32602, "invalid settle params"));
        assert_eq!(settle_param(None), expected_object);
        assert_eq!(settle_param(Some(&json!([1.5, 10, 60]))), expected_object);
        assert_eq!(settle_param(Some(&json!({"pixels": 1.5, "time": 10}))), invalid);
        assert_eq!(settle_param(Some(&json!({"pixels": "1.5", "time": 10, "timeout": 60}))), invalid);
        assert_eq!(settle_param(Some(&json!({"pixels": 1.5, "time": true, "timeout": 60}))), invalid);
        assert_eq!(
            settle_param(Some(&json!({"pixels": 1.5, "time": 10.9, "timeout": 60, "frames": 3, "arcsecs": 2}))),
            Ok(SettleParams { tolerance_px: 1.5, settle_time_s: 10, timeout_s: 60, frames: 99999 })
        );
        assert_eq!(
            settle_param(Some(&json!({"pixels": 2, "time": -0.5, "timeout": 30.0}))),
            Ok(SettleParams { tolerance_px: 2.0, settle_time_s: -1, timeout_s: 30, frames: 99999 })
        );
    }

    #[test]
    fn numbers_carry_phd2_digits() {
        let text = |v: Value| v.to_string();
        assert_eq!(text(num_g(0.5)), "0.5");
        assert_eq!(text(num_g(2.0)), "2");
        assert_eq!(text(num_g(100000.0)), "100000");
        assert_eq!(text(num_g(1.23456789)), "1.23457");
        assert_eq!(text(num_g(-0.000123456)), "-0.000123456");
        // %g's "1.23457e+06": the value is PHD2's, the spelling serde_json's.
        assert_eq!(text(num_g(1234567.0)), "1234570.0");
        assert_eq!(text(num_g(f64::NAN)), "null");
        assert_eq!(text(num_f(1.5, 3)), "1.5");
        assert_eq!(text(num_f(2.0, 2)), "2.0");
        assert_eq!(text(num_f(0.125, 2)), "0.12");
        assert_eq!(text(num_f(1731340000.123456, 3)), "1731340000.123");
        assert_eq!(text(num_f(-3.0, 0)), "-3");
        assert_eq!(text(num_f(f64::INFINITY, 1)), "null");
        assert_eq!(text(num_point(1.234, 5.678)), "[1.23,5.68]");
        assert_eq!(text(num_point(0.375, -2.0)), "[0.38,-2.0]");
        // In a reply they keep that spelling.
        assert_eq!(reply(&Ok(num_g(0.75)), &json!(1)), r#"{"jsonrpc":"2.0","result":0.75,"id":1}"#);
    }

    #[test]
    fn method_table() {
        let phd2: Vec<&str> = METHODS.iter().filter(|m| !m.ws_only).map(|m| m.name).collect();
        assert_eq!(phd2.len(), 54);
        assert_eq!(phd2[..3], ["clear_calibration", "deselect_star", "get_exposure"]);
        assert_eq!(phd2[51..], ["set_variable_delay_settings", "get_limit_frame", "set_limit_frame"]);
        // PHD2 methods first, then the extras.
        assert!(METHODS[..54].iter().all(|m| !m.ws_only));
        let mut names: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), METHODS.len());
        let params = |name| method(name, false).unwrap().params;
        assert_eq!(params("guide"), ["settle", "recalibrate", "roi"]);
        assert_eq!(params("dither"), ["amount", "raOnly", "settle"]);
        assert_eq!(params("capture_single_frame"), ["exposure", "binning", "gain", "subframe", "path", "save"]);
        assert_eq!(params("set_variable_delay_settings"), ["Enabled", "ShortDelaySeconds", "LongDelaySeconds"]);
        assert_eq!(params("get_ccd_temperature"), [] as [&str; 0]);
    }

    /// The UI reaches the engine through this table only: each command it
    /// can ask for has its method, a PHD2 one or the UI's own.
    #[test]
    fn every_command_has_a_method() {
        use crate::engine::algo::{AlgoKind, Axis};
        use crate::engine::assistant::RecKind;
        use crate::engine::calibration::Calibration;
        use crate::engine::control::DitherMode;
        use crate::engine::guider::ShiftUnits;
        use crate::engine::log::image_log::ImageLoggerSettings;
        use crate::engine::scope::DecGuideMode;
        use crate::engine::traits::Dir;
        use crate::engine::{Command as C, DarksBuildKind, Gear, SingleFrame, Which};
        let settle = SettleParams::default();
        let commands = [
            C::Loop,
            C::StopCapture,
            C::Guide { settle, recalibrate: false, roi: None },
            C::Dither { amount: 1.0, ra_only: false, settle },
            C::SetPaused { paused: true, full: false },
            C::FindStar { roi: None },
            C::DeselectStar,
            C::SetLockPosition { x: 1.0, y: 1.0, exact: true },
            C::ClearCalibration(Which::Both),
            C::FlipCalibration,
            C::SetExposure(1000),
            C::SetDecGuideMode(DecGuideMode::Auto),
            C::SetGuideOutputEnabled(true),
            C::SetAlgoParam { axis: Axis::X, name: String::new(), value: 0.0 },
            C::GuidePulse { amount: 100, dir: Dir::North, which: Which::Mount },
            C::CaptureSingleFrame(SingleFrame::default()),
            C::SetLockShiftEnabled(true),
            C::SetLockShiftParams { rate: Point::default(), units: ShiftUnits::Pixels, mount_coords: false },
            C::SetVariableDelay { enabled: false, short_ms: 0, long_ms: 0 },
            C::SetLimitFrame(None),
            C::SetCoolerState(true),
            C::SaveImage,
            C::Attach(Gear::default()),
            C::Detach,
            C::ProfileChanged,
            C::Connecting(true),
            C::SelectStar { x: 1.0, y: 1.0 },
            C::SetCalibrationData { which: Which::Mount, cal: Calibration::default() },
            C::NudgeLock { dx: 1.0, dy: 0.0 },
            C::SetAlgorithm { axis: Axis::Y, kind: AlgoKind::Identity },
            C::ResetAlgoParams { axis: Axis::Y },
            C::SetDitherMode { mode: DitherMode::Random, ra_only: false, scale: 1.0 },
            C::SetAutoExposure { min_ms: 100, max_ms: 5000, target_snr: 6.0 },
            C::SetImageLogger(ImageLoggerSettings::default()),
            C::SetManualPointing(None),
            C::GaOpen,
            C::GaStart,
            C::GaStop,
            C::GaEndWait { accept: false },
            C::GaSetMeasureBacklash(true),
            C::GaApply(RecKind::Exp),
            C::GaReview(String::new()),
            C::GaClose,
            C::DarksBuild(DarksBuildKind::DefectMap { exp_s: 15, count: 10, note: String::new() }),
            C::DarksStop,
            C::DarksUse(true),
            C::DefectMapUse(true),
            C::PolarDriftOpen,
            C::PolarDriftStart,
            C::PolarDriftStop,
            C::PolarDriftClose,
        ];
        for c in &commands {
            assert!(method(c.name(), true).is_some(), "no method for {}", c.name());
        }
        // The daemon's own, no client's: the 1 s tick and a device gone.
        assert_eq!((C::Tick.name(), C::DeviceLost(Which::Mount).name()), ("tick", "device_lost"));
        assert_eq!(method("ga_backlash_start", true), None);
    }

    #[test]
    fn relay_calls() {
        let c = call(json!({"method": "select_star", "params": [1, 2], "id": 3}), true).unwrap();
        assert_eq!((c.method.name, c.params, c.id), ("select_star", Some(json!([1, 2])), Some(json!(3))));
        assert_eq!(call(json!({"method": "loop"}), true).map(|c| c.id), Ok(None));
        let missing = Err(RpcError::new(INVALID_REQUEST, "invalid request - missing method"));
        assert_eq!(call(json!({"params": [], "id": 1}), true), missing);
        // The method is checked before the params.
        assert_eq!(call(json!({"method": 5, "params": 3}), true), missing);
        assert_eq!(
            call(json!({"method": "loop", "params": "x"}), true),
            Err(RpcError::new(INVALID_REQUEST, "invalid request - params must be an array or object"))
        );
        let not_found = Err(RpcError::new(METHOD_NOT_FOUND, "method not found"));
        assert_eq!(call(json!({"method": "select_star", "id": 1}), false), not_found);
        assert_eq!(call(json!({"method": "nope"}), true), not_found);
    }

    /// The lines Ekos writes (`phd2.cpp` `sendRpcCall`: a compact
    /// `QJsonDocument`, keys sorted, `\r\n`), whole numbers of a double
    /// written without a point.
    #[test]
    fn ekos_requests() {
        let mut r = LineReader::default();
        let mut seen = Vec::new();
        let wire = concat!(
            "{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"get_app_state\"}\r\n",
            "{\"method\":\"get_app_state\",\"id\":5,\"jsonrpc\":\"2.0\"}\r\n",
            "{\"id\":2,\"jsonrpc\":\"2.0\",\"method\":\"set_connected\",\"params\":{\"connected\":true}}\r\n",
            "{\"id\":3,\"jsonrpc\":\"2.0\",\"method\":\"guide\",\"params\":{\"recalibrate\":false,\"settle\":{\"pixels\":1.5,\"time\":10,\"timeout\":60}}}\r\n",
            "{\"id\":4,\"jsonrpc\":\"2.0\",\"method\":\"dither\",\"params\":{\"amount\":5,\"raOnly\":false,\"settle\":{\"pixels\":2,\"time\":10,\"timeout\":60}}}\r\n",
            "{\"id\":6,\"jsonrpc\":\"2.0\",\"method\":\"set_paused\",\"params\":{\"paused\":true,\"type\":\"full\"}}\r\n",
            "{\"id\":7,\"jsonrpc\":\"2.0\",\"method\":\"set_paused\",\"params\":{\"paused\":false}}\r\n",
            "{\"id\":8,\"jsonrpc\":\"2.0\",\"method\":\"set_lock_position\",\"params\":{\"exact\":false,\"x\":512.25,\"y\":384.75}}\r\n",
            "{\"id\":9,\"jsonrpc\":\"2.0\",\"method\":\"set_exposure\",\"params\":{\"exposure\":2000}}\r\n",
            "{\"id\":10,\"jsonrpc\":\"2.0\",\"method\":\"set_dec_guide_mode\",\"params\":{\"mode\":\"Auto\"}}\r\n",
            "{\"id\":11,\"jsonrpc\":\"2.0\",\"method\":\"get_star_image\",\"params\":{\"size\":32}}\r\n",
            "{\"id\":12,\"jsonrpc\":\"2.0\",\"method\":\"clear_calibration\",\"params\":{\"which\":\"mount\"}}\r\n",
            "{\"id\":13,\"jsonrpc\":\"2.0\",\"method\":\"stop_capture\"}\r\n",
        );
        let mut replies = Vec::new();
        for input in r.feed(wire.as_bytes()) {
            let parsed = parse(&input);
            let outcomes = parsed
                .calls()
                .map(|c| {
                    let p = c.params();
                    let s = |k| p.param(k).map(|v| v.to_string()).unwrap_or_default();
                    seen.push(match c.method.name {
                        "set_connected" => format!("connected={:?}", p.param("connected").and_then(bool_param)),
                        "guide" => format!(
                            "guide {:?} recal={:?} roi={:?}",
                            settle_param(p.param("settle")),
                            p.param("recalibrate").and_then(bool_param),
                            roi_param(p.param("roi"))
                        ),
                        "dither" => format!(
                            "dither {:?} {:?} {:?}",
                            p.param("amount").and_then(float_param),
                            p.param("raOnly").and_then(bool_param),
                            settle_param(p.param("settle"))
                        ),
                        "set_paused" => format!("paused={} type={}", s("paused"), s("type")),
                        "set_lock_position" => format!(
                            "lock {:?} {:?} {:?}",
                            p.param("x").and_then(float_param),
                            p.param("y").and_then(float_param),
                            p.param("exact").and_then(bool_param)
                        ),
                        "set_exposure" => format!("exposure={:?}", p.param("exposure").and_then(int_param)),
                        "set_dec_guide_mode" => format!("mode={:?}", p.param("mode").and_then(string_param)),
                        "get_star_image" => format!("size={:?}", p.param("size").and_then(int_param)),
                        "clear_calibration" => format!("which={:?}", p.param("which").and_then(string_param)),
                        name => name.to_string(),
                    });
                    Ok(json!(0))
                })
                .collect();
            replies.push(parsed.reply(outcomes).unwrap());
        }
        let settle = |px: f64| format!("{:?}", Ok::<_, RpcError>(SettleParams { tolerance_px: px, settle_time_s: 10, timeout_s: 60, frames: 99999 }));
        assert_eq!(
            seen,
            [
                "get_app_state".to_string(),
                "get_app_state".into(),
                "connected=Some(true)".into(),
                format!("guide {} recal=Some(false) roi=Ok(None)", settle(1.5)),
                format!("dither Some(5.0) Some(false) {}", settle(2.0)),
                "paused=true type=\"full\"".into(),
                "paused=false type=".into(),
                "lock Some(512.25) Some(384.75) Some(false)".into(),
                "exposure=Some(2000)".into(),
                "mode=Some(\"Auto\")".into(),
                "size=Some(32)".into(),
                "which=Some(\"mount\")".into(),
                "stop_capture".into(),
            ]
        );
        let ids: Vec<String> = [1, 5, 2, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13]
            .iter()
            .map(|id| format!("{{\"jsonrpc\":\"2.0\",\"result\":0,\"id\":{id}}}"))
            .collect();
        assert_eq!(replies, ids);
    }
}
