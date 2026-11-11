//! PHD2's guide log (`PHD2_GuideLog_*.txt`, log version 2.5), the file PHD2
//! Log Viewer and the guiding-analysis tools read. A calibration section and
//! a guiding section each open with a header of the settings in force, then
//! one CSV row per calibration step or guide frame; `INFO:` lines mark
//! dithers, settling, lock moves, server commands and parameter changes. A
//! file collects an imaging day and closes on a running summary that the
//! next opening of the same file reads back.
//!
//! Every line is PHD2's to the byte: the headers come from a
//! [`GuideLogHeader`] the engine fills when a section starts (PHD2 reads the
//! same values from its frame, guider, camera and mounts at that moment),
//! the rows from [`GuideStepInfo`] and [`CalibrationStepInfo`], the rest
//! from the events of the guider, the mounts, the controller and the
//! Guiding Assistant, one writer per event. The writer appends to a `Write`
//! sink — the file, opened by the caller in append mode — and flushes after
//! every record; it never reads the clock or the disk.
//!
//! Most `INFO:` lines are written only from the start of a calibration or
//! of guiding to its end (PHD2's `m_isGuiding`); settling changes, Guiding
//! Assistant results and star losses during calibration always are. A new
//! file is deleted at close unless something meaningful got logged in it:
//! a calibration or guiding header, or one of the lock position, lock
//! shift, server command, manual guide or parameter change lines — not a
//! dither, settling, star-lost or GA line. The writers of the `INFO:` and
//! calibration lines hand back what they wrote, without its newline
//! (`None` when nothing was written), for the relay's `guider_log`.
//!
//! Ported from PHD2 src/guidinglog.cpp, src/guidinglog.h and the settings summaries of src/myframe.cpp, src/camera.cpp, src/mount.cpp, src/scope.cpp, src/stepguider.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::io::{self, Write};

use chrono::{DateTime, Local};
use serde_json::Value;

use super::cfmt::fmt_f;
use super::debug_log::{LogFileStamp, PHD2_VERSION, remove_matching_files};
use crate::engine::algo::AlgoKind;
use crate::engine::calibration::{
    CalibrationDetails, MountCalibration, ROTATOR_POSITION_UNKNOWN, UNKNOWN_DECLINATION, degrees, norm, radians,
};
use crate::engine::guider::FrameDroppedInfo;
use crate::engine::image::NoiseReduction;
use crate::engine::mount::{CalibrationStepInfo, GuideStepInfo};
use crate::engine::scope::DecGuideMode;
use crate::engine::traits::{Dir, Parity, PierSide, Point};

/// Days a guide log is kept (`guidinglog.cpp:42`).
pub const RETENTION_DAYS: u64 = 60;

/// How much of an existing file's end its summary is read back from
/// (`guidinglog.cpp:222`).
pub const SUMMARY_TAIL_BYTES: u64 = 128;

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

const COLUMNS: &str = "Frame,Time,mount,dx,dy,RARawDistance,DECRawDistance,RAGuideDistance,DECGuideDistance,\
                       RADuration,RADirection,DECDuration,DECDirection,XStep,YStep,StarMass,SNR,ErrorCode\n";

/// Everything the calibration and guiding headers print, gathered by the
/// engine when a section starts.
#[derive(Clone, Debug, PartialEq)]
pub struct GuideLogHeader {
    /// `Equipment Profile = …`.
    pub profile: String,
    /// The frame's settings (`MyFrame::GetSettingsSummary`).
    pub dither_ra_only: bool,
    pub dither_scale: f64,
    pub noise_reduction: NoiseReduction,
    pub time_lapse_ms: i32,
    pub server_enabled: bool,
    /// arc-sec/px; exactly 1.0 (PHD2's unknown scale) prints "unspecified".
    pub pixel_scale: f64,
    pub binning: u16,
    /// mm; 0 prints "unspecified".
    pub focal_length: i32,
    /// `Guider::GetSettingsSummary`: [`MultiStar::settings_summary`](crate::engine::multistar::MultiStar::settings_summary),
    /// its newline included.
    pub guider_summary: String,
    /// `pCamera`: no camera, no camera line and no exposure line.
    pub camera: Option<CameraSummary>,
    pub exposure: ExposureSummary,
    /// `pMount`, then `pSecondaryMount` (the scope behind an AO).
    pub mount: Option<MountSummary>,
    pub secondary_mount: Option<MountSummary>,
    pub pointing: PointingSummary,
    pub lock_position: Point,
    pub star_position: Point,
    pub star_hfd: f64,
}

/// `GuideCamera::GetSettingsSummary`'s values (`camera.cpp:1365`).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraSummary {
    pub name: String,
    /// Set when the camera has a gain control.
    pub gain: Option<i32>,
    /// Set when the camera has a read delay.
    pub read_delay: Option<i32>,
    /// Set for a parallel-port camera.
    pub port: Option<u16>,
    pub frame_width: i32,
    pub frame_height: i32,
    /// The loaded dark's exposure, ms; 0 when there is none.
    pub dark_exposure_ms: i32,
    pub defect_map: bool,
    /// µm; 0 (PHD2's `UnknownPixelSize`) prints "unspecified".
    pub pixel_size: f64,
}

/// The exposure setting (`MyFrame::ExposureDurationSummary`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExposureSummary {
    pub duration_ms: i32,
    /// Auto exposure: min ms, max ms, target SNR.
    pub auto: Option<(i32, i32, f64)>,
    /// Variable delay: short ms, long ms.
    pub var_delay: Option<(i32, i32)>,
}

impl ExposureSummary {
    /// `MyFrame::ExposureDurationSummary` (`myframe.cpp:724`): the header's
    /// `Exposure = …` and the value of the `Exposure` parameter change.
    pub fn text(&self) -> String {
        let mut s = match self.auto {
            Some((min, max, snr)) => format!("Auto (min = {min} ms, max = {max} ms, SNR = {})", fmt_f(snr, 2)),
            None => format!("{} ms", self.duration_ms),
        };
        if let Some((short, long)) = self.var_delay {
            s += &format!(", VarDelay (short = {short} ms, long = {long} ms)");
        }
        s
    }
}

/// A mount's settings (`Mount::GetSettingsSummary` and the scope's or AO's
/// part).
#[derive(Clone, Debug, PartialEq)]
pub struct MountSummary {
    /// `INDI Mount [<device>]`, …
    pub name: String,
    pub connected: bool,
    pub guiding_enabled: bool,
    /// `AuxMount=…`: the pointing source's name when it is connected, can
    /// report its position and is not the scope.
    pub aux_mount: Option<String>,
    /// `IsCalibrated()`, the scope's override included: the calibration is
    /// printed only then.
    pub calibrated: bool,
    /// `m_cal` and `m_xRate`.
    pub cal: MountCalibration,
    /// `LoadCalibrationDetails`.
    pub details: CalibrationDetails,
    pub x_algo: AlgoKind,
    /// The X algorithm's `settings_summary()`, newline included.
    pub x_algo_summary: String,
    pub y_algo: AlgoKind,
    pub y_algo_summary: String,
    pub kind: MountKind,
}

/// What only a scope or only an AO prints.
#[derive(Clone, Debug, PartialEq)]
pub enum MountKind {
    Scope(ScopeSummary),
    Ao(AoSummary),
}

/// The scope's part (`scope.cpp:1885-1930`).
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeSummary {
    pub backlash_enabled: bool,
    pub backlash_pulse_ms: i32,
    pub max_ra_duration: i32,
    pub max_dec_duration: i32,
    pub dec_guide_mode: DecGuideMode,
    /// The pointing source's guide speeds, deg/s; `None` when it cannot
    /// tell.
    pub guide_speeds: Option<(f64, f64)>,
    /// The stored calibration's declination (`GetLastCalibration`):
    /// radians or [`UNKNOWN_DECLINATION`].
    pub last_cal_declination: f64,
    pub calibration_step_ms: i32,
    pub calibration_distance_px: i32,
    pub assume_orthogonal: bool,
}

