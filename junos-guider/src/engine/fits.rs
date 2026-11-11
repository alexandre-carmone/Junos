//! FITS in and out of a [`UsImage`], without cfitsio: the camera blob reader
//! (`CameraINDI::ReadFITS`), `usImage::Load`, the dark library reader, the
//! writer behind `usImage::Save` with PHD2's header cards, and the multi-HDU
//! form the dark library is stored in.
//!
//! Only what PHD2 reads and writes is implemented: 2-D images in the primary
//! HDU or `IMAGE` extensions, BITPIX 8 / 16 / 32 / 64 / −32 / −64 scaled by
//! `BSCALE` / `BZERO` and converted to `u16` the way `fits_read_pix(TUSHORT)`
//! does it — truncated, and a value outside −0.49..=65535.49 fails the read
//! (cfitsio's NUM_OVERFLOW, which every PHD2 caller treats as a read error);
//! on the way out `BITPIX 16` with `BZERO 32768`, the cfitsio `USHORT_IMG`
//! layout, card by card as cfitsio formats them, the header blank-filled.
//! Errors are the alert texts PHD2 raises at each call site.
//!
//! Ported from PHD2 src/usImage.cpp, src/cam_indi.cpp, src/myframe.cpp, src/fitsiowrap.h — BSD-3-Clause, see THIRD_PARTY.md

use chrono::{DateTime, Utc};

use super::image::UsImage;
use super::log::cfmt::fmt_gp;
use super::traits::{PierSide, Point, Rect};

const BLOCK: usize = 2880;
const CARD: usize = 80;

// ---------------------------------------------------------------- reader

/// The cards of one header, keyword → value text (what sits between `= `
/// and the `/` comment, trimmed; strings keep their quotes).
struct Header {
    cards: Vec<(String, String)>,
}

impl Header {
    fn raw(&self, key: &str) -> Option<&str> {
        self.cards.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// `fits_read_key(TFLOAT / TDOUBLE)`: Fortran `D` exponents accepted.
    fn float(&self, key: &str) -> Option<f64> {
        let v = self.raw(key)?;
        v.replace(['D', 'd'], "E").parse().ok()
    }

    /// An integer card, or a real one truncated.
    fn int(&self, key: &str) -> Option<i64> {
        let v = self.raw(key)?;
        v.parse::<i64>().ok().or_else(|| self.float(key).map(|f| f as i64))
    }

    /// A string card, quotes stripped, `''` unescaped, trailing blanks trimmed.
    fn string(&self, key: &str) -> Option<String> {
        let v = self.raw(key)?;
        let inner = v.strip_prefix('\'')?;
        let end = inner.rfind('\'')?;
        Some(inner[..end].replace("''", "'").trim_end().to_string())
    }
}

/// Parse the header starting at `start`; returns it with the offset of the
/// data that follows (the next 2880-byte boundary after the END card), or
/// `None` when the file ends before the END card.
fn parse_header(bytes: &[u8], start: usize) -> Option<(Header, usize)> {
    let mut cards = Vec::new();
    let mut off = start;
    loop {
        let card = bytes.get(off..off + CARD)?;
        off += CARD;
        let key = String::from_utf8_lossy(&card[..8]);
        let key = key.trim_end();
        if key == "END" {
            break;
        }
        if &card[8..10] != b"= " {
            continue; // COMMENT, HISTORY, blank: nothing to read.
        }
        // The value runs to the first `/` outside quotes.
        let mut text = String::new();
        let mut quoted = false;
        for &b in &card[10..] {
            if b == b'\'' {
                quoted = !quoted;
            } else if b == b'/' && !quoted {
                break;
            }
            text.push(b as char);
        }
        cards.push((key.to_string(), text.trim().to_string()));
    }
    Some((Header { cards }, off.div_ceil(BLOCK) * BLOCK))
}

/// One HDU located in the file, as cfitsio sees it when it moves there.
struct Hdu {
    header: Header,
    /// `fits_get_hdu_type == IMAGE_HDU`: the primary HDU or an `IMAGE` extension.
    image: bool,
    bitpix: i64,
    /// `NAXIS1..NAXISn`; `NAXIS` is their count.
    axes: Vec<i64>,
    data_start: usize,
    /// Bytes of data before the block padding; `usize::MAX` when the
    /// header's sizes overflow (the data can then never be read).
    data_len: usize,
}

impl Hdu {
    /// The offset of the next HDU.
    fn end(&self) -> usize {
        self.data_start.saturating_add(self.data_len).div_ceil(BLOCK).saturating_mul(BLOCK)
    }
}

/// The HDU at `start`, or `None` where cfitsio fails to open or move to it:
/// the first card must be `SIMPLE` (primary) or `XTENSION`, `BITPIX` one of
/// the six FITS values, `NAXIS` and every `NAXISn` present and not negative.
/// Any extension type is accepted. The data is not checked here: a short file
/// opens and only fails at the pixel read (END_OF_FILE).
fn read_hdu(bytes: &[u8], start: usize) -> Option<Hdu> {
    let first: &[u8] = if start == 0 { b"SIMPLE  " } else { b"XTENSION" };
    if !bytes.get(start..)?.starts_with(first) {
        return None;
    }
    let (header, data_start) = parse_header(bytes, start)?;
    let image = start == 0 || header.string("XTENSION").as_deref() == Some("IMAGE");
    let bitpix = header.int("BITPIX").filter(|b| [8, 16, 32, 64, -32, -64].contains(b))?;
    let naxis = header.int("NAXIS").filter(|n| (0..=999).contains(n))?;
    let axes =
        (1..=naxis).map(|i| header.int(&format!("NAXIS{i}")).filter(|&n| n >= 0)).collect::<Option<Vec<_>>>()?;
    let data_len = if naxis == 0 {
        Some(0)
    } else {
        let pcount = header.int("PCOUNT").unwrap_or(0).max(0) as usize;
        let gcount = header.int("GCOUNT").unwrap_or(1).max(0) as usize;
        axes.iter()
            .try_fold(1usize, |n, &a| n.checked_mul(a as usize))
            .and_then(|n| n.checked_add(pcount))
            .and_then(|n| n.checked_mul(gcount))
            .and_then(|n| n.checked_mul(bitpix.unsigned_abs() as usize / 8))
    };
    Some(Hdu { header, image, bitpix, axes, data_start, data_len: data_len.unwrap_or(usize::MAX) })
}

/// The HDUs `fits_get_num_hdus` counts, in order: cfitsio moves on while the
/// next header reads, whatever the extension type, so a binary table counts;
/// bytes after the last HDU are ignored.
fn hdus(bytes: &[u8]) -> Vec<Hdu> {
    let mut out = Vec::new();
    let mut off = 0;
    while let Some(hdu) = read_hdu(bytes, off) {
        off = hdu.end();
        out.push(hdu);
    }
    out
}

/// `fits_read_pix(TUSHORT)` of a 2-D HDU: `raw * BSCALE + BZERO`, truncated
/// to `u16`. `None` is a failed read: data past the end of the file
/// (END_OF_FILE), or any value below −0.49 or above 65535.49 (NUM_OVERFLOW:
/// cfitsio clamps it but returns the error, and PHD2 drops the frame). A NaN
/// passes both tests and becomes 0, as cfitsio's `(unsigned short)` cast
/// gives on x86.
fn read_pixels(bytes: &[u8], hdu: &Hdu) -> Option<Vec<u16>> {
    let bscale = hdu.header.float("BSCALE").unwrap_or(1.0);
    let bzero = hdu.header.float("BZERO").unwrap_or(0.0);
    let size = hdu.bitpix.unsigned_abs() as usize / 8;
    let n = (hdu.axes[0] as usize).checked_mul(hdu.axes[1] as usize)?;
    let raw = bytes.get(hdu.data_start..)?.get(..n.checked_mul(size)?)?;
    let mut overflow = false;
    let px = raw
        .chunks_exact(size)
        .map(|c| {
            let v = match hdu.bitpix {
                8 => c[0] as f64,
                16 => i16::from_be_bytes([c[0], c[1]]) as f64,
                32 => i32::from_be_bytes([c[0], c[1], c[2], c[3]]) as f64,
                64 => i64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) as f64,
                -32 => f32::from_be_bytes([c[0], c[1], c[2], c[3]]) as f64,
                _ => f64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]),
            };
            let d = v * bscale + bzero;
            // DUSHRT_MIN / DUSHRT_MAX of cfitsio's fff*u2 converters.
            if d < -0.49 || d > 65535.49 {
                overflow = true;
            }
            d as u16
        })
        .collect();
    (!overflow).then_some(px)
}

