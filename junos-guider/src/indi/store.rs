//! What an INDI client knows of the server's devices: their properties as
//! libindiclient keeps them, and a sequence number on every change, so a
//! waiter can tell an update that came after its request from the state it
//! saw before — the pulse completion PHD2 gets from setting `guide_active`
//! before `sendNewNumber` and clearing it in `updateProperty` only. The rule:
//! record [`PropertyStore::seq`] before sending, then wait for an
//! [`ChangeKind::Updated`] change past it (a property's
//! [`updated`](Property::updated) past it) whose state is not Busy; a
//! redefinition after a driver restart is no answer.
//!
//! The rules are libindiclient's (`WatchDeviceProperty::processXml`,
//! `BaseDevice::buildProp` / `setValue`, `delPropertyCmd`, `messageCmd`):
//! a device appears with the first def or set that names it and a property
//! (`newDevice`; libindi also creates it for one without a property name or
//! with an unknown tag, which no driver sends); a second definition of a
//! property is ignored; an update whose state is not one of the four is
//! refused, as is one for a property of another type; a `delProperty`
//! without a name removes the whole device (`removeDevice` alone, no
//! `removeProperty` per property). [`PropertyStore::sent`] is libindi's own
//! write on `sendNew*`: the property goes Busy with the values sent.
//!
//! BLOBs are decoded here (base64, then zlib when the format ends in `.z`,
//! which is stripped) and handed out with the outcome, never kept: the store
//! only records their format and size, a `size` of 0 being a state-only
//! update. Deliberate addition (PLAN): libindi's `endsWith(".z")` is
//! `find_last_of(".z", 0)`, which only matches two-character formats, so it
//! never inflates the `.<ext>.z` INDI's CCD sends when compression is on
//! (indiccd.cpp:2749); here those are inflated, and [`Blob::sent_format`]
//! keeps the format as sent for PHD2's "Unknown image format" alert.
//!
//! Follows INDI libs/indidevice/basedevice.cpp, libs/indidevice/watchdeviceproperty.cpp,
//! libs/indiabstractclient/abstractbaseclient.cpp — LGPL-2.1-or-later, see THIRD_PARTY.md

use flate2::{Decompress, FlushDecompress, Status};

use super::xml::{DefVector, Kind, Member, Message, Perm, PropState, Rule, SetVector, Update, Value, scan_int};

/// `DRIVER_INTERFACE` bits PHD2's device choosers test (`basedevice.h`).
pub const TELESCOPE_INTERFACE: u16 = 1 << 0;
pub const CCD_INTERFACE: u16 = 1 << 1;
pub const GUIDER_INTERFACE: u16 = 1 << 2;
pub const AO_INTERFACE: u16 = 1 << 8;
pub const ROTATOR_INTERFACE: u16 = 1 << 12;

/// A property and its members.
#[derive(Clone, Debug, PartialEq)]
pub struct Property {
    pub name: String,
    pub kind: Kind,
    pub label: String,
    pub group: String,
    pub state: PropState,
    pub perm: Perm,
    /// Switches only.
    pub rule: Rule,
    pub timeout: f64,
    /// The store's sequence number at this property's definition or last
    /// update.
    pub seq: u64,
    /// The sequence number of its last update (`updateProperty`), 0 before
    /// the first: what a waiter compares with the `seq` it recorded.
    pub updated: u64,
    pub members: Vec<Member>,
}

