//! INDI's XML wire: the incremental tokenizer for what `indiserver` sends,
//! the typed messages a client acts on, and the elements a client writes.
//!
//! The tokenizer is lilxml's state machine (`parseXMLChunk`, `oneXMLchar`):
//! top-level elements come out whole however the bytes were split; `<?…>` and
//! `<!…>` are skipped up to the next `>`; an element's text loses its leading
//! and trailing whitespace; a malformed element is dropped and parsing picks
//! up again at the next `<`. Two departures, both what any XML reader does and
//! neither reachable with what drivers write: an entity at the start of an
//! element's text is decoded (lilxml copies it raw), and so are numeric
//! character references (lilxml only knows the five named entities; any other
//! entity is kept verbatim, here too).
//!
//! A [`Message`] holds what libindiclient reads from each element
//! (`BaseDevice::buildProp`, `BaseDevice::setValue`, `dispatchCommand`),
//! parsed the way it parses: a number in a `def*` element through
//! `f_scansexa`, in a `set*` element through `std::stod`; an unknown state,
//! permission or rule falls back to libindi's default.
//!
//! Follows INDI libs/indicore/lilxml.cpp, indicom.c, indidevapi.c, indiuserio.c,
//! userio.c, libs/indidevice/basedevice.cpp,
//! libs/indiabstractclient/abstractbaseclient.cpp — LGPL-2.1-or-later, see THIRD_PARTY.md

use crate::engine::log::cfmt::fmt_gp;

// ---------------------------------------------------------------- tokenizer

/// One XML element and everything inside it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// The character data, entities decoded, outer whitespace dropped. Bytes:
    /// a `oneBLOB` carries megabytes of base64, decoded later.
    pub text: Vec<u8>,
}

impl Element {
    /// The first attribute of that name (`findXMLAtt`).
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The text as a string, invalid UTF-8 replaced.
    pub fn text_str(&self) -> String {
        String::from_utf8_lossy(&self.text).into_owned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Look4Start,
    Look4Tag,
    InTag,
    Look4AttrN,
    InAttrN,
    Look4AttrV,
    SawSlash,
    InAttrV,
    EntInAttrV,
    Look4Con,
    InCon,
    EntInCon,
    SawLtInCon,
    Look4CloseTag,
    InCloseTag,
}

/// lilxml's parser over a byte stream: feed it whatever the socket read.
#[derive(Debug)]
pub struct XmlStream {
    state: State,
    /// The element being read, under its open ancestors.
    stack: Vec<Element>,
    /// The attribute value being read.
    value: Vec<u8>,
    /// An entity being read, from its `&`.
    entity: Vec<u8>,
    end_tag: String,
    delim: u8,
    /// lilxml's `lastc`: a `<` waits for the next byte, which may make it a
    /// `<?`/`<!` to skip.
    last: u8,
    skipping: bool,
    /// Why a malformed element was dropped during the last [`push`](Self::push)
    /// (lilxml's `ynot`, cleared at each `parseXMLChunk`).
    pub last_error: Option<String>,
}

impl Default for XmlStream {
    fn default() -> Self {
        Self::new()
    }
}

impl XmlStream {
    pub fn new() -> Self {
        Self {
            state: State::Look4Start,
            stack: Vec::new(),
            value: Vec::new(),
            entity: Vec::new(),
            end_tag: String::new(),
            delim: 0,
            last: 0,
            skipping: false,
            last_error: None,
        }
    }

    /// Reads more of the stream; returns the top-level elements it completed.
    pub fn push(&mut self, data: &[u8]) -> Vec<Element> {
        self.last_error = None;
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            // Text runs go in whole: a BLOB is megabytes between two tags.
            if self.state == State::InCon && self.last != b'<' && !self.skipping {
                let rest = &data[i..];
                let n = rest.iter().position(|&c| matches!(c, b'<' | b'&' | 0)).unwrap_or(rest.len());
                if n > 0 {
                    self.top().text.extend_from_slice(&rest[..n]);
                    self.last = rest[n - 1];
                    i += n;
                    continue;
                }
            }
            let c = data[i];
            i += 1;
            if c == 0 {
                self.fail("early XML EOF".into());
                continue;
            }
            if !self.skipping && self.last == b'<' && (c == b'?' || c == b'!') {
                self.skipping = true;
                self.last = c;
                continue;
            }
            if self.skipping {
                if c == b'>' {
                    self.skipping = false;
                }
                self.last = c;
                continue;
            }
            if c == b'<' {
                // lilxml drops a `<` that another `<` follows.
                self.last = b'<';
                continue;
            }
            if self.last == b'<' {
                // A `<` never closes an element.
                if let Err(e) = self.one(b'<') {
                    self.fail(e);
                    continue;
                }
            }
            match self.one(c) {
                Ok(false) => self.last = c,
                Ok(true) => {
                    out.extend(self.stack.pop());
                    self.reset();
                }
                Err(e) => self.fail(e),
            }
        }
        out
    }

    fn top(&mut self) -> &mut Element {
        // Every state past Look4Start has an element open.
        self.stack.last_mut().expect("an open element")
    }

    fn reset(&mut self) {
        let last_error = self.last_error.take();
        *self = Self::new();
        self.last_error = last_error;
    }

    fn fail(&mut self, why: String) {
        self.reset();
        self.last_error = Some(why);
    }