/// The AO's part (`stepguider.cpp:1288-1302`).
#[derive(Clone, Debug, PartialEq)]
pub struct AoSummary {
    pub bump_percentage: i32,
    pub bump_step: f64,
    pub calibration_steps: i32,
    pub samples_to_average: i32,
}

/// Where the pointing source points (`PointingInfo`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PointingSummary {
    /// RA (h), Dec (°) and local sidereal time (h); `None` without a
    /// pointing source or when it cannot tell.
    pub coords: Option<(f64, f64, f64)>,
    pub pier_side: PierSide,
    /// The site's latitude, °; `None` when the mount does not know it.
    pub latitude: Option<f64>,
    /// `None` without a rotator ("N/A"); [`ROTATOR_POSITION_UNKNOWN`]
    /// prints "Unknown"; else the angle, °.
    pub rotator: Option<f64>,
}

/// `GuideLogSummaryInfo` (`guidinglog.h:100`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Summary {
    /// False when an existing file had no summary to read back: no
    /// `Log Summary` line is written at close then.
    valid: bool,
    cal_cnt: u32,
    guide_cnt: u32,
    guide_dur: f64,
    ga_cnt: u32,
}

impl Summary {
    /// `LoadSummaryInfo` (`guidinglog.cpp:194`): the counters of the first
    /// `Log Summary: ` line in the file's tail.
    fn load(tail: &[u8]) -> Summary {
        let text = String::from_utf8_lossy(tail);
        // wxTextInputStream ends a line at \n, \r\n or \r.
        let Some(line) = text.split(['\n', '\r']).find(|l| l.starts_with("Log Summary: ")) else {
            return Summary::default();
        };
        let after = |key: &str| line.find(key).map_or("", |p| &line[p + key.len()..]);
        Summary {
            valid: true,
            cal_cnt: strtoul(after("calcnt:")),
            guide_cnt: strtoul(after("gcnt:")),
            guide_dur: strtod(after("gdur:")),
            ga_cnt: strtoul(after("gacnt:")),
        }
    }
}

/// `strtoul` cast to `unsigned int`, on the digits a summary holds.
fn strtoul(s: &str) -> u32 {
    s.trim_start()
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0u32, |n, d| n.wrapping_mul(10).wrapping_add(u32::from(d - b'0')))
}

/// `strtod` on the `%.f` a summary holds.
fn strtod(s: &str) -> f64 {
    let s = s.trim_start();
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    s[..end].parse().unwrap_or(0.0)
}

/// `mount.cpp`'s own `ParityStr`, which `Mount::GetSettingsSummary` sees —
/// not the guide log's `Even`/`Odd` of the calibration lines.
fn parity_sign(p: Parity) -> &'static str {
    match p {
        Parity::Even => "+",
        Parity::Odd => "-",
        Parity::Unknown => "?",
    }
}

/// `Mount::DeclinationStr(dec, "%0.1f")`.
fn declination_str(dec: f64) -> String {
    if dec == UNKNOWN_DECLINATION { "Unknown".into() } else { fmt_f(degrees(dec), 1) }
}

/// `GuideSpeedSummary` (`scope.cpp:1885`).
fn guide_speed_summary(speeds: Option<(f64, f64)>) -> String {
    match speeds {
        Some((ra, dec)) => format!(
            "RA Guide Speed = {} a-s/s, Dec Guide Speed = {} a-s/s",
            fmt_f(3600.0 * ra, 1),
            fmt_f(3600.0 * dec, 1)
        ),
        None => "RA Guide Speed = Unknown, Dec Guide Speed = Unknown".into(),
    }
}

/// `MyFrame::GetSettingsSummary` (`myframe.cpp:2815`) with
/// `PixelScaleSummary` (`:2785`).
fn frame_summary(h: &GuideLogHeader) -> String {
    let scale = if h.pixel_scale == 1.0 {
        "unspecified".to_string()
    } else {
        format!("{} arc-sec/px", fmt_f(h.pixel_scale, 2))
    };
    let focal = if h.focal_length == 0 { "unspecified".to_string() } else { format!("{} mm", h.focal_length) };
    format!(
        "Dither = {}, Dither scale = {}, Image noise reduction = {}, Guide-frame time lapse = {}, Server {}\n\
         Pixel scale = {scale}, Binning = {}, Focal length = {focal}\n",
        if h.dither_ra_only { "RA only" } else { "both axes" },
        fmt_f(h.dither_scale, 3),
        h.noise_reduction.label(),
        h.time_lapse_ms,
        if h.server_enabled { "enabled" } else { "disabled" },
        h.binning
    )
}

/// `GuideCamera::GetSettingsSummary` (`camera.cpp:1365`).
fn camera_summary(c: &CameraSummary) -> String {
    let opt = |label: &str, v: Option<i32>| v.map_or(String::new(), |v| format!(", {label} = {v}"));
    let port = c.port.map_or(String::new(), |p| format!(", port = 0x{p:x}"));
    let dark = match c.dark_exposure_ms {
        0 => "no dark".to_string(),
        ms => format!("have dark, dark dur = {ms}"),
    };
    let pixel = if c.pixel_size == 0.0 { "unspecified".to_string() } else { format!("{} um", fmt_f(c.pixel_size, 1)) };
    format!(
        "Camera = {}{}{}{port}, full size = {} x {}, {dark}, {}, pixel size = {pixel}\n",
        c.name,
        opt("gain", c.gain),
        opt("delay", c.read_delay),
        c.frame_width,
        c.frame_height,
        if c.defect_map { "defect map in use" } else { "no defect map" }
    )
}

/// `Mount::GetSettingsSummary` (`mount.cpp:1878`) followed by
/// `Scope::GetSettingsSummary`'s (`scope.cpp:1904`) or
/// `StepGuider::GetSettingsSummary`'s (`stepguider.cpp:1288`) lines.
pub(crate) fn mount_summary(m: &MountSummary) -> String {
    let ao = matches!(m.kind, MountKind::Ao(_));
    let mut s = format!(
        "{} = {},{} connected, guiding {}, ",
        if ao { "AO" } else { "Mount" },
        m.name,
        if m.connected { "" } else { " not" },
        if m.guiding_enabled { "enabled" } else { "disabled" }
    );
    if let Some(aux) = &m.aux_mount {
        s += &format!("AuxMount={aux}, ");
    }
    if m.calibrated {
        let cal = &m.cal.cal;
        // xRate() is m_xRate (Dec-compensated), yRate() the calibration's.
        let y_rate_px = cal.y_rate * 1000.0;
        s += &format!(
            "xAngle = {}, xRate = {}, yAngle = {}, yRate = {}, parity = {}/{}\n",
            fmt_f(degrees(cal.x_angle), 1),
            fmt_f(m.cal.x_rate * 1000.0, 3),
            fmt_f(degrees(cal.y_angle()), 1),
            fmt_f(y_rate_px, 3),
            parity_sign(cal.ra_guide_parity),
            parity_sign(cal.dec_guide_parity)
        );
        let valid = m.details.is_valid();
        let scale = if valid { m.details.image_scale } else { 1.0 };
        let ortho = if valid { format!("{} deg", fmt_f(m.details.ortho_error, 1)) } else { "unknown".into() };
        if ao {
            s += &format!(
                "Norm rates X = {} mas/step, Y = {} mas/step; ortho.err. = {ortho}\n",
                fmt_f(cal.x_rate * 1000.0 * scale, 0),
                fmt_f(cal.y_rate * 1000.0 * scale, 0)
            );
        } else {
            let x_rate_d0 = if cal.declination != UNKNOWN_DECLINATION {
                cal.x_rate * 1000.0 / cal.declination.cos() * scale
            } else {
                cal.x_rate * 1000.0 * scale
            };
            s += &format!(
                "Norm rates RA = {}\"/s @ dec 0, Dec = {}\"/s; ortho.err. = {ortho}\n",
                fmt_f(x_rate_d0, 1),
                fmt_f(y_rate_px * scale, 1)
            );
        }
    } else {
        s += "not calibrated\n";
    }
    s += &format!("X guide algorithm = {}, {}", m.x_algo.ui_name(), m.x_algo_summary);
    s += &format!("Y guide algorithm = {}, {}", m.y_algo.ui_name(), m.y_algo_summary);
    match &m.kind {
        MountKind::Scope(sc) => {
            s += &format!(
                "Backlash comp = {}, pulse = {} ms\n",
                if sc.backlash_enabled { "enabled" } else { "disabled" },
                sc.backlash_pulse_ms
            );
            s += &format!(
                "Max RA duration = {}, Max DEC duration = {}, DEC guide mode = {}\n",
                sc.max_ra_duration,
                sc.max_dec_duration,
                sc.dec_guide_mode.label()
            );
            s += &format!(
                "{}, Cal Dec = {}, Last Cal Issue = {}, Timestamp = {}\n",
                guide_speed_summary(sc.guide_speeds),
                declination_str(sc.last_cal_declination),
                m.details.last_issue.text(),
                m.details.orig_timestamp
            );
        }
        MountKind::Ao(a) => {
            s += &format!(
                "Bump percentage = {}, Bump step = {}, Timestamp = {}\n",
                a.bump_percentage,
                fmt_f(a.bump_step, 2),
                m.details.orig_timestamp
            );
        }
    }
    s
}