impl Property {
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        match self.member(name)?.value {
            Value::Number { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Whether the switch is On.
    pub fn switch(&self, name: &str) -> Option<bool> {
        match self.member(name)?.value {
            Value::Switch(on) => Some(on),
            _ => None,
        }
    }

    pub fn text(&self, name: &str) -> Option<&str> {
        match &self.member(name)?.value {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// A device and its properties, in the order they were defined.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub name: String,
    pub properties: Vec<Property>,
    /// [`driver_interface`](Self::driver_interface) as `DRIVER_INFO` was
    /// defined, `None` before: what PHD2's chooser tests, once, at that
    /// definition (config_indi.cpp:393), never on later updates.
    pub interface_at_definition: Option<u16>,
}

impl Device {
    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == name)
    }

    /// `BaseDevice::getDriverInterface`: `atoi` of `DRIVER_INFO.DRIVER_INTERFACE`
    /// cut to a `uint16_t`, 0 without it.
    pub fn driver_interface(&self) -> u16 {
        let text = self.property("DRIVER_INFO").filter(|p| p.kind == Kind::Text).and_then(|p| p.text("DRIVER_INTERFACE"));
        text.map_or(0, |t| scan_int(t).unwrap_or(0) as u16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Defined,
    Updated,
    Deleted,
    /// libindi's `newDevice`: the first message naming the device, even one
    /// that is then refused. `property` is empty.
    DeviceDefined,
    /// `removeDevice`: a `delProperty` without a name (a driver that died).
    /// Its properties go with it, without a `Deleted` each; `property` is
    /// empty.
    DeviceDeleted,
}

/// One property or device defined, updated or deleted.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub device: String,
    pub property: String,
    pub kind: ChangeKind,
    /// The property's state after the change (its last one when deleted);
    /// Idle for a device.
    pub state: PropState,
    pub seq: u64,
}

/// A BLOB as it arrived, decoded: base64 decoded and cut to `size`, or
/// inflated. `data` can be shorter than `size` when less was sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Blob {
    pub device: String,
    pub property: String,
    pub member: String,
    /// Without the `.z` of a compressed BLOB.
    pub format: String,
    /// The format as the driver sent it, `.z` included: what PHD2, whose
    /// libindi does not inflate, names in its "Unknown image format" alert.
    pub sent_format: String,
    pub data: Vec<u8>,
}

/// A driver message, as libindi words it: `"<timestamp>: <message> "` for a
/// known device (`BaseDevice::doMessage`), `"<timestamp>: <message>"` for the
/// others (`messageCmd`, `device` `None`); the time now when none was sent.
#[derive(Clone, Debug, PartialEq)]
pub struct DriverMessage {
    pub device: Option<String>,
    pub line: String,
}

/// What applying one message did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub changes: Vec<Change>,
    pub blobs: Vec<Blob>,
    pub message: Option<DriverMessage>,
    /// Why libindi would refuse or skip the message (most go to its
    /// "Dispatch command error" log line; a def with no member is only
    /// `IDLog`ged). For the debug log, never shown to PHD2.
    pub error: Option<String>,
}

/// The devices, in the order they appeared.
#[derive(Clone, Debug, Default)]
pub struct PropertyStore {
    devices: Vec<Device>,
    seq: u64,
}

impl PropertyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn device(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name == name)
    }

    pub fn property(&self, device: &str, name: &str) -> Option<&Property> {
        self.device(device)?.property(name)
    }

    /// The last sequence number handed out: what a waiter records before
    /// sending its request. It only grows, across deletions too.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Forgets every device, as libindi does when the server connection
    /// goes. The sequence number is kept, so a wait that recorded [`seq`]
    /// before a reconnect only sees what came after.
    ///
    /// [`seq`]: PropertyStore::seq
    pub fn clear(&mut self) {
        self.devices.clear();
    }

    /// What libindi's `sendNew*` does to its own copy of the property
    /// (abstractbaseclient.cpp:463-544): it goes Busy and holds the values
    /// sent, given in the property's member order — no change reported, no
    /// sequence number. So a look right after sending sees Busy, as PHD2's
    /// `while (Slewing())` loop does (scope_indi.cpp:880, 922).
    pub fn sent(&mut self, device: &str, name: &str, values: &[Update]) {
        let Some(p) = self.devices.iter_mut().find(|d| d.name == device).and_then(|d| d.properties.iter_mut().find(|p| p.name == name))
        else {
            return;
        };
        p.state = PropState::Busy;
        for (m, u) in p.members.iter_mut().zip(values) {
            match (&mut m.value, u) {
                (Value::Number { value, .. }, Update::Number { value: v, .. }) => *value = *v,
                (Value::Switch(on), Update::Switch(v)) => *on = *v,
                (Value::Text(t), Update::Text(v)) => t.clone_from(v),
                _ => {}
            }
        }
    }

    /// Applies a message from the server. Pings and the server's own
    /// `getProperties` change nothing: the client answers the former.
    pub fn apply(&mut self, msg: Message) -> Outcome {
        let mut out = Outcome::default();
        match msg {
            Message::Def(d) => self.define(d, &mut out),
            Message::Set(s) => self.update(s, &mut out),
            Message::DelProperty { device, name, timestamp, message } => {
                self.delete(device, name, timestamp, message, &mut out)
            }
            Message::Message { device, timestamp, message } => {
                let known = device.filter(|d| self.device(d).is_some());
                match message {
                    Some(m) => out.message = Some(driver_message(known, timestamp, &m)),
                    // A known device's message element without text is just nothing.
                    None if known.is_none() => out.error = Some("No message content found.".into()),
                    None => {}
                }
            }
            Message::PingRequest { .. } | Message::GetProperties => {}
        }
        out
    }

    /// The device's index, created on first mention (`ensureDeviceByName`,
    /// `newDevice`), even by a message that is then refused.
    fn ensure(&mut self, name: &str, out: &mut Outcome) -> usize {
        if let Some(i) = self.devices.iter().position(|d| d.name == name) {
            return i;
        }
        self.devices.push(Device { name: name.to_owned(), properties: Vec::new(), interface_at_definition: None });
        let seq = self.next_seq();
        let kind = ChangeKind::DeviceDefined;
        out.changes.push(Change { device: name.to_owned(), property: String::new(), kind, state: PropState::Idle, seq });
        self.devices.len() - 1
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn define(&mut self, d: DefVector, out: &mut Outcome) {
        // processXml ignores what names no device.
        if d.device.is_empty() {
            return;
        }
        let i = self.ensure(&d.device, out);
        // INDI_PROPERTY_DUPLICATED, silently ignored, whatever the new type:
        // a driver deletes a property before defining it again.
        if self.devices[i].property(&d.name).is_some() {
            return;
        }
        let members: Vec<Member> = d.members.into_iter().filter(|m| !m.name.is_empty()).collect();
        if members.is_empty() {
            out.error = Some(format!("{}: def{}Vector with no valid members", d.name, d.kind.tag()));
            return;
        }
        let seq = self.next_seq();
        self.devices[i].properties.push(Property {
            name: d.name.clone(),
            kind: d.kind,
            label: d.label,
            group: d.group,
            state: d.state,
            perm: d.perm,
            rule: d.rule,
            timeout: d.timeout,
            seq,
            updated: 0,
            members,
        });
        if d.name == "DRIVER_INFO" {
            let interface = self.devices[i].driver_interface();
            self.devices[i].interface_at_definition = Some(interface);
        }
        out.changes.push(Change { device: d.device, property: d.name, kind: ChangeKind::Defined, state: d.state, seq });
    }

    fn update(&mut self, s: SetVector, out: &mut Outcome) {
        if s.device.is_empty() {
            return;
        }
        let i = self.ensure(&s.device, out);
        // checkMessage comes first: the message is kept even when the update is refused.
        if let Some(m) = &s.message {
            out.message = Some(driver_message(Some(s.device.clone()), s.timestamp.clone(), m));
        }
        let Some(p) = self.devices[i].properties.iter_mut().find(|p| p.name == s.name && p.kind == s.kind) else {
            out.error = Some(format!("INDI: Could not find property {} in {}", s.name, s.device));
            return;
        };
        let Some(state) = s.state else {
            // toIPState's default lands before the refusal (basedevice.cpp:626).
            p.state = PropState::Ok;
            out.error = Some(format!("INDI: <set{}Vector> bogus state for {}", s.kind.tag(), s.name));
            return;
        };
        p.state = state;
        if s.kind != Kind::Light {
            if let Some(t) = s.timeout {
                p.timeout = t;
            }
        }
        for m in s.members {
            let member = m.name.as_deref().and_then(|n| p.members.iter_mut().find(|x| x.name == n));
            match m.update {
                Update::Blob { format, size, data } => {
                    // An error drops the whole update and the BLOBs decoded
                    // with it, but not the state set above.
                    let shown = m.name.clone().unwrap_or_default();
                    let (Some(format), Some(size), Some(name)) = (format, size, m.name) else {
                        out.error = Some(format!("INDI: {}.{}.{shown} No valid members.", s.device, s.name));
                        out.blobs.clear();
                        return;
                    };
                    // A size of 0 updates the state only; libindi would crash
                    // on a member the property lacks.
                    let Some(member) = member.filter(|_| size != 0) else { continue };
                    match blob(&format, size, &data) {
                        Ok((decoded, data)) => {
                            member.value = Value::Blob { format: decoded.clone(), size: data.len() };
                            let (device, property) = (s.device.clone(), s.name.clone());
                            out.blobs.push(Blob { device, property, member: name, format: decoded, sent_format: format, data });
                        }
                        Err(e) => {
                            out.error = Some(format!("INDI: {}.{}.{name} compression error: {e}", s.device, s.name));
                            out.blobs.clear();
                            return;
                        }
                    }
                }
                Update::Number { value: v, min: lo, max: hi } => {
                    if let Some(Member { value: Value::Number { value, min, max, .. }, .. }) = member {
                        *value = v;
                        *min = lo.unwrap_or(*min);
                        *max = hi.unwrap_or(*max);
                    }
                }
                Update::Switch(on) => {
                    if let Some(x) = member {
                        x.value = Value::Switch(on);
                    }
                }
                Update::Text(t) => {
                    if let Some(x) = member {
                        x.value = Value::Text(t);
                    }
                }
                Update::Light(l) => {
                    if let Some(x) = member {
                        x.value = Value::Light(l);
                    }
                }
            }
        }
        self.seq += 1;
        p.seq = self.seq;
        p.updated = self.seq;
        let seq = self.seq;
        out.changes.push(Change { device: s.device, property: s.name, kind: ChangeKind::Updated, state, seq });
    }

    fn delete(&mut self, device: String, name: Option<String>, timestamp: Option<String>, message: Option<String>, out: &mut Outcome) {
        let Some(i) = self.devices.iter().position(|d| d.name == device) else {
            out.error = Some(format!("Device {device} not found"));
            return;
        };
        if let Some(m) = &message {
            out.message = Some(driver_message(Some(device.clone()), timestamp, m));
        }
        let Some(name) = name else {
            self.devices.remove(i);
            let seq = self.next_seq();
            let kind = ChangeKind::DeviceDeleted;
            out.changes.push(Change { device, property: String::new(), kind, state: PropState::Idle, seq });
            return;
        };
        let Some(j) = self.devices[i].properties.iter().position(|p| p.name == name) else {
            // libindi says so only to a client with a BLOB mode set
            // (abstractbaseclient.cpp:171), here for the debug log.
            out.error = Some(format!("Cannot delete property {name} as it is not defined yet. Check driver."));
            return;
        };
        let p = self.devices[i].properties.remove(j);
        let seq = self.next_seq();
        out.changes.push(Change { device, property: p.name, kind: ChangeKind::Deleted, state: p.state, seq });
    }
}