    /// `oneXMLchar`: one byte; `Ok(true)` when it closed the top-level element.
    fn one(&mut self, c: u8) -> Result<bool, String> {
        use State::*;
        match self.state {
            Look4Start => {
                if c == b'<' {
                    self.stack.push(Element::default());
                    self.state = Look4Tag;
                }
            }
            Look4Tag => {
                if is_token(true, c) {
                    self.top().name.push(c as char);
                    self.state = InTag;
                } else if !is_space(c) {
                    return Err(format!("Bogus tag char {}", c as char));
                }
            }
            InTag => {
                if is_token(false, c) {
                    self.top().name.push(c as char);
                } else if c == b'>' {
                    self.state = Look4Con;
                } else if c == b'/' {
                    self.state = SawSlash;
                } else {
                    self.state = Look4AttrN;
                }
            }
            Look4AttrN => {
                if c == b'>' {
                    self.state = Look4Con;
                } else if c == b'/' {
                    self.state = SawSlash;
                } else if is_token(true, c) {
                    self.top().attrs.push(((c as char).to_string(), String::new()));
                    self.state = InAttrN;
                } else if !is_space(c) {
                    return Err(format!("Bogus leading attr name char: {}", c as char));
                }
            }
            SawSlash => {
                if c != b'>' {
                    return Err(format!("Bogus char {} before >", c as char));
                }
                if self.stack.len() == 1 {
                    return Ok(true);
                }
                self.pop();
            }
            InAttrN => {
                if is_token(false, c) {
                    self.last_attr().0.push(c as char);
                } else if is_space(c) || c == b'=' {
                    self.state = Look4AttrV;
                } else {
                    return Err(format!("Bogus attr name char: {}", c as char));
                }
            }
            Look4AttrV => {
                if c == b'\'' || c == b'"' {
                    self.delim = c;
                    self.value.clear();
                    self.state = InAttrV;
                } else if !(is_space(c) || c == b'=') {
                    return Err(format!("No value for attribute {}", self.last_attr().0));
                }
            }
            InAttrV => {
                if c == b'&' {
                    self.entity = vec![c];
                    self.state = EntInAttrV;
                } else if c == self.delim {
                    let value = String::from_utf8_lossy(&self.value).into_owned();
                    self.last_attr().1 = value;
                    self.state = Look4AttrN;
                } else if !(c < 0x20 || c == 0x7f) {
                    // iscntrl: lilxml drops control characters from values.
                    self.value.push(c);
                }
            }
            EntInAttrV => {
                self.entity.push(c);
                if c == b';' {
                    decode_entity(&self.entity, &mut self.value);
                    self.state = InAttrV;
                }
            }
            Look4Con => {
                if c == b'<' {
                    self.state = SawLtInCon;
                } else if c == b'&' {
                    self.entity = vec![c];
                    self.state = EntInCon;
                } else if !is_space(c) {
                    self.top().text.push(c);
                    self.state = InCon;
                }
            }
            InCon => {
                if c == b'&' {
                    self.entity = vec![c];
                    self.state = EntInCon;
                } else if c == b'<' {
                    let text = &mut self.top().text;
                    while text.last().is_some_and(|&c| is_space(c)) {
                        text.pop();
                    }
                    self.state = SawLtInCon;
                } else {
                    self.top().text.push(c);
                }
            }
            EntInCon => {
                self.entity.push(c);
                if c == b';' {
                    let entity = std::mem::take(&mut self.entity);
                    decode_entity(&entity, &mut self.top().text);
                    self.state = InCon;
                }
            }
            SawLtInCon => {
                if c == b'/' {
                    self.end_tag.clear();
                    self.state = Look4CloseTag;
                } else {
                    self.stack.push(Element::default());
                    if is_token(true, c) {
                        self.top().name.push(c as char);
                        self.state = InTag;
                    } else {
                        self.state = Look4Tag;
                    }
                }
            }
            Look4CloseTag => {
                if is_token(true, c) {
                    self.end_tag.push(c as char);
                    self.state = InCloseTag;
                } else if !is_space(c) {
                    return Err(format!("Bogus preend tag char {}", c as char));
                }
            }
            InCloseTag => {
                if is_token(false, c) {
                    self.end_tag.push(c as char);
                } else if c == b'>' {
                    let name = &self.stack[self.stack.len() - 1].name;
                    if *name != self.end_tag {
                        return Err(format!("closing tag {} does not match {}", self.end_tag, name));
                    }
                    if self.stack.len() == 1 {
                        return Ok(true);
                    }
                    self.pop();
                } else if !is_space(c) {
                    return Err(format!("Bogus end tag char {}", c as char));
                }
            }
        }
        Ok(false)
    }

    /// Back to the parent's content once a child element is closed.
    fn pop(&mut self) {
        let child = self.stack.pop().expect("an open element");
        self.top().children.push(child);
        self.state = State::Look4Con;
    }

    fn last_attr(&mut self) -> &mut (String, String) {
        self.top().attrs.last_mut().expect("an attribute being read")
    }
}

/// C's `isspace` in the C locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// lilxml's `isTokenChar`: a letter or `_`, digits after the first.
fn is_token(start: bool, c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || (!start && c.is_ascii_digit())
}