/// A fresh frame (`new usImage` then `Init`) holding `data`.
fn frame(w: i32, h: i32, data: Vec<u16>) -> UsImage {
    UsImage { w, h, data, ..UsImage::default() }
}

/// `CameraINDI::ReadFITS` (cam_indi.cpp:838): the camera's FITS blob as a
/// frame — pixels only, no header card and no statistics (the capture
/// pipeline runs the dark subtraction, noise reduction, then `calc_stats`).
/// One HDU with `NAXIS == 2`; `Err` is the alert PHD2 raises otherwise.
///
/// `sub` is `takeSubframe`: the camera's full frame size and the subframe
/// the blob holds. The frame is then that size, blank, with the blob's
/// pixels copied row after row (`sub.w` at a time) to the subframe's place,
/// and `subframe` set (cam_indi.cpp:885). Without it the frame is the blob's
/// size, which the camera records as its `FrameSize`. PHD2 copies the rows
/// unchecked; here a copy that would run past the blob or the frame is
/// "Error reading data".
pub fn read_fits_blob(bytes: &[u8], sub: Option<((i32, i32), Rect)>) -> Result<UsImage, &'static str> {
    let hdus = hdus(bytes);
    // The primary HDU is always an image: "FITS file is not of an image" cannot happen here.
    let hdu = hdus.first().ok_or("Unsupported type or read error loading FITS file")?;
    if hdu.axes.len() == 3 {
        return Err("RGB images are not supported, please switch the INDI driver to Mono");
    }
    if hdus.len() != 1 || hdu.axes.len() != 2 {
        return Err("Unsupported FITS file");
    }
    let px = read_pixels(bytes, hdu).ok_or("Error reading data")?;
    let Some(((fw, fh), sf)) = sub else {
        return Ok(frame(hdu.axes[0] as i32, hdu.axes[1] as i32, px));
    };
    let mut img = UsImage::new(fw, fh);
    img.subframe = Some(sf);
    for y in 0..sf.h.max(0) {
        let src = px.get((y * sf.w) as usize..((y + 1) * sf.w) as usize);
        let at = ((y + sf.y) * fw + sf.x) as usize;
        match (src, img.data.get_mut(at..at.wrapping_add(sf.w as usize))) {
            (Some(s), Some(d)) => d.copy_from_slice(s),
            _ => return Err("Error reading data"),
        }
    }
    Ok(img)
}

/// `usImage::Load` (usImage.cpp:434): a single 2-D FITS image (one PHD2
/// saved, typically), PHD2's cards read back — `EXPOSURE` (truncated to ms),
/// `STACKCNT`, `PEDESTAL`, `SATURATE` (bit depth 16 when > 255, else 8) and
/// the `PHDSUBFX/Y/W/H` subframe — then `calc_stats`. `Err` is PHD2's alert,
/// ending with `name`, the file name. A NAXIS 3 file is plainly unsupported
/// here (only the camera reader has the RGB message).
pub fn load_fits(bytes: &[u8], name: &str) -> Result<UsImage, String> {
    let hdus = hdus(bytes);
    let hdu = hdus.first().ok_or_else(|| format!("Error opening FITS file {name}"))?;
    if hdus.len() != 1 || hdu.axes.len() != 2 {
        return Err(format!("Unsupported type or read error loading FITS file {name}"));
    }
    let px = read_pixels(bytes, hdu).ok_or_else(|| format!("Error reading data from FITS file {name}"))?;
    let mut img = frame(hdu.axes[0] as i32, hdu.axes[1] as i32, px);
    let h = &hdu.header;
    // fhdr_int is fits_read_key(TINT): a value outside int's range is
    // NUM_OVERFLOW and the card is skipped.
    let int = |key: &str| h.int(key).and_then(|v| i32::try_from(v).ok());
    if let Some(exposure) = h.float("EXPOSURE") {
        // usImage.cpp:486: a float header value times 1000.0, truncated.
        img.img_exp_dur = (exposure as f32 as f64 * 1000.0) as i32;
    }
    if let Some(v) = int("STACKCNT") {
        img.img_stack_cnt = v;
    }
    if let Some(v) = int("PEDESTAL") {
        img.pedestal = v as u16;
    }
    if let Some(v) = int("SATURATE") {
        img.bits_per_pixel = if v > 255 { 16 } else { 8 };
    }
    if let (Some(x), Some(y), Some(w), Some(hh)) = (int("PHDSUBFX"), int("PHDSUBFY"), int("PHDSUBFW"), int("PHDSUBFH")) {
        img.subframe = Some(Rect::new(x, y, w, hh));
    }
    img.calc_stats();
    Ok(img)
}

/// `ROUNDF(exposure * 1000.0)` (myframe.cpp:2478): the float exposure times
/// a double, rounded through `floorf`.
fn roundf_ms(exposure: f32) -> i32 {
    ((exposure as f64 * 1000.0 + 0.5) as f32).floor() as i32
}

/// What `load_multi_darks` (myframe.cpp:2404) gets out of the dark library file.
#[derive(Debug, Default)]
pub struct DarkFile {
    /// The frames read, in file order, statistics computed. The flag is set
    /// when the frame had no `EXPOSURE` card and took the fallback; PHD2 then
    /// debug-logs `missing EXPOSURE value, assume %.3f` (the fallback in s).
    pub frames: Vec<(UsImage, bool)>,
    /// The alert that stopped the read, ending with the file name except for
    /// the size mismatch. The frames before it stay in `frames`: PHD2 hands
    /// each one to the camera (`AddDark`) as it reads it and keeps them when
    /// a later HDU fails, while it reports the load as failed.
    pub error: Option<String>,
}