fn driver_message(device: Option<String>, timestamp: Option<String>, message: &str) -> DriverMessage {
    let ts = timestamp.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string());
    let line = match device {
        Some(_) => format!("{ts}: {message} "),
        None => format!("{ts}: {message}"),
    };
    DriverMessage { device, line }
}

/// One BLOB's bytes and format: base64 decoded, cut to `size` (all PHD2
/// reads), or inflated into at most `size` bytes when the format ends in
/// `.z` (`uncompress`, which fails on a larger result) — a deliberate
/// addition, see the module doc.
fn blob(format: &str, size: i64, text: &[u8]) -> Result<(String, Vec<u8>), String> {
    let size = usize::try_from(size).map_err(|_| "bad size".to_string())?;
    let mut data = base64(text);
    let Some(format) = format.strip_suffix(".z") else {
        data.truncate(size);
        return Ok((format.to_owned(), data));
    };
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| "Unable to allocate memory for data buffer".to_string())?;
    let status = Decompress::new(true).decompress_vec(&data, &mut out, FlushDecompress::Finish);
    match status {
        Ok(Status::StreamEnd) if out.len() <= size => Ok((format.to_owned(), out)),
        // uncompress' codes: Z_BUF_ERROR when the result outgrows `size`,
        // Z_DATA_ERROR when the stream is cut short or corrupt.
        Ok(_) if out.len() >= size => Err("-5".into()),
        _ => Err("-3".into()),
    }
}