/// Appends what `&…;` stands for, or the entity itself when unknown.
fn decode_entity(entity: &[u8], out: &mut Vec<u8>) {
    let c = match entity {
        b"&amp;" => Some('&'),
        b"&apos;" => Some('\''),
        b"&lt;" => Some('<'),
        b"&gt;" => Some('>'),
        b"&quot;" => Some('"'),
        _ => entity
            .strip_prefix(b"&#")
            .and_then(|e| e.strip_suffix(b";"))
            .and_then(|e| std::str::from_utf8(e).ok())
            .and_then(|e| match e.strip_prefix('x') {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => e.parse().ok(),
            })
            .and_then(char::from_u32),
    };
    match c {
        Some(c) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        None => out.extend_from_slice(entity),
    }
}

// ---------------------------------------------------------------- numbers

/// The longest prefix of `s` `strtod` (`scanf`: `%lf`) reads, and its length.
/// `scanf` fails on an `e` no digit follows, where `strtod` stops before it.
/// Hexadecimal floats are not read: no driver prints them.
fn scan_f64(s: &[u8], scanf: bool) -> Option<(f64, usize)> {
    let digits = |i: usize| s[i.min(s.len())..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut i = s.iter().take_while(|&&c| is_space(c)).count();
    let start = i;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    let word = ["infinity", "inf", "nan"]
        .into_iter()
        .find(|w| s.len() >= i + w.len() && s[i..i + w.len()].eq_ignore_ascii_case(w.as_bytes()));
    if let Some(w) = word {
        i += w.len();
    } else {
        let int = digits(i);
        i += int;
        let mut frac = 0;
        if s.get(i) == Some(&b'.') {
            frac = digits(i + 1);
            if int + frac > 0 {
                i += 1 + frac;
            }
        }
        if int + frac == 0 {
            return None;
        }
        if matches!(s.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(s.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            match digits(j) {
                0 if scanf => return None,
                0 => {}
                n => i = j + n,
            }
        }
    }
    let v = std::str::from_utf8(&s[start..i]).ok()?.parse().ok()?;
    Some((v, i))
}

/// `std::stod` as libindi's `LilXmlValue::toDouble` uses it: the leading
/// number, `None` when there is none.
pub fn scan_double(s: &str) -> Option<f64> {
    scan_f64(s.as_bytes(), false).map(|(v, _)| v)
}

/// `f_scansexa`: `D:M:S`, `D M S`, `D:M`, or a plain number. Spaces are
/// removed first, so `"12 30 45"` reads 123045; a leading `-` negates the
/// whole value; any run of non-digits separates the fields, so a sign after
/// the first is lost (`"12:-30"` is 12.5).
pub fn scan_sexa(s: &str) -> Option<f64> {
    // strncpy into a 128-byte buffer, then the spaces out.
    let mut b: Vec<u8> = s.as_bytes().iter().take(127).copied().filter(|&c| c != b' ').collect();
    let negative = b.first() == Some(&b'-');
    if negative {
        b[0] = b' ';
    }
    // sscanf(str, "%lf%*[^0-9]%lf%*[^0-9]%lf")
    let (a, mut i) = scan_f64(&b, true)?;
    let mut f = [a, 0.0, 0.0];
    for x in &mut f[1..] {
        let skip = b[i..].iter().take_while(|c| !c.is_ascii_digit()).count();
        if skip == 0 {
            break;
        }
        let Some((v, n)) = scan_f64(&b[i + skip..], true) else { break };
        *x = v;
        i += skip + n;
    }
    let v = f[0] + f[1] / 60.0 + f[2] / 3600.0;
    Some(if negative { -v } else { v })
}

/// `std::stoi` / `atoi`: the leading integer.
pub fn scan_int(s: &str) -> Option<i64> {
    let s = s.trim_start_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let n = rest.bytes().take_while(u8::is_ascii_digit).count();
    let v: i64 = rest[..n].parse().ok()?;
    Some(sign * v)
}

// ---------------------------------------------------------------- messages

/// A property's state (`IPState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropState {
    Idle,
    Ok,
    Busy,
    Alert,
}

impl PropState {
    /// `crackIPState`: `Ok` is matched on its first two letters only.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Idle" => Some(Self::Idle),
            "Busy" => Some(Self::Busy),
            "Alert" => Some(Self::Alert),
            _ if s.starts_with("Ok") => Some(Self::Ok),
            _ => None,
        }
    }
}

/// A property's permission (`IPerm`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    Ro,
    Wo,
    Rw,
}

impl Perm {
    /// `crackIPerm`: the first two letters.
    pub fn parse(s: &str) -> Option<Self> {
        match s.get(..2) {
            Some("rw") => Some(Self::Rw),
            Some("ro") => Some(Self::Ro),
            Some("wo") => Some(Self::Wo),
            _ => None,
        }
    }
}

/// A switch vector's rule (`ISRule`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    OneOfMany,
    AtMostOne,
    AnyOfMany,
}

impl Rule {
    /// `crackISRule`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "OneOfMany" => Some(Self::OneOfMany),
            "AtMostOne" => Some(Self::AtMostOne),
            "AnyOfMany" => Some(Self::AnyOfMany),
            _ => None,
        }
    }
}

/// The five property types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Number,
    Switch,
    Text,
    Light,
    Blob,
}