/// Read the dark library file as `load_multi_darks` does: every HDU must be a
/// 2-D image of the first one's size; each frame gets its `EXPOSURE`
/// (rounded to ms) — or `fallback_exp_ms`, PHD2's requested exposure, when
/// the card is missing — and its statistics. `name` is the file name the
/// alerts end with.
pub fn read_multi(bytes: &[u8], name: &str, fallback_exp_ms: i32) -> DarkFile {
    let mut frames = Vec::new();
    let error = read_darks(bytes, name, fallback_exp_ms, &mut frames).err();
    DarkFile { frames, error }
}

fn read_darks(bytes: &[u8], name: &str, fallback_exp_ms: i32, frames: &mut Vec<(UsImage, bool)>) -> Result<(), String> {
    let hdus = hdus(bytes);
    if hdus.is_empty() {
        return Err(format!("Error opening FITS file {name}"));
    }
    let mut size: Option<(i64, i64)> = None;
    for hdu in &hdus {
        if !hdu.image {
            return Err(format!("FITS file is not of an image: {name}"));
        }
        if hdu.axes.len() != 2 {
            return Err(format!("Unsupported type or read error loading FITS file {name}"));
        }
        let this = (hdu.axes[0], hdu.axes[1]);
        if size.is_some_and(|s| s != this) {
            return Err("Existing dark library has frames with incompatible formats - please rebuild the dark library from scratch.".into());
        }
        size = Some(this);
        let px = read_pixels(bytes, hdu).ok_or_else(|| format!("Error reading data from {name}"))?;
        let mut img = frame(this.0 as i32, this.1 as i32, px);
        let card = hdu.header.float("EXPOSURE");
        // `(float) RequestedExposureDuration() / 1000.0`: the division in double.
        let exposure = card.map_or((fallback_exp_ms as f32 as f64 / 1000.0) as f32, |e| e as f32);
        img.img_exp_dur = roundf_ms(exposure);
        img.calc_stats();
        frames.push((img, card.is_none()));
    }
    Ok(())
}

// ---------------------------------------------------------------- writer

/// The camera cards of `usImage::Save` (usImage.cpp:326), present when a
/// camera is connected.
#[derive(Clone, Debug, Default)]
pub struct CameraCards {
    /// `INSTRUME`.
    pub name: String,
    /// `XBINNING` / `YBINNING` / `CCDXBIN` / `CCDYBIN`.
    pub binning: u32,
    /// Unbinned pixel size in microns; `XPIXSZ` / `YPIXSZ` carry it times the binning.
    pub pixel_size: f64,
    /// `GAIN` (PHD's 0–100 value).
    pub gain: u32,
    /// `CAMBPP`.
    pub bits_per_pixel: u32,
}

/// Everything `usImage::Save` takes from the running application; the
/// caller fills it from the engine's state.
#[derive(Clone, Debug, Default)]
pub struct SaveHeader {
    /// `USERNOTE`, written when not empty.
    pub note: String,
    /// `DATE`: file creation time, UTC. The caller reads the clock.
    pub date: DateTime<Utc>,
    /// `CREATOR` (PHD2 writes `PHD2 <version>`).
    pub creator: String,
    /// `PHDPROFI`: the equipment profile name.
    pub profile: String,
    pub camera: Option<CameraCards>,
    /// Scope pointing as (RA hours, Dec degrees) → `RA`, `DEC`, `OBJCTRA`, `OBJCTDEC`.
    pub coords: Option<(f64, f64)>,
    /// `PIERSIDE`, written unless unknown.
    pub pier_side: PierSide,
    /// `SCALE` / `PIXSCALE`, arcsec per pixel.
    pub pixel_scale: f64,
    /// `PHDLOCKX` / `PHDLOCKY` when a lock position is set.
    pub lock_pos: Option<Point>,
}