/// `CalibrationSettingsSummary` of the scope (`scope.cpp:1925`) or the AO
/// (`stepguider.cpp:1298`).
pub(crate) fn calibration_settings(m: &MountSummary) -> String {
    match &m.kind {
        MountKind::Scope(sc) => format!(
            "Calibration Step = {} ms, Calibration Distance = {} px, Assume orthogonal axes = {}\n{}",
            sc.calibration_step_ms,
            sc.calibration_distance_px,
            if sc.assume_orthogonal { "yes" } else { "no" },
            guide_speed_summary(sc.guide_speeds)
        ),
        MountKind::Ao(a) => {
            format!("Calibration steps = {}, Samples to average = {}", a.calibration_steps, a.samples_to_average)
        }
    }
}

/// `GetAltAz` (`guidinglog.cpp:90`): latitude and declination in degrees,
/// hour angle in hours; altitude and azimuth in degrees.
fn alt_az(latitude: f64, ha: f64, dec: f64) -> (f64, f64) {
    const HRS_TO_RAD: f64 = 0.2617993881;
    let (dec_r, lat_r, ha_r) = (radians(dec), radians(latitude), ha * HRS_TO_RAD);
    let sin_alt = ha_r.cos() * dec_r.cos() * lat_r.cos() + dec_r.sin() * lat_r.sin();
    let alt = degrees(sin_alt.asin());
    // Near the zenith the azimuth is arbitrary.
    let z_sin = sin_alt.acos().sin();
    if z_sin.abs() < 1e-5 {
        return (90.0, if ha_r.sin() >= 0.0 { 270.0 } else { 90.0 });
    }
    let a_s = dec_r.cos() * ha_r.sin() / z_sin;
    let a_c = (lat_r.sin() * dec_r.cos() * ha_r.cos() - lat_r.cos() * dec_r.sin()) / z_sin;
    if a_c == 0.0 && a_s == 0.0 {
        return (alt, if dec > 0.0 { 180.0 } else { 0.0 });
    }
    (alt, degrees(a_s.atan2(a_c)) + 180.0)
}

/// `PointingInfo` (`guidinglog.cpp:135`), without its newline.
fn pointing_info(p: &PointingSummary) -> String {
    let rotator = match p.rotator {
        None => "N/A".to_string(),
        Some(r) if r == ROTATOR_POSITION_UNKNOWN => "Unknown".to_string(),
        Some(r) => fmt_f(norm(r, 0.0, 360.0), 1),
    };
    let Some((ra, dec, st)) = p.coords else {
        return format!(
            "RA/Dec = Unknown, Hour angle = Unknown, Pier side = Unknown, Rotator pos = {rotator}, Alt = Unknown, Az = Unknown"
        );
    };
    let ha = norm(st - ra, -12.0, 12.0);
    // Deviation: PHD2 prints its uninitialised alt/az when the mount cannot
    // tell the site (undefined behaviour); the port prints zeros.
    let (alt, az) = p.latitude.map_or((0.0, 0.0), |lat| alt_az(lat, ha, dec));
    format!(
        "RA = {} hr, Dec = {} deg, Hour angle = {} hr, Pier side = {}, Rotator pos = {rotator}, Alt = {} deg, Az = {} deg",
        fmt_f(ra, 2),
        fmt_f(dec, 1),
        fmt_f(ha, 2),
        p.pier_side.label(),
        fmt_f(alt, 1),
        fmt_f(az, 1)
    )
}

/// The lines both sections open with: profile, frame, guider, camera and
/// exposure.
fn common_header(h: &GuideLogHeader) -> String {
    let mut s = format!("Equipment Profile = {}\n", h.profile);
    s += &frame_summary(h);
    s += &h.guider_summary;
    if let Some(c) = &h.camera {
        s += &camera_summary(c);
        s += &format!("Exposure = {}\n", h.exposure.text());
    }
    s
}

/// The pointing line, then the lock and star line.
fn position_lines(h: &GuideLogHeader) -> String {
    format!(
        "{}\nLock position = {}, {}, Star position = {}, {}, HFD = {} px\n",
        pointing_info(&h.pointing),
        fmt_f(h.lock_position.x, 3),
        fmt_f(h.lock_position.y, 3),
        fmt_f(h.star_position.x, 3),
        fmt_f(h.star_position.y, 3),
        fmt_f(h.star_hfd, 2)
    )
}

/// `GuidingHeader` (`guidinglog.cpp:171`).
fn guiding_header(h: &GuideLogHeader) -> String {
    let mut s = common_header(h);
    for m in [&h.mount, &h.secondary_mount].into_iter().flatten() {
        s += &mount_summary(m);
    }
    s += &position_lines(h);
    s += COLUMNS;
    s
}

/// `GuidingLog::RemoveOldFiles`: guide logs older than [`RETENTION_DAYS`],
/// any instance (PHD2 runs it at start-up). Returns the debug-log record.
pub fn remove_old_files(
    files: &[(String, DateTime<Local>)],
    now: DateTime<Local>,
    remove: &mut dyn FnMut(&str),
) -> Option<String> {
    remove_matching_files(files, "PHD2_GuideLog*.txt", RETENTION_DAYS, now, remove)
}

/// PHD2's `GuidingLog` (`guidinglog.h:117`). `W` is the file (append mode)
/// in the daemon, a buffer in the tests.
pub struct GuideLog<W: Write = Box<dyn Write + Send>> {
    enabled: bool,
    file: Option<W>,
    /// The name of the file opened last.
    file_name: String,
    /// Something meaningful was logged: the file survives its close.
    keep_file: bool,
    /// `m_isGuiding`: a calibration or guiding is under way.
    is_guiding: bool,
    summary: Summary,
}

impl<W: Write> Default for GuideLog<W> {
    fn default() -> Self {
        GuideLog {
            enabled: false,
            file: None,
            file_name: String::new(),
            keep_file: false,
            is_guiding: false,
            summary: Summary::default(),
        }
    }
}

