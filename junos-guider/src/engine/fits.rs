//! FITS in and out of a [`UsImage`], without cfitsio: the reader the INDI
//! camera blob and `usImage::Load` go through, the writer behind
//! `usImage::Save` with PHD2's header cards, and the multi-HDU form the dark
//! library is stored in.
//!
//! Only what PHD2 reads and writes is implemented: the primary HDU and
//! `IMAGE` extensions, 2-D data, BITPIX 8 / 16 / 32 / −32 / −64 scaled by
//! `BSCALE` / `BZERO` and converted to `u16` the way `fits_read_pix(TUSHORT)`
//! does it; on the way out `BITPIX 16` with `BZERO 32768`, the cfitsio
//! `USHORT_IMG` layout, card by card as cfitsio formats them (so a header
//! written here is byte-for-byte what PHD2 would write).
//!
//! Ported from PHD2 src/usImage.cpp, src/cam_indi.cpp, src/myframe.cpp, src/fitsiowrap.h — BSD-3-Clause, see THIRD_PARTY.md

use chrono::{DateTime, Utc};

use super::image::UsImage;
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

    /// `fits_read_key(TINT)`: an integer card, or a real one truncated.
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
/// data that follows (the next 2880-byte boundary after the END card).
fn parse_header(bytes: &[u8], start: usize) -> Result<(Header, usize), String> {
    let mut cards = Vec::new();
    let mut off = start;
    loop {
        let card = bytes.get(off..off + CARD).ok_or("truncated FITS header")?;
        off += CARD;
        let key = std::str::from_utf8(&card[..8]).map_err(|_| "bad FITS keyword")?.trim_end();
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
    Ok((Header { cards }, off.div_ceil(BLOCK) * BLOCK))
}

/// One HDU located in the file.
struct Hdu {
    header: Header,
    bitpix: i64,
    naxis: i64,
    axes: Vec<i64>,
    data_start: usize,
    /// Bytes of data (before the block padding).
    data_len: usize,
}

impl Hdu {
    /// The offset of the next HDU.
    fn end(&self) -> usize {
        (self.data_start + self.data_len).div_ceil(BLOCK) * BLOCK
    }
}

fn read_hdu(bytes: &[u8], start: usize) -> Result<Hdu, String> {
    let (header, data_start) = parse_header(bytes, start)?;
    if start > 0 {
        // Only an IMAGE extension is an image HDU (fits_get_hdu_type).
        match header.string("XTENSION").as_deref() {
            Some("IMAGE") => {}
            _ => return Err("FITS file is not of an image".into()),
        }
    }
    let bitpix = header.int("BITPIX").ok_or("missing BITPIX")?;
    let naxis = header.int("NAXIS").unwrap_or(0);
    let mut axes = Vec::new();
    for i in 1..=naxis {
        axes.push(header.int(&format!("NAXIS{i}")).ok_or("missing NAXISn")?);
    }
    let data_len = if naxis == 0 {
        0
    } else {
        let pcount = header.int("PCOUNT").unwrap_or(0);
        let gcount = header.int("GCOUNT").unwrap_or(1);
        let n: i64 = axes.iter().product();
        (bitpix.unsigned_abs() as usize / 8) * (gcount.max(0) as usize) * ((pcount + n).max(0) as usize)
    };
    if bytes.len() < data_start + data_len {
        return Err("truncated FITS data".into());
    }
    Ok(Hdu { header, bitpix, naxis, axes, data_start, data_len })
}

/// `fits_get_num_hdus`: how many HDUs follow each other in the file.
fn hdu_count(bytes: &[u8]) -> usize {
    let mut n = 0;
    let mut off = 0;
    while off + BLOCK <= bytes.len() {
        match read_hdu(bytes, off) {
            Ok(h) => {
                n += 1;
                off = h.end();
            }
            Err(_) => break,
        }
    }
    n
}

/// The pixels as `fits_read_pix(TUSHORT)` returns them: `raw * BSCALE +
/// BZERO`, rounded half up and clamped to 0..=65535 (cfitsio also flags the
/// clamped values as an overflow; that status is not reproduced, the value
/// is kept).
fn decode_pixels(bytes: &[u8], hdu: &Hdu, n: usize) -> Result<Vec<u16>, String> {
    let bscale = hdu.header.float("BSCALE").unwrap_or(1.0);
    let bzero = hdu.header.float("BZERO").unwrap_or(0.0);
    let bytes_per_px = hdu.bitpix.unsigned_abs() as usize / 8;
    let raw = &bytes[hdu.data_start..hdu.data_start + n * bytes_per_px];
    let conv = |v: f64| (v * bscale + bzero + 0.5) as u16;
    let out = match hdu.bitpix {
        8 => raw.iter().map(|&b| conv(b as f64)).collect(),
        16 => raw.chunks_exact(2).map(|c| conv(i16::from_be_bytes([c[0], c[1]]) as f64)).collect(),
        32 => raw.chunks_exact(4).map(|c| conv(i32::from_be_bytes([c[0], c[1], c[2], c[3]]) as f64)).collect(),
        -32 => raw.chunks_exact(4).map(|c| conv(f32::from_be_bytes([c[0], c[1], c[2], c[3]]) as f64)).collect(),
        -64 => raw
            .chunks_exact(8)
            .map(|c| conv(f64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])))
            .collect(),
        other => return Err(format!("unsupported BITPIX {other}")),
    };
    Ok(out)
}