/// cfitsio's text for a `TFLOAT` card (`ffr2e`): `%.7G`, then a decimal
/// point is forced — `2` becomes `2.`, `1E+07` is rewritten as `1.0E+07`.
fn float_text(v: f32) -> String {
    // glibc's `%G` is its `%g` with `E`, `INF` and `NAN` in capitals.
    let s = fmt_gp(v as f64, 7).to_uppercase();
    if s.contains('.') {
        s
    } else if s.contains('E') {
        let sci = format!("{:.1e}", v as f64);
        let (mant, exp) = sci.split_once('e').unwrap();
        let exp: i32 = exp.parse().unwrap();
        format!("{mant}E{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        s + "."
    }
}

/// A header under construction, 80-byte cards laid out as cfitsio's
/// `ffmkky` does: `KEYWORD = ` then the value right-justified to column 30
/// (a string left-justified and padded to column 30), then ` / comment`.
struct Cards {
    bytes: Vec<u8>,
    /// A float card was NaN or infinite: cfitsio's `ffr2e` refuses it
    /// (BAD_F2C), so PHD2's save fails.
    bad_float: bool,
}

impl Cards {
    fn new() -> Self {
        Cards { bytes: Vec::new(), bad_float: false }
    }

    /// One card as `ffprec` stores it: cut at 80 bytes, blank-padded, and
    /// every byte outside printable ASCII replaced by a blank (a UTF-8 `é`
    /// becomes two blanks).
    fn raw(&mut self, text: &[u8]) {
        let mut card: Vec<u8> = text.iter().take(CARD).map(|&b| if (0x20..=0x7e).contains(&b) { b } else { b' ' }).collect();
        card.resize(CARD, b' ');
        self.bytes.extend_from_slice(&card);
    }

    /// `ffmkky`'s comment rule: ` / comment` only while the card so far is
    /// shorter than 77 bytes, then whatever fits in 80.
    fn card(&mut self, mut card: Vec<u8>, comment: &str) {
        if !comment.is_empty() && card.len() < 77 {
            card.extend_from_slice(b" / ");
            card.extend_from_slice(comment.as_bytes());
        }
        self.raw(&card);
    }

    fn value(&mut self, key: &str, value: &str, comment: &str) {
        self.card(format!("{key:<8}= {value:>20}").into_bytes(), comment);
    }

    fn logical(&mut self, key: &str, v: bool, comment: &str) {
        self.value(key, if v { "T" } else { "F" }, comment);
    }

    fn int(&mut self, key: &str, v: i64, comment: &str) {
        self.value(key, &v.to_string(), comment);
    }

    fn float(&mut self, key: &str, v: f32, comment: &str) {
        if !v.is_finite() {
            self.bad_float = true;
            return;
        }
        self.value(key, &float_text(v), comment);
    }

    /// `ffs2c` + `ffmkky`, byte for byte: at most 68 bytes of `v`, each quote
    /// doubled while there is room inside the 68, padded to 8 inside the
    /// quotes, then the card padded to column 30. ffs2c's quirk is kept: a
    /// quote whose double would not fit is dropped with it, and the first one
    /// closes the string (67 × `a` + `'b` is written as 67 × `a`).
    fn string(&mut self, key: &str, v: &str, comment: &str) {
        let mut quoted = vec![b'\''];
        for &b in v.as_bytes().iter().take(68) {
            if quoted.len() >= 69 {
                break;
            }
            quoted.push(b);
            if b == b'\'' {
                quoted.push(b'\'');
            }
        }
        quoted.resize(quoted.len().max(9), b' ');
        if quoted.len() == 70 {
            quoted.truncate(69);
        } else {
            quoted.push(b'\'');
        }
        let mut card = format!("{key:<8}= ").into_bytes();
        card.extend_from_slice(&quoted);
        card.resize(card.len().max(30), b' ');
        self.card(card, comment);
    }

    /// The END card and the block padding: blanks, as the FITS standard and
    /// cfitsio fill a header (the data is padded with zeros).
    fn finish(mut self) -> Vec<u8> {
        self.raw(b"END");
        self.bytes.resize(self.bytes.len().div_ceil(BLOCK) * BLOCK, b' ');
        self.bytes
    }
}

/// Zero-pad data to the block.
fn pad(v: &mut Vec<u8>) {
    v.resize(v.len().div_ceil(BLOCK) * BLOCK, 0);
}

/// `fits_create_img(USHORT_IMG, 2, ...)`: the cards cfitsio writes for a
/// primary HDU (`SIMPLE`, with `EXTEND` and the standard comment) or an
/// `IMAGE` extension, ending with `BZERO 32768` / `BSCALE 1`.
fn image_cards(primary: bool, w: i32, h: i32) -> Cards {
    let mut c = Cards::new();
    if primary {
        c.logical("SIMPLE", true, "file does conform to FITS standard");
    } else {
        c.string("XTENSION", "IMAGE", "IMAGE extension");
    }
    c.int("BITPIX", 16, "number of bits per data pixel");
    c.int("NAXIS", 2, "number of data axes");
    c.int("NAXIS1", w as i64, "length of data axis 1");
    c.int("NAXIS2", h as i64, "length of data axis 2");
    if primary {
        c.logical("EXTEND", true, "FITS dataset may contain extensions");
        c.raw(b"COMMENT   FITS (Flexible Image Transport System) format is defined in 'Astronomy");
        c.raw(b"COMMENT   and Astrophysics', volume 376, page 359; bibcode: 2001A&A...376..359H");
    } else {
        c.int("PCOUNT", 0, "required keyword; must = 0");
        c.int("GCOUNT", 1, "required keyword; must = 1");
    }
    c.int("BZERO", 32768, "offset data range to that of unsigned short");
    c.int("BSCALE", 1, "default scaling factor");
    c
}

/// `fits_write_pix(TUSHORT)` under `BZERO 32768`: each pixel minus 32768 as
/// a big-endian `i16`, padded to the block.
fn image_data(img: &UsImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.data.len() * 2);
    for &v in &img.data {
        out.extend_from_slice(&((v as i32 - 32768) as i16).to_be_bytes());
    }
    pad(&mut out);
    out
}

/// `FITSHdrWriter::write(key, wxDateTime, UTC)`: `%Y-%m-%dT%H:%M:%S.mmm`.
fn date_text(t: &DateTime<Utc>) -> String {
    format!("{}.{:03}", t.format("%Y-%m-%dT%H:%M:%S"), t.timestamp_subsec_millis())
}

/// `usImage::Save` (usImage.cpp:293): one primary HDU, `BITPIX 16` /
/// `BZERO 32768`, PHD2's cards in its order, the pixels big-endian. `Err`
/// when a float card is NaN or infinite (pointing, pixel size, scale, lock
/// position): cfitsio refuses it with status 402, whose text this is, and
/// PHD2 debug-logs `CFITSIO error (402) saving image: <text>` and fails the save.
pub fn write_fits(img: &UsImage, hdr: &SaveHeader) -> Result<Vec<u8>, &'static str> {
    let mut c = image_cards(true, img.w, img.h);
    c.float("EXPOSURE", img.img_exp_dur as f32 / 1000.0, "Exposure time in seconds");
    if img.img_stack_cnt > 1 {
        c.int("STACKCNT", img.img_stack_cnt as u32 as i64, "Stacked frame count");
    }
    if !hdr.note.is_empty() {
        c.string("USERNOTE", &hdr.note, "");
    }
    c.string("DATE", &date_text(&hdr.date), "file creation time, UTC");
    c.string("DATE-OBS", &date_text(&img.img_start_time.with_timezone(&Utc)), "Image capture start time, UTC");
    c.string("CREATOR", &hdr.creator, "Capture software");
    c.string("PHDPROFI", &hdr.profile, "PHD2 Equipment Profile");
    if let Some(cam) = &hdr.camera {
        c.string("INSTRUME", &cam.name, "Instrument name");
        let b = cam.binning as i64;
        c.int("XBINNING", b, "Camera X Bin");
        c.int("YBINNING", b, "Camera Y Bin");
        c.int("CCDXBIN", b, "Camera X Bin");
        c.int("CCDYBIN", b, "Camera Y Bin");
        let sz = (cam.binning as f64 * cam.pixel_size) as f32;
        c.float("XPIXSZ", sz, "pixel size in microns (with binning)");
        c.float("YPIXSZ", sz, "pixel size in microns (with binning)");
        c.int("GAIN", cam.gain as i64, "PHD Gain Value (0-100)");
        c.int("CAMBPP", cam.bits_per_pixel as i64, "Camera resolution, bits per pixel");
    }
    if let Some((ra, dec)) = hdr.coords {
        c.float("RA", (ra * 360.0 / 24.0) as f32, "Object Right Ascension in degrees");
        c.float("DEC", dec as f32, "Object Declination in degrees");
        // usImage.cpp:353-373: sexagesimal by repeated truncation.
        let mut ra = ra;
        let h = ra as i32;
        ra -= h as f64;
        ra *= 60.0;
        let m = ra as i32;
        ra -= m as f64;
        ra *= 60.0;
        c.string("OBJCTRA", &format!("{h:02} {m:02} {ra:06.3}"), "Object Right Ascension in hms");
        // `dec *= sign`, not fabs: a -0.0 stays negative and its seconds
        // print as `-0.000`, as PHD2 writes them.
        let sign = if dec < 0.0 { -1.0 } else { 1.0 };
        let mut dec = dec * sign;
        let d = dec as i32;
        dec -= d as f64;
        dec *= 60.0;
        let m = dec as i32;
        dec -= m as f64;
        dec *= 60.0;
        let s = if sign < 0.0 { '-' } else { '+' };
        c.string("OBJCTDEC", &format!("{s}{d} {m:02} {dec:06.3}"), "Object Declination in dms");
    }
    if hdr.pier_side != PierSide::Unknown {
        c.int("PIERSIDE", hdr.pier_side as i64, "Side of Pier 0=East 1=West");
    }
    let sc = hdr.pixel_scale as f32;
    c.float("SCALE", sc, "Image scale (arcsec / pixel)");
    c.float("PIXSCALE", sc, "Image scale (arcsec / pixel)");
    c.int("PEDESTAL", img.pedestal as i64, "dark subtraction bias value");
    // `(1U << BitsPerPixel) - 1`: 0 for an unknown depth.
    let saturate = 1u32.wrapping_shl(img.bits_per_pixel as u32).wrapping_sub(1);
    c.int("SATURATE", saturate as i64, "Data value at which saturation occurs");
    if let Some(lock) = hdr.lock_pos {
        c.float("PHDLOCKX", lock.x as f32, "PHD2 lock position x");
        c.float("PHDLOCKY", lock.y as f32, "PHD2 lock position y");
    }
    if let Some(sf) = img.roi() {
        c.int("PHDSUBFX", sf.x as u32 as i64, "PHD2 subframe x");
        c.int("PHDSUBFY", sf.y as u32 as i64, "PHD2 subframe y");
        c.int("PHDSUBFW", sf.w as u32 as i64, "PHD2 subframe width");
        c.int("PHDSUBFH", sf.h as u32 as i64, "PHD2 subframe height");
    }
    if c.bad_float {
        return Err("bad float to string conversion");
    }
    let mut out = c.finish();
    out.extend_from_slice(&image_data(img));
    Ok(out)
}