impl Kind {
    /// The type as it sits in a tag: `defBLOBVector`, `oneNumber`.
    fn from_tag(s: &str) -> Option<Self> {
        match s {
            "Number" => Some(Self::Number),
            "Switch" => Some(Self::Switch),
            "Text" => Some(Self::Text),
            "Light" => Some(Self::Light),
            "BLOB" => Some(Self::Blob),
            _ => None,
        }
    }

    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Number => "Number",
            Self::Switch => "Switch",
            Self::Text => "Text",
            Self::Light => "Light",
            Self::Blob => "BLOB",
        }
    }
}

/// What `enableBLOB` asks for a device or a BLOB property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobMode {
    Never,
    Also,
    Only,
}

/// A member's value as a property holds it.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number { value: f64, format: String, min: f64, max: f64, step: f64 },
    /// On.
    Switch(bool),
    Text(String),
    Light(PropState),
    /// The last BLOB's format and size; the bytes are never kept.
    Blob { format: String, size: usize },
}

/// A member of a property (an INDI "element" or "widget").
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub name: String,
    pub label: String,
    pub value: Value,
}

/// A `def*Vector`: a new property.
#[derive(Clone, Debug, PartialEq)]
pub struct DefVector {
    pub kind: Kind,
    pub device: String,
    pub name: String,
    pub label: String,
    pub group: String,
    pub state: PropState,
    pub perm: Perm,
    /// Switches only.
    pub rule: Rule,
    pub timeout: f64,
    pub members: Vec<Member>,
}

/// One member of a `set*Vector`.
#[derive(Clone, Debug, PartialEq)]
pub struct SetMember {
    /// `None` without the attribute: libindi tests its presence, so an
    /// empty name is a name.
    pub name: Option<String>,
    pub update: Update,
}

/// A member's new value. A missing attribute is `None` where libindi tells a
/// missing attribute from a present one.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    Number { value: f64, min: Option<f64>, max: Option<f64> },
    Switch(bool),
    Text(String),
    Light(PropState),
    /// `data` is the base64 text as received. `size` reads like `stoi`: 0
    /// when it does not parse or overflows an `int`. (`enclen` only serves
    /// lilxml's optional `WITH_ENCLEN` buffering, so it is not read.)
    Blob { format: Option<String>, size: Option<i64>, data: Vec<u8> },
}

/// A `set*Vector`: new values for a defined property.
#[derive(Clone, Debug, PartialEq)]
pub struct SetVector {
    pub kind: Kind,
    pub device: String,
    pub name: String,
    /// `None` when missing or unknown: libindi then rejects the update.
    pub state: Option<PropState>,
    /// Applied only when present and numeric.
    pub timeout: Option<f64>,
    pub timestamp: Option<String>,
    pub message: Option<String>,
    pub members: Vec<SetMember>,
}

/// A message from `indiserver`, as libindiclient tells them apart.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Def(DefVector),
    Set(SetVector),
    /// No `name`: the whole device goes.
    DelProperty { device: String, name: Option<String>, timestamp: Option<String>, message: Option<String> },
    Message { device: Option<String>, timestamp: Option<String>, message: Option<String> },
    PingRequest { uid: String },
    /// A server's own `getProperties`, which a client ignores.
    GetProperties,
}

impl Message {
    /// The message a top-level element carries; `None` for what a client
    /// ignores (echoed `new*Vector`s, `pingReply`, unknown tags) and for a
    /// `def*`/`set*` without the `device` or `name` libindi requires. Taken
    /// by value: a BLOB's base64 text moves into the message uncopied.
    pub fn from_element(mut e: Element) -> Option<Message> {
        let attr = |name: &str| e.attr(name).map(str::to_owned);
        match e.name.as_str() {
            "delProperty" => Some(Message::DelProperty {
                device: attr("device").unwrap_or_default(),
                name: attr("name"),
                timestamp: attr("timestamp"),
                message: attr("message"),
            }),
            "message" => Some(Message::Message {
                device: attr("device"),
                timestamp: attr("timestamp"),
                message: attr("message"),
            }),
            "pingRequest" => Some(Message::PingRequest { uid: attr("uid").unwrap_or_default() }),
            "getProperties" => Some(Message::GetProperties),
            tag => {
                let vector = |prefix: &str| tag.strip_prefix(prefix)?.strip_suffix("Vector").and_then(Kind::from_tag);
                if let Some(kind) = vector("def") {
                    Some(Message::Def(def_vector(&e, kind)?))
                } else if let Some(kind) = vector("set") {
                    Some(Message::Set(set_vector(&mut e, kind)?))
                } else {
                    None
                }
            }
        }
    }
}