/// A 2-D image HDU into a fresh [`UsImage`] (no stats, no header cards).
fn load_image(bytes: &[u8], hdu: &Hdu) -> Result<UsImage, String> {
    if hdu.naxis != 2 {
        return Err("Unsupported FITS file".into());
    }
    let (w, h) = (hdu.axes[0] as i32, hdu.axes[1] as i32);
    let mut img = UsImage::new(w, h);
    img.data = decode_pixels(bytes, hdu, (w as usize) * (h as usize))?;
    Ok(img)
}

/// Read a single-HDU FITS image as the INDI camera (`CameraINDI::ReadFITS`,
/// cam_indi.cpp:838) and `usImage::Load` (usImage.cpp:434) do: one HDU,
/// `NAXIS == 2` (a 3-axis file is the RGB refusal), then PHD2's own cards
/// read back — `EXPOSURE` (truncated to ms as `Load` does), `STACKCNT`,
/// `PEDESTAL`, `SATURATE` (bit depth 16 when > 255, else 8) and the
/// `PHDSUBFX/Y/W/H` subframe. Statistics are not computed: the capture
/// pipeline runs noise reduction first, then `calc_stats`.
pub fn read_fits(bytes: &[u8]) -> Result<UsImage, String> {
    let hdu = read_hdu(bytes, 0)?;
    if hdu.naxis == 3 {
        return Err("RGB images are not supported, please switch the INDI driver to Mono".into());
    }
    if hdu_count(bytes) != 1 || hdu.naxis != 2 {
        return Err("Unsupported FITS file".into());
    }
    let mut img = load_image(bytes, &hdu)?;
    let h = &hdu.header;
    if let Some(exposure) = h.float("EXPOSURE") {
        // usImage.cpp:486: a float header value times 1000.0, truncated.
        img.img_exp_dur = (exposure as f32 as f64 * 1000.0) as i32;
    }
    if let Some(v) = h.int("STACKCNT") {
        img.img_stack_cnt = v as i32;
    }
    if let Some(v) = h.int("PEDESTAL") {
        img.pedestal = v as u16;
    }
    if let Some(v) = h.int("SATURATE") {
        img.bits_per_pixel = if v > 255 { 16 } else { 8 };
    }
    if let (Some(x), Some(y), Some(w), Some(hh)) =
        (h.int("PHDSUBFX"), h.int("PHDSUBFY"), h.int("PHDSUBFW"), h.int("PHDSUBFH"))
    {
        img.subframe = Some(Rect::new(x as i32, y as i32, w as i32, hh as i32));
    }
    Ok(img)
}