/// The dark library file (`save_multi_darks`, myframe.cpp:2347): the darks
/// in the camera's order (its exposure-keyed `std::map`, ascending), the
/// first as the primary HDU, the others `IMAGE` extensions, each with
/// `EXPOSURE` (`ImgExpDur / 1000` in float) and the one `note` as `USERNOTE`
/// when it is not empty, then its pixels.
pub fn write_multi(darks: &[&UsImage], note: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, img) in darks.iter().enumerate() {
        let mut c = image_cards(i == 0, img.w, img.h);
        c.float("EXPOSURE", img.img_exp_dur as f32 / 1000.0, "Exposure time in seconds");
        if !note.is_empty() {
            c.string("USERNOTE", note, "");
        }
        out.extend_from_slice(&c.finish());
        out.extend_from_slice(&image_data(img));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A raw FITS file from cards (blank-padded here) and data (zero-padded here).
    fn raw_fits(cards: &[&str], data: &[u8]) -> Vec<u8> {
        let mut c = Cards::new();
        for card in cards {
            c.raw(card.as_bytes());
        }
        let mut out = c.finish();
        out.extend_from_slice(data);
        pad(&mut out);
        out
    }

    /// A 2-D primary header: `SIMPLE`, `BITPIX`, `NAXIS 2`, `NAXIS1/2`, then `extra`.
    fn image_fits(bitpix: i32, w: i64, h: i64, extra: &[&str], data: &[u8]) -> Vec<u8> {
        let mut cards = vec![
            "SIMPLE  =                    T".to_string(),
            format!("BITPIX  = {bitpix:>20}"),
            "NAXIS   =                    2".to_string(),
            format!("NAXIS1  = {w:>20}"),
            format!("NAXIS2  = {h:>20}"),
        ];
        cards.extend(extra.iter().map(|c| c.to_string()));
        raw_fits(&cards.iter().map(String::as_str).collect::<Vec<_>>(), data)
    }

    fn cards_of(bytes: &[u8]) -> Vec<String> {
        bytes[..BLOCK * 2]
            .chunks(CARD)
            .map(|c| String::from_utf8_lossy(c).trim_end().to_string())
            .take_while(|c| c != "END")
            .collect()
    }

    fn keywords(cards: &[String]) -> Vec<String> {
        cards.iter().filter(|c| c.contains("= ")).map(|c| c[..8].trim_end().to_string()).collect()
    }

    fn header() -> SaveHeader {
        SaveHeader {
            note: "it's a test".into(),
            date: Utc.with_ymd_and_hms(2026, 3, 4, 22, 15, 9).unwrap(),
            creator: "PHD2 2.6.14".into(),
            profile: "Sim".into(),
            camera: Some(CameraCards { name: "INDI Camera [CCD Simulator]".into(), binning: 2, pixel_size: 3.75, gain: 95, bits_per_pixel: 16 }),
            coords: Some((5.5755, -22.9725)),
            pier_side: PierSide::West,
            pixel_scale: 2.5,
            lock_pos: Some(Point::new(123.25, 45.5)),
        }
    }

    #[test]
    fn write_then_read_round_trips_pixels_and_phd_cards() {
        let mut img = UsImage::new(5, 4);
        img.data = vec![0, 1, 65535, 32768, 32767, 12345, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20];
        img.subframe = Some(Rect::new(1, 1, 3, 2));
        img.img_exp_dur = 1500;
        img.img_stack_cnt = 3;
        img.pedestal = 20;
        img.bits_per_pixel = 16;
        img.img_start_time = Utc.with_ymd_and_hms(2026, 3, 4, 22, 15, 7).unwrap().with_timezone(&chrono::Local);
        let bytes = write_fits(&img, &header()).unwrap();
        assert_eq!(bytes.len() % BLOCK, 0);
        assert_eq!(bytes.len(), 3 * BLOCK); // 42 cards: two header blocks, one data block

        let cards = cards_of(&bytes);
        let expected_order = [
            "SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "EXTEND", "BZERO", "BSCALE", "EXPOSURE", "STACKCNT", "USERNOTE",
            "DATE", "DATE-OBS", "CREATOR", "PHDPROFI", "INSTRUME", "XBINNING", "YBINNING", "CCDXBIN", "CCDYBIN", "XPIXSZ",
            "YPIXSZ", "GAIN", "CAMBPP", "RA", "DEC", "OBJCTRA", "OBJCTDEC", "PIERSIDE", "SCALE", "PIXSCALE", "PEDESTAL",
            "SATURATE", "PHDLOCKX", "PHDLOCKY", "PHDSUBFX", "PHDSUBFY", "PHDSUBFW", "PHDSUBFH",
        ];
        assert_eq!(keywords(&cards), expected_order);
        // cfitsio's layout: numbers right-justified to column 30, strings
        // padded to it, `%.7G` floats with a forced decimal point.
        assert!(cards.contains(&"EXPOSURE=                  1.5 / Exposure time in seconds".to_string()));
        assert!(cards.contains(&"BZERO   =                32768 / offset data range to that of unsigned short".to_string()));
        assert!(cards.contains(&"XPIXSZ  =                  7.5 / pixel size in microns (with binning)".to_string()));
        assert!(cards.contains(&"RA      =              83.6325 / Object Right Ascension in degrees".to_string()));
        assert!(cards.contains(&"OBJCTRA = '05 34 31.800'       / Object Right Ascension in hms".to_string()));
        assert!(cards.contains(&"OBJCTDEC= '-22 58 21.000'      / Object Declination in dms".to_string()));
        assert!(cards.contains(&"USERNOTE= 'it''s a test'".to_string()));
        assert!(cards.contains(&"DATE    = '2026-03-04T22:15:09.000' / file creation time, UTC".to_string()));
        assert!(cards.contains(&"DATE-OBS= '2026-03-04T22:15:07.000' / Image capture start time, UTC".to_string()));
        assert!(cards.contains(&"PHDPROFI= 'Sim     '           / PHD2 Equipment Profile".to_string()));
        assert!(cards.contains(&"PIERSIDE=                    1 / Side of Pier 0=East 1=West".to_string()));
        assert!(cards.contains(&"SATURATE=                65535 / Data value at which saturation occurs".to_string()));
        assert!(cards.contains(&"PHDLOCKX=               123.25 / PHD2 lock position x".to_string()));
        // The data: big-endian i16 of (v - 32768).
        assert_eq!(&bytes[2 * BLOCK..2 * BLOCK + 8], &[0x80, 0x00, 0x80, 0x01, 0x7f, 0xff, 0x00, 0x00]);

        let back = load_fits(&bytes, "x.fit").unwrap();
        assert_eq!((back.w, back.h), (5, 4));
        assert_eq!(back.data, img.data);
        assert_eq!(back.img_exp_dur, 1500);
        assert_eq!(back.img_stack_cnt, 3);
        assert_eq!(back.pedestal, 20);
        assert_eq!(back.bits_per_pixel, 16);
        assert_eq!(back.subframe, Some(Rect::new(1, 1, 3, 2)));
        // calc_stats ran, over the subframe: 7, 8, 9, 12, 13, 14.
        assert_eq!((back.min_adu, back.max_adu, back.median_adu), (7, 14, 12));

        // The camera reader takes the pixels only: no card, no stats.
        let blob = read_fits_blob(&bytes, None).unwrap();
        assert_eq!(blob.data, img.data);
        assert_eq!((blob.img_exp_dur, blob.img_stack_cnt, blob.pedestal, blob.bits_per_pixel), (0, 1, 0, 0));
        assert_eq!((blob.subframe, blob.median_adu), (None, 0));
    }

    #[test]
    fn header_is_blank_filled_and_data_zero_filled() {
        // cfitsio 4.7 fills a header block with 0x20 after END (checked on a
        // memfile it wrote, and on PHD2's own savetest.fit) and the data
        // block with 0x00.
        let mut img = UsImage::new(2, 1);
        img.data = vec![1, 65535];
        let bytes = write_fits(&img, &SaveHeader::default()).unwrap();
        let end = bytes[..BLOCK].chunks(CARD).position(|c| c.starts_with(b"END     ")).unwrap() * CARD;
        assert!(bytes[end + 3..BLOCK].iter().all(|&b| b == b' '));
        assert_eq!(&bytes[BLOCK..BLOCK + 4], &[0x80, 0x01, 0x7f, 0xff]);
        assert!(bytes[BLOCK + 4..].iter().all(|&b| b == 0));
        let multi = write_multi(&[&img, &img], "");
        for hdr in [0, 2 * BLOCK] {
            let end = multi[hdr..hdr + BLOCK].chunks(CARD).position(|c| c.starts_with(b"END     ")).unwrap() * CARD;
            assert!(multi[hdr + end + 3..hdr + BLOCK].iter().all(|&b| b == b' '));
        }
    }

    #[test]
    fn minimal_header_without_equipment() {
        let img = UsImage::new(2, 2);
        let bytes = write_fits(&img, &SaveHeader::default()).unwrap();
        let kw = keywords(&cards_of(&bytes));
        assert_eq!(
            kw,
            ["SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "EXTEND", "BZERO", "BSCALE", "EXPOSURE", "DATE", "DATE-OBS", "CREATOR", "PHDPROFI", "SCALE", "PIXSCALE", "PEDESTAL", "SATURATE"]
        );
        let cards = cards_of(&bytes);
        assert!(cards.contains(&"EXPOSURE=                   0. / Exposure time in seconds".to_string()));
        assert!(cards.contains(&"SATURATE=                    0 / Data value at which saturation occurs".to_string()));
        let back = load_fits(&bytes, "x.fit").unwrap();
        assert_eq!(back.bits_per_pixel, 8); // SATURATE 0 is not > 255
        assert_eq!(back.subframe, None);
    }

    #[test]
    fn string_cards_as_cfitsio_writes_them() {
        // Every expected card below is what cfitsio 4.7's fits_write_key
        // (TSTRING) wrote in a throwaway harness.
        let card = |key: &str, v: &str, comment: &str| {
            let mut c = Cards::new();
            c.string(key, v, comment);
            String::from_utf8(c.bytes).unwrap()
        };
        // Bytes outside printable ASCII become blanks, one per byte.
        assert_eq!(card("USERNOTE", "Télescope", "").trim_end(), "USERNOTE= 'T  lescope'");
        assert_eq!(card("TABS", "a\tb~\x7f", "c").trim_end(), "TABS    = 'a b~    '           / c");
        assert_eq!(card("EMPTY", "", "c").trim_end(), "EMPTY   = '        '           / c");
        // The 68-byte cap counts bytes: 34 × `é` + `z` is 68 blanks.
        let utf = "é".repeat(34) + "z";
        assert_eq!(card("UTF68", &utf, ""), format!("UTF68   = '{}'", " ".repeat(68)));
        // A doubled quote that does not fit is dropped with its pair.
        let a67 = "a".repeat(67);
        assert_eq!(card("QUOTE67", &format!("{a67}'b"), ""), format!("QUOTE67 = '{a67}' "));
        let a66 = "a".repeat(66);
        assert_eq!(card("QUOTE66", &format!("{a66}'b"), ""), format!("QUOTE66 = '{a66}'''"));
        assert_eq!(card("LONG70", &"a".repeat(70), "a comment"), format!("LONG70  = '{}'", "a".repeat(68)));
        // The comment only goes in while the card is under 77 bytes.
        assert_eq!(card("LEN64", &"a".repeat(64), "abcdef"), format!("LEN64   = '{}' / a", "a".repeat(64)));
        assert_eq!(card("LEN65", &"a".repeat(65), "abcdef"), format!("LEN65   = '{}'   ", "a".repeat(65)));
        assert_eq!(card("LEN66", &"a".repeat(66), "abcdef"), format!("LEN66   = '{}'  ", "a".repeat(66)));
    }

    #[test]
    fn non_finite_float_card_fails_the_save() {
        // cfitsio's ffr2e returns BAD_F2C (402) for NaN and infinity.
        let img = UsImage::new(1, 1);
        let mut hdr = header();
        hdr.coords = Some((f64::NAN, 10.0));
        assert_eq!(write_fits(&img, &hdr), Err("bad float to string conversion"));
        let mut hdr = header();
        hdr.pixel_scale = 1e39; // inf as a float
        assert_eq!(write_fits(&img, &hdr), Err("bad float to string conversion"));
    }

    #[test]
    fn objctdec_of_negative_zero_keeps_the_sign_in_the_seconds() {
        // PHD2's `dec *= sign` leaves -0.0 negative: "+0 00 -0.000".
        let mut hdr = SaveHeader::default();
        hdr.coords = Some((0.0, -0.0));
        let cards = cards_of(&write_fits(&UsImage::new(1, 1), &hdr).unwrap());
        assert!(cards.contains(&"OBJCTDEC= '+0 00 -0.000'       / Object Declination in dms".to_string()), "{cards:?}");
    }

    #[test]
    fn pixel_conversion_truncates_and_overflow_fails_the_read() {
        // Every expectation here is cfitsio 4.7's fits_read_pix(TUSHORT) on
        // the same bytes: the value is truncated, and anything outside
        // -0.49..=65535.49 returns NUM_OVERFLOW (412), which PHD2 alerts as
        // "Error reading data".
        let be32 = |v: &[f32]| v.iter().flat_map(|x| x.to_be_bytes()).collect::<Vec<u8>>();
        let f = image_fits(-32, 6, 1, &[], &be32(&[1.4, 1.5, 1.6, 65535.4, -0.4, 65535.49]));
        assert_eq!(read_fits_blob(&f, None).unwrap().data, [1, 1, 1, 65535, 0, 65535]);
        for bad in [[1.4f32, 1.5, 70000.0, -3.0].as_slice(), &[-0.5], &[-0.49], &[65535.5]] {
            let f = image_fits(-32, bad.len() as i64, 1, &[], &be32(bad));
            assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Error reading data", "{bad:?}");
        }
        // A NaN is no overflow: cfitsio returns 0.
        let f = image_fits(-32, 2, 1, &[], &be32(&[f32::NAN, 5.0]));
        assert_eq!(read_fits_blob(&f, None).unwrap().data, [0, 5]);

        let f = image_fits(-64, 1, 1, &[], &65534.6f64.to_be_bytes());
        assert_eq!(read_fits_blob(&f, None).unwrap().data, [65534]);

        // Scaled: 3 * 2 + 1.5 = 7.5, truncated; Fortran exponent accepted.
        let f = image_fits(16, 1, 1, &["BZERO   =               1.5D+0", "BSCALE  =                   2."], &3i16.to_be_bytes());
        assert_eq!(read_fits_blob(&f, None).unwrap().data, [7]);

        let data: Vec<u8> = [-32768i16, 0, 32767].iter().flat_map(|v| v.to_be_bytes()).collect();
        let f = image_fits(16, 3, 1, &["BZERO   =                32768", "BSCALE  =                    1"], &data);
        assert_eq!(read_fits_blob(&f, None).unwrap().data, [0, 32768, 65535]);

        // Signed 16-bit without BZERO: a negative pixel fails the read.
        let data: Vec<u8> = [-5i16, 7].iter().flat_map(|v| v.to_be_bytes()).collect();
        assert_eq!(read_fits_blob(&image_fits(16, 2, 1, &[], &data), None).unwrap_err(), "Error reading data");

        let data: Vec<u8> = [65535i32, 0, 7].iter().flat_map(|v| v.to_be_bytes()).collect();
        assert_eq!(read_fits_blob(&image_fits(32, 3, 1, &[], &data), None).unwrap().data, [65535, 0, 7]);
        let f = image_fits(32, 1, 1, &[], &100_000i32.to_be_bytes());
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Error reading data");

        let data: Vec<u8> = [9i64, 65535].iter().flat_map(|v| v.to_be_bytes()).collect();
        let f = image_fits(64, 2, 1, &[], &data);
        assert_eq!(read_fits_blob(&f, None).unwrap().data, [9, 65535]);

        assert_eq!(read_fits_blob(&image_fits(8, 3, 1, &[], &[0, 200, 255]), None).unwrap().data, [0, 200, 255]);
        let f = image_fits(8, 3, 1, &["BZERO   =                 -1.0"], &[0, 200, 255]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Error reading data");
    }

    #[test]
    fn shapes_hdu_count_and_open_failures() {
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    3", "NAXIS1  =                    1", "NAXIS2  =                    1", "NAXIS3  =                    3"], &[1, 2, 3]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "RGB images are not supported, please switch the INDI driver to Mono");
        // usImage::Load has no RGB branch.
        assert_eq!(load_fits(&f, "a.fit").unwrap_err(), "Unsupported type or read error loading FITS file a.fit");

        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    1", "NAXIS1  =                    2"], &[1, 2]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Unsupported FITS file");

        // cfitsio counts a binary table after the image: two HDUs.
        let mut f = image_fits(8, 2, 2, &["EXTEND  =                    T"], &[1, 2, 3, 4]);
        f.extend(raw_fits(
            &["XTENSION= 'BINTABLE'", "BITPIX  =                    8", "NAXIS   =                    2", "NAXIS1  =                    4", "NAXIS2  =                    1", "PCOUNT  =                    0", "GCOUNT  =                    1", "TFIELDS =                    1", "TFORM1  = '1J      '"],
            &[0, 0, 0, 5],
        ));
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Unsupported FITS file");
        assert_eq!(load_fits(&f, "a.fit").unwrap_err(), "Unsupported type or read error loading FITS file a.fit");

        // Trailing junk after the last HDU is ignored (a block of `X`, of
        // zeros, or a short tail): still one HDU.
        let base = image_fits(8, 2, 2, &[], &[1, 2, 3, 4]);
        for junk in [vec![b'X'; BLOCK], vec![0; BLOCK], vec![b'X'; 100]] {
            let mut f = base.clone();
            f.extend(junk);
            assert_eq!(read_fits_blob(&f, None).unwrap().data, [1, 2, 3, 4]);
        }

        // Data short of NAXIS1 × NAXIS2: the file opens and the read fails
        // (cfitsio END_OF_FILE at fits_read_pix).
        let mut f = image_fits(8, 2, 2, &[], &[]);
        f.extend([1, 2]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Error reading data");
        assert_eq!(load_fits(&f, "a.fit").unwrap_err(), "Error reading data from FITS file a.fit");

        // A negative axis (cfitsio BAD_NAXES, 213) or no SIMPLE card (252)
        // fail at open; huge axes open and fail at the read, without panicking.
        let f = image_fits(8, -1, 2, &[], &[]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Unsupported type or read error loading FITS file");
        assert_eq!(load_fits(&f, "a.fit").unwrap_err(), "Error opening FITS file a.fit");
        let f = raw_fits(&["BITPIX  =                    8", "NAXIS   =                    2", "NAXIS1  =                    1", "NAXIS2  =                    1"], &[1]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Unsupported type or read error loading FITS file");
        let f = image_fits(8, 2_000_000_000, 2_000_000_000, &[], &[]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Error reading data");
        let f = image_fits(16, i64::MAX, 3, &[], &[]);
        assert_eq!(read_fits_blob(&f, None).unwrap_err(), "Error reading data");
    }

    #[test]
    fn blob_subframe_goes_into_a_blank_full_frame() {
        let f = image_fits(8, 2, 2, &[], &[1, 2, 3, 4]);
        let img = read_fits_blob(&f, Some(((4, 3), Rect::new(1, 1, 2, 2)))).unwrap();
        assert_eq!((img.w, img.h), (4, 3));
        assert_eq!(img.subframe, Some(Rect::new(1, 1, 2, 2)));
        assert_eq!(img.data, [0, 0, 0, 0, 0, 1, 2, 0, 0, 3, 4, 0]);
        // A subframe running past the frame's end, or larger than the blob:
        // PHD2 would copy out of bounds.
        assert_eq!(read_fits_blob(&f, Some(((4, 2), Rect::new(1, 1, 2, 2)))).unwrap_err(), "Error reading data");
        assert_eq!(read_fits_blob(&f, Some(((4, 4), Rect::new(0, 0, 3, 3)))).unwrap_err(), "Error reading data");
    }

    #[test]
    fn exposure_card_is_truncated_to_ms_as_load_does() {
        // 0.7 as a float is 0.699999988…: usImage::Load truncates to 699 ms,
        // the dark library rounds to 700.
        let f = image_fits(8, 1, 1, &["EXPOSURE=                  0.7"], &[1]);
        assert_eq!(load_fits(&f, "a.fit").unwrap().img_exp_dur, 699);
        let darks = read_multi(&f, "d.fit", 0);
        assert_eq!(darks.error, None);
        assert_eq!(darks.frames[0].0.img_exp_dur, 700);
        assert!(!darks.frames[0].1);
    }

    #[test]
    fn multi_hdu_darks_round_trip() {
        let mut d1 = UsImage::new(3, 2);
        d1.data = vec![10, 20, 30, 40, 50, 60];
        d1.img_exp_dur = 700;
        let mut d2 = UsImage::new(3, 2);
        d2.data = vec![1, 2, 3, 4, 5, 60000];
        d2.img_exp_dur = 1500;
        let mut d3 = UsImage::new(3, 2);
        d3.img_exp_dur = 2000;
        let bytes = write_multi(&[&d1, &d2, &d3], "dark library");
        assert_eq!(bytes.len(), 6 * BLOCK);
        // The second HDU is an IMAGE extension with PCOUNT / GCOUNT.
        let ext: Vec<String> = bytes[2 * BLOCK..3 * BLOCK].chunks(CARD).map(|c| String::from_utf8_lossy(c).trim_end().to_string()).collect();
        assert_eq!(ext[0], "XTENSION= 'IMAGE   '           / IMAGE extension");
        assert_eq!(keywords(&ext), ["XTENSION", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "PCOUNT", "GCOUNT", "BZERO", "BSCALE", "EXPOSURE", "USERNOTE"]);
        assert!(ext.contains(&"EXPOSURE=                  1.5 / Exposure time in seconds".to_string()));
        assert!(ext.contains(&"USERNOTE= 'dark library'".to_string()));
        let last: Vec<String> = bytes[4 * BLOCK..5 * BLOCK].chunks(CARD).map(|c| String::from_utf8_lossy(c).trim_end().to_string()).collect();
        assert!(last.contains(&"EXPOSURE=                   2. / Exposure time in seconds".to_string()));
        assert!(last.contains(&"USERNOTE= 'dark library'".to_string()));
        assert!(!keywords(&cards_of(&write_multi(&[&d1], ""))).contains(&"USERNOTE".to_string()));

        let darks = read_multi(&bytes, "d.fit", 0);
        assert_eq!(darks.error, None);
        assert_eq!(darks.frames.len(), 3);
        assert_eq!(darks.frames.iter().map(|d| d.0.img_exp_dur).collect::<Vec<_>>(), [700, 1500, 2000]);
        assert_eq!(darks.frames[0].0.data, d1.data);
        assert_eq!(darks.frames[1].0.data, d2.data);
        assert_eq!(darks.frames[1].0.median_adu, 4); // calc_stats ran
        // The single-image readers insist on one HDU.
        assert_eq!(read_fits_blob(&bytes, None).unwrap_err(), "Unsupported FITS file");
        // Trailing junk after the last HDU does not fail the load.
        let mut junk = bytes.clone();
        junk.extend(vec![b'X'; BLOCK]);
        assert_eq!(read_multi(&junk, "d.fit", 0).frames.len(), 3);
    }

    #[test]
    fn multi_hdu_fallback_exposure_size_check_and_partial_load() {
        let f = image_fits(8, 1, 1, &[], &[1]);
        let darks = read_multi(&f, "d.fit", 2500);
        assert_eq!(darks.frames[0].0.img_exp_dur, 2500);
        assert!(darks.frames[0].1); // the fallback was used

        // A size change stops the load; the frame read before it is kept,
        // as PHD2 has already added it to the camera.
        let a = UsImage::new(2, 2);
        let b = UsImage::new(3, 2);
        let darks = read_multi(&write_multi(&[&a, &b], ""), "d.fit", 0);
        assert_eq!(darks.frames.len(), 1);
        assert_eq!(
            darks.error.as_deref(),
            Some("Existing dark library has frames with incompatible formats - please rebuild the dark library from scratch.")
        );

        // A binary table HDU: "not of an image", after the frames before it.
        let mut f = write_multi(&[&a], "");
        f.extend(raw_fits(&["XTENSION= 'BINTABLE'", "BITPIX  =                    8", "NAXIS   =                    2", "NAXIS1  =                    4", "NAXIS2  =                    1", "PCOUNT  =                    0", "GCOUNT  =                    1", "TFIELDS =                    1"], &[0; 4]));
        let darks = read_multi(&f, "d.fit", 0);
        assert_eq!((darks.frames.len(), darks.error.as_deref()), (1, Some("FITS file is not of an image: d.fit")));

        assert_eq!(read_multi(b"garbage", "d.fit", 0).error.as_deref(), Some("Error opening FITS file d.fit"));
        let mut short = write_multi(&[&a, &a], "");
        short.truncate(3 * BLOCK + 4); // the second HDU's data cut short
        let darks = read_multi(&short, "d.fit", 0);
        assert_eq!((darks.frames.len(), darks.error.as_deref()), (1, Some("Error reading data from d.fit")));
    }

    #[test]
    fn cfitsio_float_formatting() {
        assert_eq!(float_text(1.5), "1.5");
        assert_eq!(float_text(2.0), "2.");
        assert_eq!(float_text(0.1), "0.1");
        assert_eq!(float_text(1e7), "1.0E+07");
        assert_eq!(float_text(123456.789), "123456.8");
        assert_eq!(float_text(0.0001234), "0.0001234");
        assert_eq!(float_text(2.5e-5), "2.5E-05");
        assert_eq!(float_text(65535.0), "65535.");
        assert_eq!(float_text(0.0), "0.");
        assert_eq!(float_text(-0.0), "-0."); // cfitsio 4.7 writes -0.0 as `-0.`
        assert_eq!(float_text(-3.25), "-3.25");
    }
}