/// `BaseDevice::buildProp`: members are the `def<Type>` children; numbers'
/// `min`/`max`/`step` and the timeout read through `std::stod` (0 when they
/// do not parse), the value through `f_scansexa` (0 likewise).
fn def_vector(e: &Element, kind: Kind) -> Option<DefVector> {
    let attr = |name: &str| e.attr(name).unwrap_or_default();
    let stod = |s: Option<&str>| scan_double(s.unwrap_or_default()).unwrap_or(0.0);
    let tag = format!("def{}", kind.tag());
    let members = e
        .children
        .iter()
        .filter(|m| m.name == tag)
        .map(|m| {
            let text = m.text_str();
            let value = match kind {
                Kind::Number => Value::Number {
                    value: scan_sexa(&text).unwrap_or(0.0),
                    format: m.attr("format").unwrap_or_default().to_owned(),
                    min: stod(m.attr("min")),
                    max: stod(m.attr("max")),
                    step: stod(m.attr("step")),
                },
                Kind::Switch => Value::Switch(switch_on(&text)),
                Kind::Text => Value::Text(text),
                Kind::Light => Value::Light(PropState::parse(&text).unwrap_or(PropState::Ok)),
                Kind::Blob => Value::Blob { format: m.attr("format").unwrap_or_default().to_owned(), size: 0 },
            };
            Member {
                name: m.attr("name").unwrap_or_default().to_owned(),
                label: m.attr("label").unwrap_or_default().to_owned(),
                value,
            }
        })
        .collect();
    Some(DefVector {
        kind,
        device: e.attr("device")?.to_owned(),
        name: e.attr("name")?.to_owned(),
        label: attr("label").to_owned(),
        group: attr("group").to_owned(),
        state: PropState::parse(attr("state")).unwrap_or(PropState::Ok),
        perm: Perm::parse(attr("perm")).unwrap_or(Perm::Ro),
        rule: Rule::parse(attr("rule")).unwrap_or(Rule::OneOfMany),
        timeout: stod(e.attr("timeout")),
        // buildProp never reads a def's `message` (no checkMessage).
        members,
    })
}

/// `BaseDevice::setValue`: every child is a member (only `oneBLOB` ones for
/// a BLOB vector), whatever its tag; numbers read through `std::stod`.
fn set_vector(e: &mut Element, kind: Kind) -> Option<SetVector> {
    let stod = |s: &str| scan_double(s).unwrap_or(0.0);
    let members = e
        .children
        .iter_mut()
        .filter(|m| kind != Kind::Blob || m.name == "oneBLOB")
        .map(|m| {
            let update = match kind {
                Kind::Number => Update::Number {
                    value: stod(&m.text_str()),
                    min: m.attr("min").map(stod),
                    max: m.attr("max").map(stod),
                },
                Kind::Switch => Update::Switch(switch_on(&m.text_str())),
                Kind::Text => Update::Text(m.text_str()),
                Kind::Light => Update::Light(PropState::parse(&m.text_str()).unwrap_or(PropState::Ok)),
                Kind::Blob => Update::Blob {
                    format: m.attr("format").map(str::to_owned),
                    size: m.attr("size").map(|s| scan_int(s).filter(|&v| i32::try_from(v).is_ok()).unwrap_or(0)),
                    data: std::mem::take(&mut m.text),
                },
            };
            SetMember { name: m.attr("name").map(str::to_owned), update }
        })
        .collect();
    Some(SetVector {
        kind,
        device: e.attr("device")?.to_owned(),
        name: e.attr("name")?.to_owned(),
        state: e.attr("state").and_then(PropState::parse),
        timeout: e.attr("timeout").and_then(scan_double),
        timestamp: e.attr("timestamp").map(str::to_owned),
        message: e.attr("message").map(str::to_owned),
        members,
    })
}

/// `crackISState`: `On` on its first two letters; anything else is Off.
fn switch_on(s: &str) -> bool {
    s.starts_with("On")
}

// ---------------------------------------------------------------- writers

/// `userio_xml_escape`.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// A number as `IUUserIONumberContext` writes it: `%.20g`.
fn number_text(v: f64) -> String {
    fmt_gp(v, 20)
}

/// `IUUserIOGetProperties`: `device` and `name` only when non-empty.
pub fn get_properties(device: Option<&str>, name: Option<&str>) -> String {
    let mut s = String::from("<getProperties version='1.7'");
    for (key, v) in [("device", device), ("name", name)] {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            s += &format!(" {key}='{}'", escape(v));
        }
    }
    s + "/>\n"
}

/// `IUUserIOEnableBLOB`.
pub fn enable_blob(device: &str, name: Option<&str>, mode: BlobMode) -> String {
    let name = name.map(|n| format!("' name='{}", escape(n))).unwrap_or_default();
    let mode = match mode {
        BlobMode::Never => "Never",
        BlobMode::Also => "Also",
        BlobMode::Only => "Only",
    };
    format!("<enableBLOB device='{}{name}'>{mode}</enableBLOB>\n", escape(device))
}

/// `IUUserIONewNumber`: every member, as given.
pub fn new_number_vector(device: &str, name: &str, members: &[(&str, f64)]) -> String {
    let body = members
        .iter()
        .map(|(n, v)| format!("  <oneNumber name='{}'>\n      {}\n  </oneNumber>\n", escape(n), number_text(*v)))
        .collect::<String>();
    format!("<newNumberVector device='{}' name='{}'>\n{body}</newNumberVector>\n", escape(device), escape(name))
}

/// `IUUserIONewSwitch`: for a OneOfMany vector with a member On, that member
/// only; otherwise every member.
pub fn new_switch_vector(device: &str, name: &str, rule: Rule, members: &[(&str, bool)]) -> String {
    let on = members.iter().find(|(_, on)| *on);
    let sent = match on {
        Some(m) if rule == Rule::OneOfMany => std::slice::from_ref(m),
        _ => members,
    };
    let body = sent
        .iter()
        .map(|(n, on)| format!("  <oneSwitch name='{}'>\n      {}\n  </oneSwitch>\n", escape(n), if *on { "On" } else { "Off" }))
        .collect::<String>();
    format!("<newSwitchVector device='{}' name='{}'>\n{body}</newSwitchVector>\n", escape(device), escape(name))
}