/// Base64 to bytes; whatever is not in the alphabet (the line breaks, the
/// padding) is skipped.
fn base64(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in text {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::engine::fits::read_fits_blob;
    use crate::engine::rng::Rng;
    use crate::indi::xml::{Element, XmlStream};

    fn b64(data: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in data.chunks(3) {
            let n = c.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
            for i in 0..4 {
                s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        s
    }

    /// A 4 × 3 16-bit FITS as the CCD Simulator writes one: `BZERO 32768`.
    fn fits() -> (Vec<u8>, Vec<u16>) {
        let px: Vec<u16> = (0..12).map(|i| i * 5000 + 7).collect();
        let cards = ["SIMPLE  =                    T", "BITPIX  =                   16", "NAXIS   =                    2"];
        let mut hdr: String = cards.iter().map(|c| format!("{c:80}")).collect();
        for c in ["NAXIS1  =                    4", "NAXIS2  =                    3", "BZERO   =                32768", "END"] {
            hdr += &format!("{c:80}");
        }
        let mut bytes = format!("{hdr:2880}").into_bytes();
        for &v in &px {
            bytes.extend_from_slice(&((v as i32 - 32768) as i16).to_be_bytes());
        }
        bytes.resize(5760, 0);
        (bytes, px)
    }

    /// A `setBLOBVector` as `IUUserIOSetBLOBVA` writes it: base64 in 72-character lines.
    fn set_blob(format: &str, size: usize, payload: &[u8], ts: &str) -> String {
        let enc = b64(payload);
        let lines: String = enc.as_bytes().chunks(72).map(|l| format!("{}\n", std::str::from_utf8(l).unwrap())).collect();
        format!(
            "<setBLOBVector\n  device='CCD Simulator'\n  name='CCD1'\n  state='Ok'\n  timeout='60'\n  timestamp='{ts}'\n>\n\
             \x20 <oneBLOB\n    name='CCD1'\n    size='{size}'\n    enclen='{}'\n    format='{format}'>\n{lines}  </oneBLOB>\n\
             </setBLOBVector>\n",
            enc.len()
        )
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data).unwrap();
        z.finish().unwrap()
    }

    /// What indiserver sends a client watching the CCD and Telescope
    /// Simulators, written the way indiuserio.c writes each element.
    fn transcript() -> String {
        let (fits, _) = fits();
        let mut t = String::from(
            "<defTextVector\n  device='CCD Simulator'\n  name='DRIVER_INFO'\n  label='Driver Info'\n  group='General Info'\n\
             \x20 state='Idle'\n  perm='ro'\n  timeout='60'\n  timestamp='2026-11-11T21:00:00'\n>\n\
             \x20 <defText\n    name='DRIVER_NAME'\n    label='Name'>\n      CCD Simulator\n  </defText>\n\
             \x20 <defText\n    name='DRIVER_INTERFACE'\n    label='Interface'>\n      22\n  </defText>\n\
             </defTextVector>\n\
             <defSwitchVector\n  device='CCD Simulator'\n  name='CONNECTION'\n  label='Connection'\n  group='Main Control'\n\
             \x20 state='Ok'\n  perm='rw'\n  rule='OneOfMany'\n  timeout='60'\n  timestamp='2026-11-11T21:00:00'\n>\n\
             \x20 <defSwitch\n    name='CONNECT'\n    label='Connect'>\n      On\n  </defSwitch>\n\
             \x20 <defSwitch\n    name='DISCONNECT'\n    label='Disconnect'>\n      Off\n  </defSwitch>\n\
             </defSwitchVector>\n\
             <defNumberVector\n  device='CCD Simulator'\n  name='CCD_EXPOSURE'\n  label='Expose'\n  group='Main Control'\n\
             \x20 state='Idle'\n  perm='rw'\n  timeout='60'\n  timestamp='2026-11-11T21:00:01'\n>\n\
             \x20 <defNumber\n    name='CCD_EXPOSURE_VALUE'\n    label='Duration (s)'\n    format='%5.2f'\n\
             \x20   min='0.01000000000000000021'\n    max='3600'\n    step='1'>\n      1\n  </defNumber>\n\
             </defNumberVector>\n\
             <defBLOBVector\n  device='CCD Simulator'\n  name='CCD1'\n  label='Image Data'\n  group='Image Info'\n\
             \x20 state='Idle'\n  perm='ro'\n  timeout='60'\n  timestamp='2026-11-11T21:00:01'\n>\n\
             \x20 <defBLOB\n    name='CCD1'\n    label='Image'\n  />\n\
             </defBLOBVector>\n",
        );
        t += &set_blob(".fits", fits.len(), &fits, "2026-11-11T21:00:02");
        t += &set_blob(".fits.z", fits.len(), &zlib(&fits), "2026-11-11T21:00:03");
        t += "<defTextVector\n  device='Telescope Simulator'\n  name='DRIVER_INFO'\n  label='Driver Info'\n  group='General Info'\n\
              \x20 state='Idle'\n  perm='ro'\n  timeout='60'\n  timestamp='2026-11-11T21:00:00'\n>\n\
              \x20 <defText\n    name='DRIVER_INTERFACE'\n    label='Interface'>\n      5\n  </defText>\n\
              </defTextVector>\n\
              <defNumberVector\n  device='Telescope Simulator'\n  name='TELESCOPE_TIMED_GUIDE_NS'\n  label='Guide N/S'\n\
              \x20 group='Guide'\n  state='Idle'\n  perm='rw'\n  timeout='60'\n  timestamp='2026-11-11T21:00:04'\n>\n\
              \x20 <defNumber\n    name='TIMED_GUIDE_N'\n    label='North (ms)'\n    format='%.f'\n    min='0'\n\
              \x20   max='60000'\n    step='100'>\n      0\n  </defNumber>\n\
              \x20 <defNumber\n    name='TIMED_GUIDE_S'\n    label='South (ms)'\n    format='%.f'\n    min='0'\n\
              \x20   max='60000'\n    step='100'>\n      0\n  </defNumber>\n\
              </defNumberVector>\n\
              <setNumberVector\n  device='Telescope Simulator'\n  name='TELESCOPE_TIMED_GUIDE_NS'\n  state='Busy'\n\
              \x20 timeout='60'\n  timestamp='2026-11-11T21:00:05'\n>\n\
              \x20 <oneNumber name='TIMED_GUIDE_N'>\n      1500\n  </oneNumber>\n\
              \x20 <oneNumber name='TIMED_GUIDE_S'>\n      0\n  </oneNumber>\n\
              </setNumberVector>\n\
              <setNumberVector\n  device='Telescope Simulator'\n  name='TELESCOPE_TIMED_GUIDE_NS'\n  state='Ok'\n\
              \x20 timeout='60'\n  timestamp='2026-11-11T21:00:06'\n>\n\
              \x20 <oneNumber name='TIMED_GUIDE_N'>\n      0\n  </oneNumber>\n\
              \x20 <oneNumber name='TIMED_GUIDE_S'>\n      0\n  </oneNumber>\n\
              </setNumberVector>\n\
              <delProperty\n  device='CCD Simulator'\n name='CCD1'\n  timestamp='2026-11-11T21:00:07'\n/>\n\
              <message\n device='Telescope Simulator'\n  timestamp='2026-11-11T21:00:08'\n\
              \x20 message='[WARNING] &lt;Guide&gt; &amp; &quot;Co&quot; &apos;s&apos;'\n/>\n\
              <pingRequest uid='1731358808' />\n";
        t
    }

    /// The elements and the store's outcomes, the stream fed in pieces of `cut()` bytes.
    fn run(bytes: &[u8], mut cut: impl FnMut() -> usize) -> (Vec<Element>, Vec<Outcome>, PropertyStore) {
        let (mut stream, mut store) = (XmlStream::new(), PropertyStore::new());
        let (mut elements, mut outcomes) = (Vec::new(), Vec::new());
        let mut rest = bytes;
        while !rest.is_empty() {
            let n = cut().clamp(1, rest.len());
            for e in stream.push(&rest[..n]) {
                outcomes.push(store.apply(Message::from_element(e.clone()).expect("a message")));
                elements.push(e);
            }
            rest = &rest[n..];
        }
        (elements, outcomes, store)
    }

    #[test]
    fn transcript_in_any_chunks() {
        let t = transcript();
        let (elements, outcomes, store) = run(t.as_bytes(), || usize::MAX);
        assert_eq!(elements.len(), 13);
        let mut rng = Rng::new(1);
        for _ in 0..100 {
            let max = 1 + rng.below(3000) as usize;
            let (e, o, _) = run(t.as_bytes(), || 1 + rng.below(max as u32) as usize);
            assert!(e == elements && o == outcomes);
        }

        let changes: Vec<_> =
            outcomes.iter().flat_map(|o| &o.changes).map(|c| (c.device.as_str(), c.property.as_str(), c.kind, c.state, c.seq)).collect();
        use ChangeKind::*;
        use PropState::{Busy, Idle, Ok};
        assert_eq!(
            changes,
            [
                ("CCD Simulator", "", DeviceDefined, Idle, 1),
                ("CCD Simulator", "DRIVER_INFO", Defined, Idle, 2),
                ("CCD Simulator", "CONNECTION", Defined, Ok, 3),
                ("CCD Simulator", "CCD_EXPOSURE", Defined, Idle, 4),
                ("CCD Simulator", "CCD1", Defined, Idle, 5),
                ("CCD Simulator", "CCD1", Updated, Ok, 6),
                ("CCD Simulator", "CCD1", Updated, Ok, 7),
                ("Telescope Simulator", "", DeviceDefined, Idle, 8),
                ("Telescope Simulator", "DRIVER_INFO", Defined, Idle, 9),
                ("Telescope Simulator", "TELESCOPE_TIMED_GUIDE_NS", Defined, Idle, 10),
                ("Telescope Simulator", "TELESCOPE_TIMED_GUIDE_NS", Updated, Busy, 11),
                ("Telescope Simulator", "TELESCOPE_TIMED_GUIDE_NS", Updated, Ok, 12),
                ("CCD Simulator", "CCD1", Deleted, Ok, 13),
            ]
        );
        assert!(outcomes.iter().all(|o| o.error.is_none()));

        // Both BLOBs come out as the same FITS, `.z` inflated and stripped.
        let (fits, px) = fits();
        let blobs: Vec<_> = outcomes.iter().flat_map(|o| &o.blobs).collect();
        assert_eq!(blobs.len(), 2);
        for b in blobs {
            assert_eq!((b.device.as_str(), b.property.as_str(), b.member.as_str(), b.format.as_str()), ("CCD Simulator", "CCD1", "CCD1", ".fits"));
            assert!(b.sent_format == ".fits" || b.sent_format == ".fits.z");
            assert_eq!(b.data, fits);
            let img = read_fits_blob(&b.data, None).unwrap();
            assert_eq!((img.w, img.h, &img.data), (4, 3, &px));
        }

        assert_eq!(
            outcomes[11].message,
            Some(DriverMessage {
                device: Some("Telescope Simulator".into()),
                line: "2026-11-11T21:00:08: [WARNING] <Guide> & \"Co\" 's' ".into(),
            })
        );

        // The store as it stands: the values, the BLOB gone, the interfaces.
        let ccd = store.device("CCD Simulator").unwrap();
        let names: Vec<_> = ccd.properties.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["DRIVER_INFO", "CONNECTION", "CCD_EXPOSURE"]);
        assert_eq!(ccd.property("CONNECTION").unwrap().switch("CONNECT"), Some(true));
        assert_eq!(ccd.property("CONNECTION").unwrap().rule, Rule::OneOfMany);
        let exp = ccd.property("CCD_EXPOSURE").unwrap();
        assert_eq!(
            exp.member("CCD_EXPOSURE_VALUE").unwrap().value,
            Value::Number { value: 1.0, format: "%5.2f".into(), min: 0.01, max: 3600.0, step: 1.0 }
        );
        assert_eq!((exp.perm, exp.timeout, exp.label.as_str(), exp.group.as_str()), (Perm::Rw, 60.0, "Expose", "Main Control"));
        let guide = store.property("Telescope Simulator", "TELESCOPE_TIMED_GUIDE_NS").unwrap();
        assert_eq!((guide.state, guide.seq, guide.updated, guide.number("TIMED_GUIDE_N")), (Ok, 12, 12, Some(0.0)));
        assert_eq!(store.seq(), 13);

        let ifs: Vec<_> = store.devices().iter().map(|d| (d.name.as_str(), d.driver_interface())).collect();
        assert_eq!(ifs, [("CCD Simulator", 22), ("Telescope Simulator", 5)]);
        // PHD2's choosers (config_indi.cpp:395): camera = CCD, mount = GUIDER and not CCD.
        let camera = |i: u16| i & CCD_INTERFACE != 0;
        let mount = |i: u16| i & CCD_INTERFACE == 0 && i & GUIDER_INTERFACE != 0;
        assert!(camera(22) && !mount(22) && mount(5) && !camera(5) && 5 & TELESCOPE_INTERFACE != 0);
    }

    fn apply(store: &mut PropertyStore, xml: &str) -> Outcome {
        let e = XmlStream::new().push(xml.as_bytes()).remove(0);
        store.apply(Message::from_element(e).unwrap())
    }

    const GUIDE_NS: &str = "<defNumberVector device='M' name='TELESCOPE_TIMED_GUIDE_NS' state='Idle' perm='rw'>\
        <defNumber name='TIMED_GUIDE_N' min='0' max='60000'>0</defNumber><defNumber name='TIMED_GUIDE_S'>0</defNumber>\
        </defNumberVector>";

    fn guide(state: &str, n: f64) -> String {
        format!(
            "<setNumberVector device='M' name='TELESCOPE_TIMED_GUIDE_NS' state='{state}'>\
             <oneNumber name='TIMED_GUIDE_N'>{n}</oneNumber></setNumberVector>"
        )
    }

    #[test]
    fn seq_tells_a_pulse_completion_from_an_old_state() {
        let mut store = PropertyStore::new();
        apply(&mut store, GUIDE_NS);
        apply(&mut store, &guide("Ok", 0.0));
        // The axis already reads Ok before the pulse: only a later update counts.
        let seq0 = store.seq();
        let done =
            |o: &Outcome| o.changes.iter().any(|c| c.kind == ChangeKind::Updated && c.seq > seq0 && c.state != PropState::Busy);
        // libindi's own write on sendNewNumber: Busy at once, no change, no number.
        store.sent("M", "TELESCOPE_TIMED_GUIDE_NS", &[Update::Number { value: 1500.0, min: None, max: None }]);
        let p = store.property("M", "TELESCOPE_TIMED_GUIDE_NS").unwrap();
        assert_eq!((p.state, p.number("TIMED_GUIDE_N"), p.updated, store.seq()), (PropState::Busy, Some(1500.0), 3, seq0));
        assert!(!done(&apply(&mut store, &guide("Busy", 1500.0))));
        let o = apply(&mut store, &guide("Ok", 0.0));
        assert!(done(&o));
        let p = store.property("M", "TELESCOPE_TIMED_GUIDE_NS").unwrap();
        assert_eq!((o.changes[0].seq, p.seq, p.updated), (5, 5, 5));
        // Deleting and defining again keeps the numbers growing, and a
        // redefinition in state Idle is no answer to a waiter.
        let seq0 = store.seq();
        let o = apply(&mut store, "<delProperty device='M' name='TELESCOPE_TIMED_GUIDE_NS'/>");
        assert_eq!((o.changes[0].kind, o.changes[0].seq), (ChangeKind::Deleted, 6));
        let o = apply(&mut store, GUIDE_NS);
        assert!(!done(&o));
        let p = store.property("M", "TELESCOPE_TIMED_GUIDE_NS").unwrap();
        assert_eq!((o.changes[0].seq, p.seq, p.updated > seq0), (7, 7, false));
        assert_eq!(store.seq(), 7);
    }

    #[test]
    fn what_libindi_refuses_or_ignores() {
        let mut store = PropertyStore::new();
        apply(&mut store, GUIDE_NS);
        // A second definition is ignored, silently.
        let o = apply(&mut store, "<defSwitchVector device='M' name='TELESCOPE_TIMED_GUIDE_NS'><defSwitch name='X'>On</defSwitch></defSwitchVector>");
        assert_eq!(o, Outcome::default());
        assert_eq!(store.property("M", "TELESCOPE_TIMED_GUIDE_NS").unwrap().kind, Kind::Number);
        // A bad state refuses the update, but the message is kept and the
        // state reads Ok, toIPState's default.
        apply(&mut store, &guide("Busy", 0.0));
        let o = apply(
            &mut store,
            "<setNumberVector device='M' name='TELESCOPE_TIMED_GUIDE_NS' state='Bad' timestamp='T' message='hi'>\
             <oneNumber name='TIMED_GUIDE_N'>9</oneNumber></setNumberVector>",
        );
        assert!(o.changes.is_empty() && o.error.is_some());
        assert_eq!(o.message, Some(DriverMessage { device: Some("M".into()), line: "T: hi ".into() }));
        let p = store.property("M", "TELESCOPE_TIMED_GUIDE_NS").unwrap();
        assert_eq!((p.number("TIMED_GUIDE_N"), p.state), (Some(0.0), PropState::Ok));
        // So does another type, and an unknown property; the device appears anyway.
        let o = apply(&mut store, "<setSwitchVector device='M' name='TELESCOPE_TIMED_GUIDE_NS' state='Ok'/>");
        assert_eq!(o.error.as_deref(), Some("INDI: Could not find property TELESCOPE_TIMED_GUIDE_NS in M"));
        let o = apply(&mut store, "<setTextVector device='Ghost' name='X' state='Ok'/>");
        assert!(store.device("Ghost").is_some_and(|d| d.properties.is_empty()));
        assert_eq!(o.changes.iter().map(|c| (c.device.as_str(), c.kind)).collect::<Vec<_>>(), [("Ghost", ChangeKind::DeviceDefined)]);
        // Members need a name; a vector with none is not defined.
        let o = apply(&mut store, "<defTextVector device='M' name='T'><defText label='x'>a</defText></defTextVector>");
        assert_eq!(o.error.as_deref(), Some("T: defTextVector with no valid members"));
        assert!(store.property("M", "T").is_none());
        // Min and max change only when sent; unknown members are skipped.
        apply(
            &mut store,
            "<setNumberVector device='M' name='TELESCOPE_TIMED_GUIDE_NS' state='Ok' timeout='5'>\
             <oneNumber name='TIMED_GUIDE_N' max='100'>3</oneNumber><oneNumber name='NOPE'>1</oneNumber></setNumberVector>",
        );
        let p = store.property("M", "TELESCOPE_TIMED_GUIDE_NS").unwrap();
        assert_eq!(p.timeout, 5.0);
        assert!(matches!(p.member("TIMED_GUIDE_N").unwrap().value, Value::Number { value: 3.0, min: 0.0, max: 100.0, .. }));
        // delProperty: an unknown property, then the whole device.
        let o = apply(&mut store, "<delProperty device='M' name='NOPE'/>");
        assert_eq!(o.error.as_deref(), Some("Cannot delete property NOPE as it is not defined yet. Check driver."));
        apply(&mut store, "<defLightVector device='M' name='L'><defLight name='A'>Busy</defLight></defLightVector>");
        // The whole device: libindi's removeDevice alone, also for a device
        // with no property left.
        let o = apply(&mut store, "<delProperty device='M'/>");
        let gone: Vec<_> = o.changes.iter().map(|c| (c.device.as_str(), c.property.as_str(), c.kind)).collect();
        assert_eq!(gone, [("M", "", ChangeKind::DeviceDeleted)]);
        assert!(store.device("M").is_none());
        let o = apply(&mut store, "<delProperty device='Ghost'/>");
        assert_eq!((o.changes.len(), o.changes[0].kind), (1, ChangeKind::DeviceDeleted));
        // A def's message is never read (buildProp has no checkMessage).
        let o = apply(&mut store, "<defTextVector device='M' name='T2' message='hi'><defText name='A'>a</defText></defTextVector>");
        assert_eq!((o.message, o.changes.len()), (None, 2));
        // A message for no known device has no trailing space.
        let o = apply(&mut store, "<message device='Nobody' timestamp='T' message='x'/>");
        assert_eq!(o.message, Some(DriverMessage { device: None, line: "T: x".into() }));
        assert_eq!(apply(&mut store, "<message timestamp='T'/>").error.as_deref(), Some("No message content found."));
    }

    #[test]
    fn blobs_state_only_cut_and_broken() {
        let mut store = PropertyStore::new();
        apply(&mut store, "<defBLOBVector device='C' name='CCD1'><defBLOB name='CCD1' label='Image'/></defBLOBVector>");
        let blob = |size: &str, format: &str, data: &str| {
            format!(
                "<setBLOBVector device='C' name='CCD1' state='Ok'><oneBLOB name='CCD1' {size} {format}>{data}</oneBLOB></setBLOBVector>"
            )
        };
        // size 0: a state change only.
        let o = apply(&mut store, &blob("size='0'", "format='.fits'", ""));
        assert_eq!((o.changes.len(), o.blobs.len()), (1, 0));
        // Decoded bytes past `size` are dropped; padding and breaks are skipped.
        let o = apply(&mut store, &blob("size='2'", "format='.raw'", "QU\nJD"));
        assert_eq!((o.blobs[0].data.as_slice(), o.blobs[0].format.as_str()), (&b"AB"[..], ".raw"));
        assert_eq!(apply(&mut store, &blob("size='5'", "format='.raw'", "QUJDRA==")).blobs[0].data, b"ABCD");
        assert_eq!(
            store.property("C", "CCD1").unwrap().member("CCD1").unwrap().value,
            Value::Blob { format: ".raw".into(), size: 4 }
        );
        // A missing attribute refuses the update.
        let o = apply(&mut store, &blob("size='4'", "", "QUJDRA=="));
        assert_eq!((o.changes.len(), o.blobs.len(), o.error.as_deref()), (0, 0, Some("INDI: C.CCD1.CCD1 No valid members.")));
        // A `.z` BLOB must inflate within `size`: Z_BUF_ERROR, Z_DATA_ERROR.
        let z = b64(&zlib(b"hello world"));
        let o = apply(&mut store, &blob("size='5'", "format='.fits.z'", &z));
        assert_eq!(o.error.as_deref(), Some("INDI: C.CCD1.CCD1 compression error: -5"));
        let o = apply(&mut store, &blob("size='50'", "format='.fits.z'", &z[..z.len() - 8]));
        assert_eq!(o.error.as_deref(), Some("INDI: C.CCD1.CCD1 compression error: -3"));
        let o = apply(&mut store, &blob("size='11'", "format='.fits.z'", &z));
        assert_eq!((o.blobs[0].data.as_slice(), o.blobs[0].format.as_str()), (&b"hello world"[..], ".fits"));
    }

    #[test]
    fn driver_interface_is_atoi_cut_to_16_bits() {
        let mut store = PropertyStore::new();
        let info = |dev: &str, kind: &str, v: &str| {
            format!("<def{kind}Vector device='{dev}' name='DRIVER_INFO'><def{kind} name='DRIVER_INTERFACE'>{v}</def{kind}></def{kind}Vector>")
        };
        apply(&mut store, &info("A", "Text", "6abc"));
        apply(&mut store, &info("B", "Text", "70000"));
        apply(&mut store, &info("C", "Number", "5"));
        apply(&mut store, &info("D", "Text", "x"));
        apply(&mut store, "<defTextVector device='E' name='OTHER'><defText name='A'>5</defText></defTextVector>");
        let ifs: Vec<_> = store.devices().iter().map(|d| d.driver_interface()).collect();
        assert_eq!(ifs, [6, 4464, 0, 0, 0]); // 70000 - 65536
        // The chooser's value is the one DRIVER_INFO was defined with, and
        // none without it.
        let at_def: Vec<_> = store.devices().iter().map(|d| d.interface_at_definition).collect();
        assert_eq!(at_def, [Some(6), Some(4464), Some(0), Some(0), None]);
        apply(&mut store, "<setTextVector device='A' name='DRIVER_INFO' state='Ok'><oneText name='DRIVER_INTERFACE'>1</oneText></setTextVector>");
        let a = store.device("A").unwrap();
        assert_eq!((a.driver_interface(), a.interface_at_definition), (1, Some(6)));
        assert_eq!(AO_INTERFACE | ROTATOR_INTERFACE, 0x1100);
    }
}