/// `ROUNDF(exposure * 1000.0)` (myframe.cpp:2478): the float exposure times
/// a double, rounded through `floorf`.
fn roundf_ms(exposure: f32) -> i32 {
    ((exposure as f64 * 1000.0 + 0.5) as f32).floor() as i32
}

/// Read the dark library file as `load_multi_darks` (myframe.cpp:2404): every
/// HDU must be a 2-D image of the same size; each frame gets its `EXPOSURE`
/// (rounded to ms) — or `fallback_exp_ms`, PHD2's requested exposure, when
/// the card is missing — and its statistics.
pub fn read_multi(bytes: &[u8], fallback_exp_ms: i32) -> Result<Vec<UsImage>, String> {
    let mut out = Vec::new();
    let mut off = 0;
    let mut size: Option<(i64, i64)> = None;
    loop {
        let hdu = read_hdu(bytes, off)?;
        if hdu.naxis != 2 {
            return Err("Unsupported type or read error loading FITS file".into());
        }
        let this = (hdu.axes[0], hdu.axes[1]);
        if size.is_some_and(|s| s != this) {
            return Err("Existing dark library has frames with incompatible formats - please rebuild the dark library from scratch.".into());
        }
        size = Some(this);
        let mut img = load_image(bytes, &hdu)?;
        let exposure = match hdu.header.float("EXPOSURE") {
            Some(e) => e as f32,
            None => fallback_exp_ms as f32 / 1000.0,
        };
        img.img_exp_dur = roundf_ms(exposure);
        img.calc_stats();
        out.push(img);
        off = hdu.end();
        if off + BLOCK > bytes.len() {
            break;
        }
    }
    Ok(out)
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

/// C's `%.{prec}G` of a double (what cfitsio's `ffr2e` uses for a float
/// card with `decim = -7`): the shorter of fixed and exponent notation,
/// trailing zeros removed, at least two exponent digits.
fn c_g(v: f64, prec: usize) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{:.*e}", prec - 1, v);
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if exp < -4 || exp >= prec as i32 {
        format!("{}E{}{:02}", strip_zeros(mant), if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        strip_zeros(&format!("{:.*}", (prec as i32 - 1 - exp) as usize, v))
    }
}

fn strip_zeros(s: &str) -> String {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() }
}