/// `IUUserIONewText`.
pub fn new_text_vector(device: &str, name: &str, members: &[(&str, &str)]) -> String {
    let body = members
        .iter()
        .map(|(n, t)| format!("  <oneText name='{}'>\n      {}\n  </oneText>\n", escape(n), escape(t)))
        .collect::<String>();
    format!("<newTextVector device='{}' name='{}'>\n{body}</newTextVector>\n", escape(device), escape(name))
}

/// `IUUserIOPingReply`.
pub fn ping_reply(uid: &str) -> String {
    format!("<pingReply uid='{}' />\n", escape(uid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::rng::Rng;

    /// Odd input lilxml copes with; the expected trees are what INDI's own
    /// lilxml.cpp produced for it (a throwaway harness over `parseXMLChunk`),
    /// but for `&#10;`, which lilxml keeps raw.
    const TRICKY: &str = "<?xml version='1.0'?>\n<!-- a comment -->\n\
        <message device='CCD Simulator' timestamp='2026-11-11T10:00:01' \
        message='[INFO] a &lt;b&gt; &amp; &quot;c&quot; &apos;d&apos; &foo; &#65;'/>\n\
        <setTextVector device=\"D\" name=\"T\" state=\"Ok\">\n\
        \x20 <oneText name='A'>x &amp; y &lt;z&gt;  </oneText>\n\
        \x20 <oneText name='B'>  pre <!-- inner --> post </oneText>\n\
        \x20 <oneText name='C'>a<b/>c</oneText>\n\
        </setTextVector>\n\
        garbage < bad\n<pingRequest uid='u1'/>\n\
        <a x='1&#10;2' y = \"q\" ><<b/></a>\n<c></d>\n<e/ >\n<f z='a\tb'/>\n<g>\ntext\n</g >\n< h/>\n<i></ i>\n";

    fn show(e: &Element) -> String {
        let attrs: String = e.attrs.iter().map(|(n, v)| format!(" {n}=[{v}]")).collect();
        let kids: String = e.children.iter().map(show).collect();
        format!("<{}{attrs}>[{}]{kids}</>", e.name, e.text_str())
    }

    #[test]
    fn tokenizer_matches_lilxml() {
        let got: Vec<String> = XmlStream::new().push(TRICKY.as_bytes()).iter().map(show).collect();
        assert_eq!(
            got,
            [
                "<message device=[CCD Simulator] timestamp=[2026-11-11T10:00:01] \
                 message=[[INFO] a <b> & \"c\" 'd' &foo; A]>[]</>",
                "<setTextVector device=[D] name=[T] state=[Ok]>[]<oneText name=[A]>[x & y <z>]</>\
                 <oneText name=[B]>[pre  post]</><oneText name=[C]>[ac]<b>[]</></></>",
                // `< bad` swallows the `<p` of the ping, which is lost.
                "<a x=[1\n2] y=[q]>[]<b>[]</></>",
                // `<c></d>` and `<e/ >` are dropped; a tab in a value too.
                "<f z=[ab]>[]</>",
                "<g>[text]</>",
                "<h>[]</>",
                "<i>[]</>",
            ]
        );
    }

    #[test]
    fn any_split_gives_the_same_elements() {
        let whole = XmlStream::new().push(TRICKY.as_bytes());
        let mut rng = Rng::new(33);
        for _ in 0..200 {
            let mut stream = XmlStream::new();
            let mut got = Vec::new();
            let mut rest = TRICKY.as_bytes();
            while !rest.is_empty() {
                let n = (1 + rng.below(40) as usize).min(rest.len());
                got.extend(stream.push(&rest[..n]));
                rest = &rest[n..];
            }
            assert_eq!(got, whole);
        }
    }

    #[test]
    fn entities_numeric_references_and_errors() {
        let mut s = XmlStream::new();
        // lilxml copies an entity opening the text raw; this decodes it.
        let e = &s.push(b"<t a='&#x263A;&amp;'>&lt;x&gt; &#233;t&#xE9;</t>")[0];
        assert_eq!(e.attr("a"), Some("\u{263A}&"));
        assert_eq!(e.text_str(), "<x> \u{e9}t\u{e9}");
        // A NUL resets the parser; the next element still comes out.
        assert!(s.push(b"<a>\0").is_empty());
        assert_eq!(s.last_error.as_deref(), Some("early XML EOF"));
        assert_eq!(s.push(b"<b/>")[0].name, "b");
        // Each chunk starts with no error, as parseXMLChunk clears ynot.
        assert_eq!(s.last_error, None);
    }

    #[test]
    fn sexagesimal_like_f_scansexa() {
        // Goldens from INDI's f_scansexa (indicom.c) under glibc.
        let cases: &[(&str, Option<f64>)] = &[
            ("12:30:00", Some(12.5)),
            ("-12:30:00", Some(-12.5)),
            ("-12:30:45", Some(-12.512499999999999)),
            ("12:30:45.5", Some(12.512638888888889)),
            ("359:59:59.9", Some(359.99997222222225)),
            ("12 30 45", Some(123045.0)),
            (" - 12 : 30", Some(-12.5)),
            ("+5:30", Some(5.5)),
            ("-0:30", Some(-0.5)),
            ("12:-30", Some(12.5)),
            ("12:.5", Some(12.083333333333334)),
            ("12h30m15s", Some(12.504166666666666)),
            ("12:30:", Some(12.5)),
            ("\t7:30", Some(7.5)),
            ("1e-06", Some(1e-6)),
            ("-1.5e2", Some(-150.0)),
            ("1e2:30", Some(100.5)),
            ("3.5", Some(3.5)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("-.5", Some(-0.5)),
            ("1.e2", Some(100.0)),
            ("inf", Some(f64::INFINITY)),
            ("abc", None),
            ("", None),
            ("-", None),
            ("1e", None),
            ("12e:30", None),
        ];
        for &(s, want) in cases {
            assert_eq!(scan_sexa(s), want, "{s:?}");
        }
        assert!(scan_sexa("nan").unwrap().is_nan());
    }

    #[test]
    fn stod_reads_the_leading_number() {
        assert_eq!(scan_double("12:30"), Some(12.0));
        assert_eq!(scan_double(" 42.5"), Some(42.5));
        assert_eq!(scan_double("  -3"), Some(-3.0));
        assert_eq!(scan_double("1e"), Some(1.0));
        assert_eq!(scan_double("-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(scan_double("abc"), None);
        assert_eq!(scan_double(""), None);
        assert_eq!(scan_int(" 22abc"), Some(22));
        assert_eq!(scan_int("-5"), Some(-5));
        assert_eq!(scan_int("x"), None);
    }

    fn message(xml: &str) -> Option<Message> {
        Message::from_element(XmlStream::new().push(xml.as_bytes()).remove(0))
    }

    #[test]
    fn def_vector_attributes_and_defaults() {
        let Some(Message::Def(d)) = message(
            "<defNumberVector device='Telescope Simulator' name='EQUATORIAL_EOD_COORD' label='Eq. Coordinates' \
             group='Main Control' state='Ok' perm='rw' timeout='60' timestamp='2026-11-11T10:00:00'>\
             <defNumber name='RA' label='RA (hh:mm:ss)' format='%010.6m' min='0' max='24' step='0'>12:30:00</defNumber>\
             <defNumber name='DEC' label='DEC (dd:mm:ss)' format='%010.6m' min='-90' max='90' step='0'>-5:23:24</defNumber>\
             </defNumberVector>",
        ) else {
            panic!()
        };
        assert_eq!((d.kind, d.device.as_str(), d.name.as_str()), (Kind::Number, "Telescope Simulator", "EQUATORIAL_EOD_COORD"));
        assert_eq!((d.label.as_str(), d.group.as_str()), ("Eq. Coordinates", "Main Control"));
        assert_eq!((d.state, d.perm, d.timeout), (PropState::Ok, Perm::Rw, 60.0));
        assert_eq!(
            d.members[1],
            Member {
                name: "DEC".into(),
                label: "DEC (dd:mm:ss)".into(),
                value: Value::Number { value: -5.390000000000001, format: "%010.6m".into(), min: -90.0, max: 90.0, step: 0.0 },
            }
        );
        // An unknown state, perm or rule takes libindi's default.
        let Some(Message::Def(d)) = message(
            "<defSwitchVector device='D' name='S' state='Bogus' perm='xx' rule='Bogus'>\
             <defSwitch name='A'>On</defSwitch><defSwitch name='B'>Off</defSwitch><defNumber name='C'>On</defNumber>\
             </defSwitchVector>",
        ) else {
            panic!()
        };
        assert_eq!((d.state, d.perm, d.rule, d.timeout), (PropState::Ok, Perm::Ro, Rule::OneOfMany, 0.0));
        let values: Vec<_> = d.members.iter().map(|m| m.value.clone()).collect();
        assert_eq!(values, [Value::Switch(true), Value::Switch(false)]);
        // device and name are required; echoes and pingReply are ignored.
        assert_eq!(message("<defTextVector name='T'><defText name='A'>x</defText></defTextVector>"), None);
        assert_eq!(message("<newNumberVector device='D' name='N'/>"), None);
        assert_eq!(message("<pingReply uid='1'/>"), None);
    }

    #[test]
    fn set_vector_reads_like_set_value() {
        let Some(Message::Set(s)) = message(
            "<setNumberVector device='D' name='N' state='Busy' timeout='x'>\
             <oneNumber name='A' min='1' max='abc'>12:30</oneNumber><oneSwitch name='B'>7</oneSwitch></setNumberVector>",
        ) else {
            panic!()
        };
        assert_eq!((s.state, s.timeout), (Some(PropState::Busy), None));
        // std::stod: no sexagesimal; an unparsable min reads 0.
        assert_eq!(s.members[0].update, Update::Number { value: 12.0, min: Some(1.0), max: Some(0.0) });
        // Any child tag counts as a member.
        assert_eq!(s.members[1].update, Update::Number { value: 7.0, min: None, max: None });
        let Some(Message::Set(s)) = message("<setLightVector device='D' name='L' state='Okay'><oneLight name='X'>Bad</oneLight></setLightVector>")
        else {
            panic!()
        };
        assert_eq!((s.state, &s.members[0].update), (Some(PropState::Ok), &Update::Light(PropState::Ok)));
        let Some(Message::Set(s)) = message(
            "<setBLOBVector device='D' name='CCD1' state='Ok'>\
             <oneBLOB name='CCD1' size='3' enclen='4' format='.fits'>\nQUJD\n</oneBLOB><oneText name='X'>y</oneText>\
             <oneBLOB name='' size='2147483648' format='.fits'>QUJD</oneBLOB><oneBLOB size='-2147483648'/></setBLOBVector>",
        ) else {
            panic!()
        };
        assert_eq!(s.members.len(), 3);
        assert_eq!(s.members[0].update, Update::Blob { format: Some(".fits".into()), size: Some(3), data: b"QUJD".to_vec() });
        // An empty name is a name; a size past an int reads 0 (stoi throws).
        assert_eq!(s.members[1].name.as_deref(), Some(""));
        assert_eq!(s.members[1].update, Update::Blob { format: Some(".fits".into()), size: Some(0), data: b"QUJD".to_vec() });
        assert_eq!((s.members[2].name.as_deref(), &s.members[2].update), (None, &Update::Blob { format: None, size: Some(i32::MIN as i64), data: vec![] }));
        let Some(Message::Set(s)) = message("<setSwitchVector device='D' name='S' state='Alert '/>") else { panic!() };
        assert_eq!(s.state, None);
    }

    #[test]
    fn other_messages() {
        assert_eq!(
            message("<delProperty device='CCD Simulator' name='CCD1' timestamp='t'/>"),
            Some(Message::DelProperty {
                device: "CCD Simulator".into(),
                name: Some("CCD1".into()),
                timestamp: Some("t".into()),
                message: None,
            })
        );
        assert_eq!(message("<pingRequest uid='42'/>"), Some(Message::PingRequest { uid: "42".into() }));
        assert_eq!(message("<getProperties version='1.7' device='X'/>"), Some(Message::GetProperties));
    }

    #[test]
    fn writers() {
        assert_eq!(get_properties(None, None), "<getProperties version='1.7'/>\n");
        assert_eq!(get_properties(Some(""), Some("")), "<getProperties version='1.7'/>\n");
        assert_eq!(
            get_properties(Some("Guide & Co"), Some("CCD1")),
            "<getProperties version='1.7' device='Guide &amp; Co' name='CCD1'/>\n"
        );
        assert_eq!(
            enable_blob("CCD Simulator", Some("CCD2"), BlobMode::Also),
            "<enableBLOB device='CCD Simulator' name='CCD2'>Also</enableBLOB>\n"
        );
        assert_eq!(
            enable_blob("Telescope Simulator", None, BlobMode::Never),
            "<enableBLOB device='Telescope Simulator'>Never</enableBLOB>\n"
        );
        assert_eq!(
            new_number_vector("Telescope Simulator", "TELESCOPE_TIMED_GUIDE_NS", &[("TIMED_GUIDE_N", 1500.0), ("TIMED_GUIDE_S", 0.0)]),
            "<newNumberVector device='Telescope Simulator' name='TELESCOPE_TIMED_GUIDE_NS'>\n\
             \x20 <oneNumber name='TIMED_GUIDE_N'>\n      1500\n  </oneNumber>\n\
             \x20 <oneNumber name='TIMED_GUIDE_S'>\n      0\n  </oneNumber>\n\
             </newNumberVector>\n"
        );
        assert_eq!(
            new_switch_vector("CCD Simulator", "CONNECTION", Rule::OneOfMany, &[("CONNECT", true), ("DISCONNECT", false)]),
            "<newSwitchVector device='CCD Simulator' name='CONNECTION'>\n\
             \x20 <oneSwitch name='CONNECT'>\n      On\n  </oneSwitch>\n\
             </newSwitchVector>\n"
        );
        assert_eq!(
            new_switch_vector("D", "S", Rule::AnyOfMany, &[("A", true), ("B", false)]),
            "<newSwitchVector device='D' name='S'>\n\
             \x20 <oneSwitch name='A'>\n      On\n  </oneSwitch>\n\
             \x20 <oneSwitch name='B'>\n      Off\n  </oneSwitch>\n\
             </newSwitchVector>\n"
        );
        assert_eq!(
            new_text_vector("D", "T", &[("A", "<a 'b'>")]),
            "<newTextVector device='D' name='T'>\n\
             \x20 <oneText name='A'>\n      &lt;a &apos;b&apos;&gt;\n  </oneText>\n\
             </newTextVector>\n"
        );
        assert_eq!(ping_reply("1234\""), "<pingReply uid='1234&quot;' />\n");
    }

    #[test]
    fn numbers_go_out_as_percent_20g_and_read_back_identical() {
        let texts: Vec<_> = [0.5, 0.1, 1.0, -0.0, 1.5e-5, 2.5e20, 1e19, f64::NAN].into_iter().map(number_text).collect();
        assert_eq!(
            texts,
            ["0.5", "0.10000000000000000555", "1", "-0", "1.500000000000000038e-05", "2.5e+20", "10000000000000000000", "nan"]
        );
        let values = [0.1, 1.0 / 3.0, 1e-7, 123456789.123, 6.02e23, -2.5, f64::MIN_POSITIVE, f64::MAX];
        let xml = new_number_vector("D", "N", &values.map(|v| ("V", v)));
        let e = &XmlStream::new().push(xml.as_bytes())[0];
        for (child, v) in e.children.iter().zip(values) {
            assert_eq!(scan_double(&child.text_str()), Some(v));
            assert_eq!(scan_sexa(&child.text_str()), Some(v));
        }
    }
}