impl<W: Write> GuideLog<W> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// One record, flushed. Nothing without a file.
    fn put(&mut self, s: &str) -> bool {
        let Some(f) = &mut self.file else { return false };
        let _ = f.write_all(s.as_bytes());
        let _ = f.flush();
        true
    }

    /// [`put`](Self::put) a line, handed back without its newline.
    fn put_line(&mut self, mut s: String) -> Option<String> {
        if !self.put(&s) {
            return None;
        }
        if s.ends_with('\n') {
            s.pop();
        }
        Some(s)
    }

    /// `EnableLogging` (`guidinglog.cpp:258`). With no file open, opens
    /// `stamp`'s through `open`, which returns the sink and the file's last
    /// [`SUMMARY_TAIL_BYTES`] (nothing for a new file): an existing file is
    /// kept and its summary read back; a new one is deleted at close unless
    /// something meaningful gets logged. Then the `Log enabled at` line,
    /// with the stamp's time rather than now (quirk kept), and the guiding
    /// header when the guider is guiding (`guiding`). The caller saves
    /// `/LoggingMode` = [`is_enabled`](Self::is_enabled). A failed open
    /// leaves logging off; the error comes back.
    pub fn enable(
        &mut self,
        stamp: &LogFileStamp,
        guiding: Option<&GuideLogHeader>,
        open: impl FnOnce(&str) -> io::Result<(W, Vec<u8>)>,
    ) -> io::Result<()> {
        if self.enabled {
            return Ok(());
        }
        if self.file.is_none() {
            self.file_name = stamp.file_name("GuideLog");
            let (file, tail) = open(&self.file_name)?;
            self.file = Some(file);
            if tail.is_empty() {
                // Quirk kept: PHD2 does not clear the counters here, so a new
                // file's summary carries on the previous file's counts.
                self.keep_file = false;
                self.summary.valid = true;
            } else {
                self.keep_file = true;
                self.summary = Summary::load(&tail);
            }
        }
        self.put(&format!(
            "PHD2 version {PHD2_VERSION} [Linux], Log version 2.5. Log enabled at {}\n",
            stamp.time.format(TIME_FORMAT)
        ));
        self.enabled = true;
        if let Some(h) = guiding {
            self.put(&guiding_header(h));
        }
        Ok(())
    }

    /// `DisableLogging`: the `Log disabled at` line. The file stays open, so
    /// [`enable`](Self::enable) continues it and
    /// a forced [`guiding_param`](Self::guiding_param) still writes.
    /// The caller saves `/LoggingMode`.
    pub fn disable(&mut self, now: DateTime<Local>) {
        if !self.enabled {
            return;
        }
        self.put(&format!("\nLog disabled at {}\n", now.format(TIME_FORMAT)));
        self.enabled = false;
    }

    /// `CloseGuideLog`: a kept file gets the summary and the `Log closed
    /// at` line, then the sink is dropped. Returns the file's name when it
    /// must be deleted (nothing meaningful was logged in it).
    pub fn close(&mut self, now: DateTime<Local>) -> Option<String> {
        if self.file.is_some() && self.keep_file {
            let mut s = "\n".to_string();
            let sum = self.summary;
            if sum.valid {
                s += &format!(
                    "Log Summary: calcnt:{} gcnt:{} gdur:{} gacnt:{}\n",
                    sum.cal_cnt,
                    sum.guide_cnt,
                    fmt_f(sum.guide_dur, 0),
                    sum.ga_cnt
                );
            }
            s += &format!("Log closed at {}\n", now.format(TIME_FORMAT));
            self.put(&s);
        }
        self.file = None;
        self.enabled = false;
        (!self.keep_file && !self.file_name.is_empty()).then(|| self.file_name.clone())
    }

    /// `ChangeDirLog` (`guidinglog.cpp:341`): [`close`](Self::close) (the
    /// name that comes back is the old folder's), then, if logging was on,
    /// [`enable`](Self::enable) through `open` in the new folder — or the
    /// old one, when the caller could not make the new one the log folder.
    pub fn change_dir_log(
        &mut self,
        now: DateTime<Local>,
        stamp: &LogFileStamp,
        guiding: Option<&GuideLogHeader>,
        open: impl FnOnce(&str) -> io::Result<(W, Vec<u8>)>,
    ) -> (Option<String>, io::Result<()>) {
        let enabled = self.enabled;
        let removed = self.close(now);
        let res = if enabled { self.enable(stamp, guiding, open) } else { Ok(()) };
        (removed, res)
    }

    /// `StartCalibration` (`guidinglog.cpp:418`): the calibration header,
    /// `mount` being the mount calibrated.
    pub fn start_calibration(&mut self, now: DateTime<Local>, h: &GuideLogHeader, mount: &MountSummary) {
        self.is_guiding = true;
        if !self.enabled {
            return;
        }
        let mut s = format!("\nCalibration Begins at {}\n", now.format(TIME_FORMAT));
        s += &common_header(h);
        s += &format!("Mount = {}, {}\n", mount.name, calibration_settings(mount));
        s += &position_lines(h);
        s += "Direction,Step,dx,dy,x,y,Dist\n";
        self.put(&s);
        self.keep_file = true;
    }

    /// `CalibrationFailed`: the failure message as is.
    pub fn calibration_failed(&mut self, msg: &str) {
        self.is_guiding = false;
        if !self.enabled {
            return;
        }
        self.put(&format!("{msg}\n"));
    }

    /// `CalibrationStep`: a `Direction,Step,dx,dy,x,y,Dist` row
    /// ([`MountEvent::CalibrationRow`](crate::engine::mount::MountEvent::CalibrationRow)).
    pub fn calibration_step(&mut self, info: &CalibrationStepInfo) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.put_line(format!(
            "{},{},{},{},{},{},{}\n",
            info.direction,
            info.step,
            fmt_f(info.dx, 3),
            fmt_f(info.dy, 3),
            fmt_f(info.pos.x, 3),
            fmt_f(info.pos.y, 3),
            fmt_f(info.dist, 3)
        ))
    }

    /// `CalibrationDirectComplete`: an axis is done
    /// ([`MountEvent::CalibrationAxisComplete`](crate::engine::mount::MountEvent::CalibrationAxisComplete)).
    /// Angle in radians, rate in px/ms.
    pub fn calibration_direct_complete(
        &mut self,
        direction: &str,
        angle: f64,
        rate: f64,
        parity: Parity,
    ) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.put_line(format!(
            "{direction} calibration complete. Angle = {} deg, Rate = {} px/sec, Parity = {}\n",
            fmt_f(degrees(angle), 1),
            fmt_f(rate * 1000.0, 3),
            parity.label()
        ))
    }

    /// `CalibrationComplete`: `mount` is the calibrated mount's name.
    pub fn calibration_complete(&mut self, mount: &str) {
        self.is_guiding = false;
        if !self.enabled {
            return;
        }
        self.summary.cal_cnt += 1;
        self.put(&format!("Calibration complete, mount = {mount}.\n"));
    }

    /// `GuidingStarted`: the guiding header, stamped with the time guiding
    /// started.
    pub fn guiding_started(&mut self, started: DateTime<Local>, h: &GuideLogHeader) {
        self.is_guiding = true;
        if !self.enabled {
            return;
        }
        self.put(&format!("\nGuiding Begins at {}\n{}", started.format(TIME_FORMAT), guiding_header(h)));
        self.keep_file = true;
    }

    /// `GuidingStopped`: `guiding_secs` (`TimeSinceGuidingStarted`) goes to
    /// the summary's duration.
    pub fn guiding_stopped(&mut self, now: DateTime<Local>, guiding_secs: f64) {
        self.is_guiding = false;
        if !self.enabled {
            return;
        }
        self.summary.guide_cnt += 1;
        self.summary.guide_dur += guiding_secs;
        self.put(&format!("Guiding Ends at {}\n", now.format(TIME_FORMAT)));
    }

    /// `GuideStep`: a `"Mount"` row (durations, direction letters only for
    /// a non-zero pulse) or an `"AO"` row (signed steps, left and down
    /// negative).
    pub fn guide_step(&mut self, step: &GuideStepInfo) {
        if !self.enabled {
            return;
        }
        let mut s = format!(
            "{},{},\"{}\",{},{},{},{},{},{},",
            step.frame_number,
            fmt_f(step.time, 3),
            if step.is_ao { "AO" } else { "Mount" },
            fmt_f(step.camera_offset.x, 3),
            fmt_f(step.camera_offset.y, 3),
            fmt_f(step.mount_offset.x, 3),
            fmt_f(step.mount_offset.y, 3),
            fmt_f(step.guide_distance_ra, 3),
            fmt_f(step.guide_distance_dec, 3)
        );
        if step.is_ao {
            let x = if step.direction_ra == Dir::LEFT { step.duration_ra.wrapping_neg() } else { step.duration_ra };
            let y = if step.direction_dec == Dir::DOWN { step.duration_dec.wrapping_neg() } else { step.duration_dec };
            s += &format!(",,,,{x},{y},");
        } else {
            let letter = |ms: i32, dir: Dir| if ms > 0 { dir.log_char().to_string() } else { String::new() };
            s += &format!(
                "{},{},{},{},,,",
                step.duration_ra,
                letter(step.duration_ra, step.direction_ra),
                step.duration_dec,
                letter(step.duration_dec, step.direction_dec)
            );
        }
        s += &format!("{},{},{}\n", fmt_f(step.star_mass, 0), fmt_f(step.star_snr, 2), step.star_error);
        self.put(&s);
    }

    /// `FrameDropped`: the `"DROP"` row, its status quoted in a 19th column
    /// ([`GuiderEvent::FrameDropped`](crate::engine::guider::GuiderEvent::FrameDropped)).
    pub fn frame_dropped(&mut self, info: &FrameDroppedInfo) {
        if !self.enabled {
            return;
        }
        let star = &info.star;
        self.put(&format!(
            "{},{},\"DROP\",,,,,,,,,,,,,{},{},{},\"{}\"\n",
            // PHD2's frameNumber is an int.
            info.frame_number as i32,
            fmt_f(info.time, 3),
            fmt_f(star.star_mass, 0),
            fmt_f(star.star_snr, 2),
            star.star_error.error_code(),
            star.status
        ));
    }

    /// `CalibrationFrameDropped`: the star was lost during a calibration
    /// ([`GuiderEvent::CalibrationFrameDropped`](crate::engine::guider::GuiderEvent::CalibrationFrameDropped)).
    pub fn calibration_frame_dropped(&mut self, info: &FrameDroppedInfo) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let star = &info.star;
        self.put_line(format!(
            "INFO: STAR LOST during calibration, Mass= {}, SNR= {}, Error= {}, Status={}\n",
            fmt_f(star.star_mass, 0),
            fmt_f(star.star_snr, 2),
            star.star_error.error_code(),
            star.status
        ))
    }

    /// `NotifyGuidingDithered`: `lock` is the new lock position
    /// ([`ControlEvent::GuidingDithered`](crate::engine::control::ControlEvent::GuidingDithered)).
    pub fn guiding_dithered(&mut self, dx: f64, dy: f64, lock: Point) -> Option<String> {
        if !self.enabled || !self.is_guiding {
            return None;
        }
        self.put_line(format!(
            "INFO: DITHER by {}, {}, new lock pos = {}, {}\n",
            fmt_f(dx, 3),
            fmt_f(dy, 3),
            fmt_f(lock.x, 3),
            fmt_f(lock.y, 3)
        ))
    }

    /// `NotifySettlingStateChange` (`Settling started`, …): written even
    /// when not guiding
    /// ([`ControlEvent::SettlingStateChange`](crate::engine::control::ControlEvent::SettlingStateChange)).
    pub fn settling_state_change(&mut self, msg: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.put_line(format!("INFO: SETTLING STATE CHANGE, {msg}\n"))
    }

    /// `NotifyGACompleted`: counted in the summary, no line
    /// ([`GaEvent::Completed`](crate::engine::assistant::GaEvent::Completed)).
    pub fn ga_completed(&mut self) {
        if self.enabled {
            self.summary.ga_cnt += 1;
        }
    }

    /// `NotifyGAResult`: `msg` brings its own newline
    /// ([`GaEvent::Result`](crate::engine::assistant::GaEvent::Result)).
    pub fn ga_result(&mut self, msg: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.put_line(format!("INFO: GA Result - {msg}"))
    }

    /// The guider's own `INFO:` lines, `line` without the prefix
    /// ([`GuiderEvent::Info`](crate::engine::guider::GuiderEvent::Info)):
    /// `SET LOCK POSITION` (`NotifySetLockPosition`) and `LOCK SHIFT`
    /// (`NotifyLockShiftParams`), which the guider formats. Both are written
    /// only while guiding, and both keep the file.
    pub fn info(&mut self, line: &str) -> Option<String> {
        if !self.enabled || !self.is_guiding {
            return None;
        }
        let line = self.put_line(format!("INFO: {line}\n"));
        self.keep_file = true;
        line
    }

    /// `ServerCommand`: `INFO: Server received <cmd>`
    /// ([`ControlEvent::ServerCommand`](crate::engine::control::ControlEvent::ServerCommand)).
    pub fn server_command(&mut self, cmd: &str) -> Option<String> {
        if !self.enabled || !self.is_guiding {
            return None;
        }
        let line = self.put_line(format!("INFO: Server received {cmd}\n"));
        self.keep_file = true;
        line
    }

    /// `NotifyManualGuide`: ms on a scope, steps on an AO.
    pub fn manual_guide(&mut self, is_ao: bool, dir: Dir, duration: i32) -> Option<String> {
        if !self.enabled || !self.is_guiding {
            return None;
        }
        let (who, label, unit) = if is_ao {
            ("AO", dir.ao_label(), if duration != 1 { "steps" } else { "step" })
        } else {
            ("Mount", dir.mount_label(), "ms")
        };
        let line = self.put_line(format!("INFO: Manual guide ({who}) {label} {duration} {unit}\n"));
        self.keep_file = true;
        line
    }

    /// `SetGuidingParam`: the line of every parameter change — the
    /// guider's, the controller's and the Guiding Assistant's `ParamChange`,
    /// the mounts' `GuideParamChange`. The value's JSON type is PHD2's
    /// overload: a float prints `%.2f`, an integer, a bool or a string as
    /// is. `force` is the guider's `force_log`, PHD2's
    /// `SetGuidingParam(…, AlwaysLog)` (the `MultiStar` change, a string):
    /// written whenever the file is open, guiding or not.
    pub fn guiding_param(&mut self, name: &str, value: &Value, force: bool) -> Option<String> {
        if !force && (!self.enabled || !self.is_guiding) {
            return None;
        }
        let value = match value {
            Value::Number(n) if n.is_f64() => fmt_f(n.as_f64().unwrap_or_default(), 2),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let line = self.put_line(format!("INFO: Guiding parameter change, {name} = {value}\n"));
        // Set even with the file closed, as in PHD2.
        self.keep_file = true;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::calibration::{Calibration, CalibrationIssue};
    use crate::engine::log::debug_log::tests::{Buf, t};
    use crate::engine::multistar::FrameErrorInfo;
    use crate::engine::star::FindResult;
    use crate::engine::traits::MoveOpts;

    // The scenario of target/scratch/harness/guide_log (PHD2's own
    // guidinglog.cpp with the settings summaries copied verbatim), call for
    // call; the goldens are its output.

    fn header() -> GuideLogHeader {
        GuideLogHeader {
            profile: "Simulators".into(),
            dither_ra_only: false,
            dither_scale: 1.0,
            noise_reduction: NoiseReduction::None,
            time_lapse_ms: 0,
            server_enabled: true,
            pixel_scale: 1.5545,
            binning: 1,
            focal_length: 690,
            guider_summary: "Search region = 15 px, Star mass tolerance = 50.0%, Single-star mode\n".into(),
            camera: Some(CameraSummary {
                name: "INDI Camera [Guide Simulator]".into(),
                gain: Some(95),
                read_delay: None,
                port: None,
                frame_width: 1280,
                frame_height: 1024,
                dark_exposure_ms: 0,
                defect_map: false,
                pixel_size: 5.2,
            }),
            exposure: ExposureSummary { duration_ms: 2000, auto: None, var_delay: None },
            mount: Some(scope()),
            secondary_mount: None,
            pointing: PointingSummary {
                coords: Some((5.5875, -5.39, 3.2)),
                pier_side: PierSide::West,
                latitude: Some(48.85),
                rotator: None,
            },
            lock_position: Point::new(640.123, 512.456),
            star_position: Point::new(640.5, 512.25),
            star_hfd: 2.345,
        }
    }

    fn scope() -> MountSummary {
        MountSummary {
            name: "INDI Mount [Telescope Simulator]".into(),
            connected: true,
            guiding_enabled: true,
            aux_mount: None,
            calibrated: false,
            cal: MountCalibration::default(),
            details: CalibrationDetails::default(),
            x_algo: AlgoKind::Hysteresis,
            x_algo_summary: "Hysteresis = 0.120, Aggression = 0.650, Minimum move = 0.150\n".into(),
            y_algo: AlgoKind::ResistSwitch,
            y_algo_summary: "Minimum move = 0.250 Aggression = 90% FastSwitch = disabled\n".into(),
            kind: MountKind::Scope(ScopeSummary {
                backlash_enabled: true,
                backlash_pulse_ms: 450,
                max_ra_duration: 2500,
                max_dec_duration: 2500,
                dec_guide_mode: DecGuideMode::Auto,
                guide_speeds: Some((0.0020833333, 0.0020833333)),
                last_cal_declination: 0.0,
                calibration_step_ms: 750,
                calibration_distance_px: 25,
                assume_orthogonal: false,
            }),
        }
    }

    fn scope_part(m: &mut MountSummary) -> &mut ScopeSummary {
        match &mut m.kind {
            MountKind::Scope(s) => s,
            MountKind::Ao(_) => unreachable!(),
        }
    }

    fn cal_step(direction: &'static str, step: i32, dx: f64, dy: f64, x: f64, y: f64, dist: f64) -> CalibrationStepInfo {
        CalibrationStepInfo { direction, step, dx, dy, pos: Point::new(x, y), dist, msg: String::new() }
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        is_ao: bool,
        frame: i32,
        time: f64,
        cam: (f64, f64),
        mnt: (f64, f64),
        guide: (f64, f64),
        ra: (i32, Dir),
        dec: (i32, Dir),
        mass: f64,
        snr: f64,
        err: i32,
    ) -> GuideStepInfo {
        GuideStepInfo {
            move_options: MoveOpts(13),
            frame_number: frame,
            time,
            is_ao,
            camera_offset: Point::new(cam.0, cam.1),
            mount_offset: Point::new(mnt.0, mnt.1),
            guide_distance_ra: guide.0,
            guide_distance_dec: guide.1,
            duration_ra: ra.0,
            direction_ra: ra.1,
            duration_dec: dec.0,
            direction_dec: dec.1,
            ra_limited: false,
            dec_limited: false,
            ao_pos: (0, 0),
            star_mass: mass,
            star_snr: snr,
            star_hfd: 0.0,
            avg_dist: 0.0,
            star_error: err,
        }
    }

    /// A frame dropped for a low SNR, as the guider reports it.
    fn dropped(frame_number: u32, time: f64, star_mass: f64, star_snr: f64) -> FrameDroppedInfo {
        FrameDroppedInfo {
            frame_number,
            time,
            avg_dist: 0.0,
            star: FrameErrorInfo {
                star_error: FindResult::LowSnr,
                star_mass,
                star_snr,
                star_hfd: 0.0,
                status: "Star lost - low SNR",
            },
        }
    }

    /// A log folder: the files by name.
    #[derive(Default)]
    struct Folder(std::collections::BTreeMap<String, Buf>);

    impl Folder {
        /// `fopen(name, "a+")` and the last [`SUMMARY_TAIL_BYTES`].
        fn open(&mut self, name: &str) -> io::Result<(Buf, Vec<u8>)> {
            let buf = self.0.entry(name.to_string()).or_default().clone();
            let bytes = buf.0.borrow().clone();
            let tail = bytes[bytes.len().saturating_sub(SUMMARY_TAIL_BYTES as usize)..].to_vec();
            Ok((buf, tail))
        }

        fn text(&self, name: &str) -> String {
            self.0[name].text()
        }
    }

    fn golden(name: &str, got: &str, want: &str) {
        if got != want {
            for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
                assert_eq!(g, w, "{name}: line {}", i + 1);
            }
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn calibration_section_matches_phd2() {
        let mut dir = Folder::default();
        let mut log: GuideLog<Buf> = GuideLog::new();
        let h = header();
        let sc = scope();
        let stamp = LogFileStamp { instance: 1, time: t(2026, 11, 11, 21, 58, 3, 0) };
        log.enable(&stamp, None, |n| dir.open(n)).unwrap();
        log.start_calibration(t(2026, 11, 11, 22, 0, 0, 0), &h, &sc);
        // The calibration lines come back for the relay.
        assert_eq!(
            log.calibration_step(&cal_step("West", 0, 0.0, 0.0, 640.5, 512.25, 0.0)).as_deref(),
            Some("West,0,0.000,0.000,640.500,512.250,0.000")
        );
        log.calibration_step(&cal_step("West", 1, -3.9055, 0.2125, 644.4055, 512.0375, 3.911277));
        log.calibration_step(&cal_step("West", 2, -7.8125, 0.4375, 648.3125, 511.8125, 7.8247));
        assert_eq!(
            log.calibration_direct_complete("West", 0.0523, 0.0052083, Parity::Even).as_deref(),
            Some("West calibration complete. Angle = 3.0 deg, Rate = 5.208 px/sec, Parity = Even")
        );
        log.calibration_step(&cal_step("East", 1, 3.9, -0.21, 644.4125, 512.0225, 3.905649));
        log.calibration_step(&cal_step("Backlash", 1, 0.0005, 0.2995, 640.4995, 511.9505, 0.2995));
        log.calibration_step(&cal_step("North", 0, 0.0, 0.0, 640.4995, 511.9505, 0.0));
        log.calibration_step(&cal_step("North", 1, -0.1875, 3.6875, 640.687, 508.263, 3.692264));
        log.calibration_direct_complete("North", -1.5236, 0.0049166, Parity::Odd);
        log.calibration_step(&cal_step("South", 1, 0.125, -3.6, 640.5625, 511.8675, 3.602169));
        log.calibration_frame_dropped(&dropped(31, 0.0, 1234.5, 3.125));
        log.calibration_complete(&sc.name);
        log.start_calibration(t(2026, 11, 11, 22, 10, 0, 0), &h, &sc);
        log.calibration_step(&cal_step("West", 0, 0.0, 0.0, 640.5, 512.25, 0.0));
        log.calibration_failed("Calibration failed: star did not move enough");
        assert_eq!(log.close(t(2026, 11, 11, 22, 30, 0, 0)), None);
        // The same imaging day: the same file, its summary read back.
        log.enable(&stamp, None, |n| dir.open(n)).unwrap();
        log.start_calibration(t(2026, 11, 11, 23, 0, 0, 0), &h, &sc);
        log.calibration_complete(&sc.name);
        assert_eq!(log.close(t(2026, 11, 11, 23, 5, 0, 0)), None);
        golden(
            "calibration",
            &dir.text("PHD2_GuideLog_2026-11-11_215803.txt"),
            include_str!("../testdata/guide_log_calibration.txt"),
        );
    }

    /// The guiding scenario's header: an aux mount, a rotator, auto exposure
    /// and a camera with every option.
    fn guiding_header_values() -> GuideLogHeader {
        let mut h = header();
        h.dither_ra_only = true;
        h.dither_scale = 0.75;
        h.noise_reduction = NoiseReduction::Mean2x2;
        h.time_lapse_ms = 500;
        h.server_enabled = false;
        h.pixel_scale = 1.0;
        h.focal_length = 0;
        h.binning = 2;
        h.exposure = ExposureSummary { duration_ms: 2000, auto: Some((500, 4000, 6.5)), var_delay: Some((1000, 15000)) };
        h.camera = Some(CameraSummary {
            name: "INDI Camera [CCD Simulator]".into(),
            gain: Some(50),
            read_delay: Some(10),
            port: Some(0x378),
            frame_width: 1280,
            frame_height: 1024,
            dark_exposure_ms: 3000,
            defect_map: true,
            pixel_size: 0.0,
        });
        let m = h.mount.as_mut().unwrap();
        m.aux_mount = Some("INDI Mount [EQMod Mount]".into());
        m.calibrated = true;
        m.cal = MountCalibration {
            cal: Calibration {
                x_rate: 0.0052,
                y_rate: 0.0049,
                x_angle: 0.0523,
                y_angle: -1.5236,
                declination: radians(-5.39),
                ra_guide_parity: Parity::Even,
                dec_guide_parity: Parity::Odd,
                ..Calibration::default()
            },
            calibrated: true,
            x_rate: 0.00523,
        };
        m.details = CalibrationDetails {
            image_scale: 1.55,
            ortho_error: 1.2,
            ra_step_count: 12,
            last_issue: CalibrationIssue::None,
            orig_timestamp: "2026-11-11 22:05:01".into(),
            ..CalibrationDetails::default()
        };
        let sp = scope_part(m);
        sp.max_dec_duration = 2000;
        sp.last_cal_declination = radians(-5.39);
        h.pointing = PointingSummary {
            coords: Some((10.0, 48.85, 10.0)),
            pier_side: PierSide::East,
            latitude: Some(48.85),
            rotator: Some(-30.25),
        };
        h.lock_position = Point::new(320.0, 240.0);
        h.star_position = Point::new(320.25, 239.875);
        h.star_hfd = 3.1;
        h
    }

    #[test]
    fn guiding_section_with_rows_and_info_lines_matches_phd2() {
        let mut dir = Folder::default();
        let mut log: GuideLog<Buf> = GuideLog::new();
        // The previous file's counters carry on (PHD2 never clears them).
        log.summary = Summary { valid: true, cal_cnt: 2, ..Summary::default() };
        let h = guiding_header_values();
        let stamp = LogFileStamp { instance: 1, time: t(2026, 11, 12, 8, 15, 42, 0) };
        // Logging enabled while guiding: the header comes at once.
        log.enable(&stamp, Some(&h), |n| dir.open(n)).unwrap();
        log.guiding_started(t(2026, 11, 12, 8, 16, 0, 0), &h);
        let (w, e, n, s) = (Dir::West, Dir::East, Dir::North, Dir::South);
        let (mass, snr) = (12345.6, 25.4567);
        log.guide_step(&row(false, 1, 2.512, (0.25, -0.125), (0.2366, -0.1412), (0.154, -0.0), (30, w), (0, s), mass, snr, 0));
        let (mass, snr) = (12001.5, 24.005);
        log.guide_step(&row(false, 2, 5.0249999, (-0.05, 0.6), (-0.0712, 0.5958), (0.0, 0.4171), (0, e), (120, n), mass, snr, 0));
        let (mass, snr) = (65535.4999, 112.0);
        log.guide_step(&row(false, 3, 7.5, (-0.3, -1.1), (-0.3567, -1.0824), (-0.2318, -0.9742), (45, e), (200, s), mass, snr, 1));
        log.frame_dropped(&FrameDroppedInfo { avg_dist: 0.5, ..dropped(4, 10.0005, 0.0, 0.0) });
        log.guiding_dithered(1.234, -2.345, Point::new(321.234, 237.655));
        log.settling_state_change("Settling started");
        log.settling_state_change("Settling complete");
        // The guider's lines, as guider.rs formats them (its tests check
        // the formats against PHD2's).
        assert_eq!(
            log.info("SET LOCK POSITION, new lock pos = 300.500, 200.250").as_deref(),
            Some("INFO: SET LOCK POSITION, new lock pos = 300.500, 200.250")
        );
        log.info("LOCK SHIFT, enabled = 1 RA,Dec rate (1.50,-2.25) arc-sec/hr (0.36,-0.72) px/hr");
        log.info("LOCK SHIFT, enabled = 1 X,Y rate (0.00,0.00) pixels/hr (0.00,0.00) px/hr");
        log.info("LOCK SHIFT, enabled = 0 ");
        log.server_command("PAUSE");
        log.server_command("RESUME");
        log.manual_guide(false, Dir::North, 500);
        assert_eq!(
            log.guiding_param("Dither scale", &Value::from(0.755), false).as_deref(),
            Some("INFO: Guiding parameter change, Dither scale = 0.76")
        );
        log.guiding_param("Dec Max Duration", &Value::from(2000), false);
        log.guiding_param("MountGuidingEnabled", &Value::from(false), false);
        log.guiding_param("Dec Guide Mode", &Value::from("Auto"), false);
        log.ga_result("Polar alignment error = 3.2 arc-min\n");
        log.ga_completed();
        log.guiding_stopped(t(2026, 11, 12, 8, 46, 0, 0), 1800.4);
        // Not guiding any more: these are dropped…
        assert_eq!(log.guiding_dithered(1.0, 1.0, Point::new(300.5, 200.25)), None);
        assert_eq!(log.info("SET LOCK POSITION, new lock pos = 300.500, 200.250"), None);
        assert_eq!(log.server_command("PAUSE"), None);
        assert_eq!(log.guiding_param("Exposure", &Value::from("2000 ms"), false), None);
        // … but not these two.
        log.settling_state_change("Settling failed");
        assert_eq!(log.ga_result("Backlash=120 ms\n").as_deref(), Some("INFO: GA Result - Backlash=120 ms"));
        assert_eq!(log.close(t(2026, 11, 12, 9, 0, 0, 0)), None);
        golden(
            "guiding",
            &dir.text("PHD2_GuideLog_2026-11-12_081542.txt"),
            include_str!("../testdata/guide_log_guiding.txt"),
        );
    }

    #[test]
    fn ao_and_predictive_pec_section_matches_phd2() {
        let mut dir = Folder::default();
        let mut log: GuideLog<Buf> = GuideLog::new();
        log.summary = Summary { valid: true, cal_cnt: 2, guide_cnt: 1, guide_dur: 1800.4, ga_cnt: 1 };
        let mut h = guiding_header_values();
        let mut sc = h.mount.take().unwrap();
        sc.aux_mount = None;
        sc.guiding_enabled = false;
        sc.cal.cal.declination = UNKNOWN_DECLINATION;
        sc.details = CalibrationDetails { last_issue: CalibrationIssue::Angle, ..CalibrationDetails::default() };
        sc.x_algo = AlgoKind::GaussianProcess;
        sc.x_algo_summary = "Control gain = 0.700\nPrediction gain = 0.500\nMinimum move = 0.200\nHyperparameters\n\
                             \tLength scale long range SE kernel = 700.000\n\tSignal variance long range SE kernel = 20.000\n\
                             \tLength scale periodic kernel = 10.000\n\tSignal variance periodic kernel = 20.000\n\
                             \tLength scale short range SE kernel = 25.000\n\tSignal variance short range SE kernel = 10.000\n\
                             \tPeriod length periodic kernel = 483.300\n\tFFT called after = 2.000 worm cycles\n\
                             \tAuto-adjust period length = On\n"
            .into();
        sc.y_algo = AlgoKind::None;
        sc.y_algo_summary = "\n".into();
        let sp = scope_part(&mut sc);
        sp.backlash_enabled = false;
        sp.backlash_pulse_ms = 0;
        sp.dec_guide_mode = DecGuideMode::North;
        sp.guide_speeds = None;
        sp.last_cal_declination = UNKNOWN_DECLINATION;
        let hyst = "Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.200\n".to_string();
        let ao = MountSummary {
            name: "INDI AO [SX AO]".into(),
            connected: true,
            guiding_enabled: true,
            aux_mount: None,
            calibrated: true,
            cal: MountCalibration {
                cal: Calibration {
                    x_rate: 0.312,
                    y_rate: 0.298,
                    x_angle: 1.2,
                    y_angle: -0.37,
                    declination: 0.2,
                    ..Calibration::default()
                },
                calibrated: true,
                x_rate: 0.312,
            },
            details: CalibrationDetails {
                image_scale: 1.2,
                ortho_error: 2.5,
                ra_step_count: 6,
                dec_step_count: 6,
                orig_timestamp: "2026-11-12 19:40:00".into(),
                ..CalibrationDetails::default()
            },
            x_algo: AlgoKind::Hysteresis,
            x_algo_summary: hyst.clone(),
            y_algo: AlgoKind::Hysteresis,
            y_algo_summary: hyst,
            kind: MountKind::Ao(AoSummary {
                bump_percentage: 75,
                bump_step: 1.5,
                calibration_steps: 3,
                samples_to_average: 3,
            }),
        };
        h.mount = Some(ao.clone());
        h.secondary_mount = Some(sc.clone());
        h.dither_ra_only = false;
        h.dither_scale = 1.0;
        h.noise_reduction = NoiseReduction::Median3;
        h.time_lapse_ms = 0;
        h.server_enabled = true;
        h.pixel_scale = 1.2;
        h.focal_length = 650;
        h.binning = 1;
        h.exposure = ExposureSummary { duration_ms: 1500, auto: None, var_delay: None };
        // The multi-star summary really ends in a newline and a space.
        h.guider_summary = "Search region = 20 px, Star mass tolerance disabled, Multi-star mode, list size = 7\n ".into();
        h.camera = Some(CameraSummary {
            name: "INDI Camera [ZWO ASI120MM Mini]".into(),
            gain: None,
            read_delay: None,
            port: None,
            frame_width: 3096,
            frame_height: 2080,
            dark_exposure_ms: 0,
            defect_map: false,
            pixel_size: 3.8,
        });
        h.pointing = PointingSummary::default();
        h.lock_position = Point::new(1500.0, 1000.0);
        h.star_position = Point::new(1500.125, 999.5);
        h.star_hfd = 2.2;

        let stamp = LogFileStamp { instance: 1, time: t(2026, 11, 12, 20, 0, 0, 0) };
        log.enable(&stamp, None, |n| dir.open(n)).unwrap();
        log.start_calibration(t(2026, 11, 12, 20, 1, 0, 0), &h, &ao);
        log.calibration_step(&cal_step("Left", 0, 0.0, 0.0, 1500.125, 999.5, 0.0));
        log.calibration_step(&cal_step("Left", 1, -1.875, 0.25, 1502.0, 999.25, 1.891594));
        log.calibration_direct_complete("Left", 1.2, 0.000312, Parity::Unknown);
        log.calibration_step(&cal_step("Up", 1, 0.5, 1.75, 1499.625, 997.75, 1.820027));
        log.calibration_direct_complete("Up", -0.37, 0.000298, Parity::Unknown);
        log.calibration_complete(&ao.name);
        log.guiding_started(t(2026, 11, 12, 20, 5, 0, 0), &h);
        let (l, r, u, d) = (Dir::LEFT, Dir::RIGHT, Dir::UP, Dir::DOWN);
        let (mass, snr) = (5000.5, 15.0);
        log.guide_step(&row(true, 1, 1.5, (-0.5, 0.25), (-0.4632, 0.3051), (-0.3242, 0.2136), (3, l), (2, d), mass, snr, 0));
        let (mass, snr) = (4999.5, 14.995);
        log.guide_step(&row(true, 2, 3.0, (0.75, -0.25), (0.7193, -0.3388), (0.5035, -0.2372), (4, r), (1, u), mass, snr, 0));
        let (west, south) = ((403, Dir::West), (183, Dir::South));
        log.guide_step(&row(false, 2, 3.0, (0.0, 0.0), (2.1, -0.9), (2.1, -0.9), west, south, mass, snr, 0));
        log.manual_guide(true, Dir::UP, 1);
        log.manual_guide(true, Dir::LEFT, 3);
        log.guiding_stopped(t(2026, 11, 12, 21, 5, 0, 0), 3600.5);
        log.disable(t(2026, 11, 12, 21, 10, 0, 0));
        log.guiding_param("Dec guide mode", &Value::from("North"), true); // the forced form ignores the switch
        log.guiding_param("Exposure", &Value::from("1500 ms"), false); // dropped
        log.enable(&stamp, None, |_| unreachable!("the file is still open")).unwrap();
        assert_eq!(log.close(t(2026, 11, 12, 21, 30, 0, 0)), None);
        golden("ppec", &dir.text("PHD2_GuideLog_2026-11-12_200000.txt"), include_str!("../testdata/guide_log_ppec.txt"));
    }

    #[test]
    fn an_empty_new_log_is_deleted_at_close() {
        let mut dir = Folder::default();
        let mut log: GuideLog<Buf> = GuideLog::new();
        let stamp = LogFileStamp { instance: 2, time: t(2026, 11, 13, 10, 0, 0, 0) };
        log.enable(&stamp, None, |n| dir.open(n)).unwrap();
        // Settling and GA lines are not "meaningful".
        log.settling_state_change("Settling started");
        assert_eq!(log.close(t(2026, 11, 13, 11, 0, 0, 0)).as_deref(), Some("PHD2_GuideLog_2_2026-11-13_100000.txt"));
        assert_eq!(
            dir.text("PHD2_GuideLog_2_2026-11-13_100000.txt"),
            "PHD2 version 2.6.14 [Linux], Log version 2.5. Log enabled at 2026-11-13 10:00:00\n\
             INFO: SETTLING STATE CHANGE, Settling started\n"
        );
        // A lock move while guiding keeps it.
        let stamp = LogFileStamp { instance: 1, time: t(2026, 11, 14, 10, 0, 0, 0) };
        log.enable(&stamp, None, |n| dir.open(n)).unwrap();
        log.guiding_started(t(2026, 11, 14, 10, 0, 0, 0), &header());
        assert_eq!(log.close(t(2026, 11, 14, 11, 0, 0, 0)), None);
    }

    #[test]
    fn summary_is_read_back_from_the_tail() {
        let tail = b"ing Ends at 2026-11-12 08:46:00\n\nLog Summary: calcnt:12 gcnt:3 gdur:5401 gacnt:1\nLog closed at 2026-11-12 09:00:00\n";
        assert_eq!(
            Summary::load(tail),
            Summary { valid: true, cal_cnt: 12, guide_cnt: 3, guide_dur: 5401.0, ga_cnt: 1 }
        );
        // A tail without the line: no summary at close.
        assert_eq!(Summary::load(b"mmary: calcnt:1\r\nLog closed at\r\n"), Summary::default());
        let mut dir = Folder::default();
        dir.0.insert("PHD2_GuideLog_2026-11-13_100000.txt".into(), Buf::default());
        dir.0["PHD2_GuideLog_2026-11-13_100000.txt"].0.borrow_mut().extend_from_slice(b"x\n");
        let mut log: GuideLog<Buf> = GuideLog::new();
        let stamp = LogFileStamp { instance: 1, time: t(2026, 11, 13, 10, 0, 0, 0) };
        log.enable(&stamp, None, |n| dir.open(n)).unwrap();
        assert_eq!(log.close(t(2026, 11, 13, 10, 0, 1, 0)), None);
        assert!(dir.text("PHD2_GuideLog_2026-11-13_100000.txt").ends_with("2026-11-13 10:00:00\n\nLog closed at 2026-11-13 10:00:01\n"));
    }

    #[test]
    fn exposure_and_pointing_text() {
        let auto = ExposureSummary { duration_ms: 1000, auto: Some((500, 4000, 6.5)), var_delay: None };
        assert_eq!(auto.text(), "Auto (min = 500 ms, max = 4000 ms, SNR = 6.50)");
        // Rotator position unknown; azimuth through atan2.
        let p = PointingSummary {
            coords: Some((5.5875, -5.39, 3.2)),
            pier_side: PierSide::West,
            latitude: Some(48.85),
            rotator: Some(ROTATOR_POSITION_UNKNOWN),
        };
        assert_eq!(
            pointing_info(&p),
            "RA = 5.59 hr, Dec = -5.4 deg, Hour angle = -2.39 hr, Pier side = West, Rotator pos = Unknown, Alt = 27.4 deg, Az = 139.0 deg"
        );
    }
}