/// cfitsio's text for a `TFLOAT` card (`ffr2e`): `%.7G`, then a decimal
/// point is forced — `2` becomes `2.`, `1E+07` is rewritten as `1.0E+07`.
fn float_text(v: f32) -> String {
    let s = c_g(v as f64, 7);
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
struct Cards(Vec<u8>);

impl Cards {
    fn raw(&mut self, text: &str) {
        let mut card: Vec<u8> = text.bytes().take(CARD).collect();
        card.resize(CARD, b' ');
        self.0.extend_from_slice(&card);
    }

    fn value(&mut self, key: &str, value: &str, comment: &str) {
        let mut card = format!("{key:<8}= {value:>20}");
        if !comment.is_empty() {
            card.push_str(" / ");
            card.push_str(comment);
        }
        self.raw(&card);
    }

    fn logical(&mut self, key: &str, v: bool, comment: &str) {
        self.value(key, if v { "T" } else { "F" }, comment);
    }

    fn int(&mut self, key: &str, v: i64, comment: &str) {
        self.value(key, &v.to_string(), comment);
    }

    fn float(&mut self, key: &str, v: f32, comment: &str) {
        self.value(key, &float_text(v), comment);
    }

    /// `ffs2c` + `ffmkky`: quotes doubled, padded to 8 characters inside the
    /// quotes, at most 68 characters, then padded to column 30.
    fn string(&mut self, key: &str, v: &str, comment: &str) {
        let escaped: String = v.replace('\'', "''").chars().take(68).collect();
        let mut card = format!("{key:<8}= '{escaped:<8}'");
        while card.len() < 30 {
            card.push(' ');
        }
        if !comment.is_empty() {
            card.push_str(" / ");
            card.push_str(comment);
        }
        self.raw(&card);
    }

    /// The END card and the block padding.
    fn finish(mut self) -> Vec<u8> {
        self.raw("END");
        pad(&mut self.0);
        self.0
    }
}

fn pad(v: &mut Vec<u8>) {
    v.resize(v.len().div_ceil(BLOCK) * BLOCK, 0);
}

/// `fits_create_img(USHORT_IMG, 2, ...)`: the cards cfitsio writes for a
/// primary HDU (`SIMPLE`, with `EXTEND` and the standard comment) or an
/// `IMAGE` extension, ending with `BZERO 32768` / `BSCALE 1`.
fn image_cards(primary: bool, w: i32, h: i32) -> Cards {
    let mut c = Cards(Vec::new());
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
        c.raw("COMMENT   FITS (Flexible Image Transport System) format is defined in 'Astronomy");
        c.raw("COMMENT   and Astrophysics', volume 376, page 359; bibcode: 2001A&A...376..359H");
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
/// `BZERO 32768`, PHD2's cards in its order, the pixels big-endian.
pub fn write_fits(img: &UsImage, hdr: &SaveHeader) -> Vec<u8> {
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
        let sign = if dec < 0.0 { '-' } else { '+' };
        let mut dec = dec.abs();
        let d = dec as i32;
        dec -= d as f64;
        dec *= 60.0;
        let m = dec as i32;
        dec -= m as f64;
        dec *= 60.0;
        c.string("OBJCTDEC", &format!("{sign}{d} {m:02} {dec:06.3}"), "Object Declination in dms");
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
    let mut out = c.finish();
    out.extend_from_slice(&image_data(img));
    out
}

/// The dark library file (`save_multi_darks`, myframe.cpp:2347): the first
/// frame is the primary HDU, the others `IMAGE` extensions, each with
/// `EXPOSURE` (seconds, float) and `USERNOTE` when the note is not empty,
/// then its pixels.
pub fn write_multi(darks: &[(&UsImage, f32, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, (img, exposure, note)) in darks.iter().enumerate() {
        let mut c = image_cards(i == 0, img.w, img.h);
        c.float("EXPOSURE", *exposure, "Exposure time in seconds");
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

    /// A raw FITS file from cards (padded here) and data (padded here).
    fn raw_fits(cards: &[&str], data: &[u8]) -> Vec<u8> {
        let mut c = Cards(Vec::new());
        for card in cards {
            c.raw(card);
        }
        let mut out = c.finish();
        out.extend_from_slice(data);
        pad(&mut out);
        out
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
        let hdr = SaveHeader {
            note: "it's a test".into(),
            date: Utc.with_ymd_and_hms(2026, 3, 4, 22, 15, 9).unwrap(),
            creator: "PHD2 2.6.14".into(),
            profile: "Sim".into(),
            camera: Some(CameraCards { name: "INDI Camera [CCD Simulator]".into(), binning: 2, pixel_size: 3.75, gain: 95, bits_per_pixel: 16 }),
            coords: Some((5.5755, -22.9725)),
            pier_side: PierSide::West,
            pixel_scale: 2.5,
            lock_pos: Some(Point::new(123.25, 45.5)),
        };
        let bytes = write_fits(&img, &hdr);
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

        let back = read_fits(&bytes).unwrap();
        assert_eq!((back.w, back.h), (5, 4));
        assert_eq!(back.data, img.data);
        assert_eq!(back.img_exp_dur, 1500);
        assert_eq!(back.img_stack_cnt, 3);
        assert_eq!(back.pedestal, 20);
        assert_eq!(back.bits_per_pixel, 16);
        assert_eq!(back.subframe, Some(Rect::new(1, 1, 3, 2)));
    }

    #[test]
    fn minimal_header_without_equipment() {
        let img = UsImage::new(2, 2);
        let bytes = write_fits(&img, &SaveHeader::default());
        let kw = keywords(&cards_of(&bytes));
        assert_eq!(
            kw,
            ["SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "EXTEND", "BZERO", "BSCALE", "EXPOSURE", "DATE", "DATE-OBS", "CREATOR", "PHDPROFI", "SCALE", "PIXSCALE", "PEDESTAL", "SATURATE"]
        );
        let cards = cards_of(&bytes);
        assert!(cards.contains(&"EXPOSURE=                   0. / Exposure time in seconds".to_string()));
        assert!(cards.contains(&"SATURATE=                    0 / Data value at which saturation occurs".to_string()));
        let back = read_fits(&bytes).unwrap();
        assert_eq!(back.bits_per_pixel, 8); // SATURATE 0 is not > 255
        assert_eq!(back.subframe, None);
    }

    #[test]
    fn bitpix16_with_bzero_decodes_to_u16() {
        let data: Vec<u8> = [-32768i16, 0, 32767].iter().flat_map(|v| v.to_be_bytes()).collect();
        let f = raw_fits(
            &["SIMPLE  =                    T", "BITPIX  =                   16", "NAXIS   =                    2", "NAXIS1  =                    3", "NAXIS2  =                    1", "BZERO   =                32768 / offset", "BSCALE  =                    1"],
            &data,
        );
        assert_eq!(read_fits(&f).unwrap().data, [0, 32768, 65535]);

        // Signed 16-bit without BZERO: negatives clamp to 0, as TUSHORT does.
        let data: Vec<u8> = [-5i16, 7].iter().flat_map(|v| v.to_be_bytes()).collect();
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                   16", "NAXIS   =                    2", "NAXIS1  =                    2", "NAXIS2  =                    1"], &data);
        assert_eq!(read_fits(&f).unwrap().data, [0, 7]);

        // Scaled: 3 * 2 + 1.5 = 7.5, rounded half up; Fortran exponent accepted.
        let data = 3i16.to_be_bytes();
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                   16", "NAXIS   =                    2", "NAXIS1  =                    1", "NAXIS2  =                    1", "BZERO   =               1.5D+0", "BSCALE  =                   2."], &data);
        assert_eq!(read_fits(&f).unwrap().data, [8]);
    }

    #[test]
    fn bitpix8_stays_in_byte_range_and_floats_clamp() {
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    2", "NAXIS1  =                    3", "NAXIS2  =                    1"], &[0, 200, 255]);
        assert_eq!(read_fits(&f).unwrap().data, [0, 200, 255]);

        let data: Vec<u8> = [1.4f32, 1.5, 70000.0, -3.0].iter().flat_map(|v| v.to_be_bytes()).collect();
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                  -32", "NAXIS   =                    2", "NAXIS1  =                    4", "NAXIS2  =                    1"], &data);
        assert_eq!(read_fits(&f).unwrap().data, [1, 2, 65535, 0]);

        let data: Vec<u8> = [65534.6f64].iter().flat_map(|v| v.to_be_bytes()).collect();
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                  -64", "NAXIS   =                    2", "NAXIS1  =                    1", "NAXIS2  =                    1"], &data);
        assert_eq!(read_fits(&f).unwrap().data, [65535]);

        let data = 100_000i32.to_be_bytes();
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                   32", "NAXIS   =                    2", "NAXIS1  =                    1", "NAXIS2  =                    1"], &data);
        assert_eq!(read_fits(&f).unwrap().data, [65535]);
    }

    #[test]
    fn naxis3_is_the_rgb_refusal_and_other_shapes_are_unsupported() {
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    3", "NAXIS1  =                    1", "NAXIS2  =                    1", "NAXIS3  =                    3"], &[1, 2, 3]);
        let err = read_fits(&f).unwrap_err();
        assert!(err.contains("switch the INDI driver to Mono"), "{err}");

        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    1", "NAXIS1  =                    2"], &[1, 2]);
        assert_eq!(read_fits(&f).unwrap_err(), "Unsupported FITS file");

        let mut f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    2", "NAXIS1  =                    2", "NAXIS2  =                    2"], &[]);
        f.extend([1, 2]); // two of the four pixels
        assert_eq!(read_fits(&f).unwrap_err(), "truncated FITS data");
    }

    #[test]
    fn exposure_card_is_truncated_to_ms_as_load_does() {
        // 0.7 as a float is 0.699999988…: usImage::Load truncates to 699 ms,
        // the dark library rounds to 700.
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    2", "NAXIS1  =                    1", "NAXIS2  =                    1", "EXPOSURE=                  0.7"], &[1]);
        assert_eq!(read_fits(&f).unwrap().img_exp_dur, 699);
        assert_eq!(read_multi(&f, 0).unwrap()[0].img_exp_dur, 700);
    }

    #[test]
    fn multi_hdu_darks_round_trip() {
        let mut d1 = UsImage::new(3, 2);
        d1.data = vec![10, 20, 30, 40, 50, 60];
        let mut d2 = UsImage::new(3, 2);
        d2.data = vec![1, 2, 3, 4, 5, 60000];
        let d3 = UsImage::new(3, 2);
        let bytes = write_multi(&[(&d1, 0.7, "dark library"), (&d2, 1.5, "dark library"), (&d3, 2.0, "")]);
        assert_eq!(bytes.len(), 6 * BLOCK);
        // The second HDU is an IMAGE extension with PCOUNT / GCOUNT.
        let ext: Vec<String> = bytes[2 * BLOCK..3 * BLOCK].chunks(CARD).map(|c| String::from_utf8_lossy(c).trim_end().to_string()).collect();
        assert_eq!(ext[0], "XTENSION= 'IMAGE   '           / IMAGE extension");
        assert_eq!(keywords(&ext), ["XTENSION", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "PCOUNT", "GCOUNT", "BZERO", "BSCALE", "EXPOSURE", "USERNOTE"]);
        assert!(ext.contains(&"EXPOSURE=                  1.5 / Exposure time in seconds".to_string()));
        assert!(ext.contains(&"USERNOTE= 'dark library'".to_string()));
        let last: Vec<String> = bytes[4 * BLOCK..5 * BLOCK].chunks(CARD).map(|c| String::from_utf8_lossy(c).trim_end().to_string()).collect();
        assert!(last.contains(&"EXPOSURE=                   2. / Exposure time in seconds".to_string()));
        assert!(!last.iter().any(|c| c.starts_with("USERNOTE")));

        let darks = read_multi(&bytes, 0).unwrap();
        assert_eq!(darks.len(), 3);
        assert_eq!(darks.iter().map(|d| d.img_exp_dur).collect::<Vec<_>>(), [700, 1500, 2000]);
        assert_eq!(darks[0].data, d1.data);
        assert_eq!(darks[1].data, d2.data);
        assert_eq!(darks[1].median_adu, 4); // calc_stats ran
        // The single-image reader insists on one HDU.
        assert_eq!(read_fits(&bytes).unwrap_err(), "Unsupported FITS file");
    }

    #[test]
    fn multi_hdu_fallback_exposure_and_size_check() {
        let f = raw_fits(&["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    2", "NAXIS1  =                    1", "NAXIS2  =                    1"], &[1]);
        assert_eq!(read_multi(&f, 2500).unwrap()[0].img_exp_dur, 2500);

        let a = UsImage::new(2, 2);
        let b = UsImage::new(3, 2);
        let bytes = write_multi(&[(&a, 1.0, ""), (&b, 1.0, "")]);
        assert!(read_multi(&bytes, 0).unwrap_err().contains("incompatible formats"));
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
        assert_eq!(float_text(-3.25), "-3.25");
    }
}
