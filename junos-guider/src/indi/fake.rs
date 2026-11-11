//! A scripted `indiserver` for the tests: a listener on an ephemeral port
//! whose connections run a script on each element the client sends, plus
//! the XML the drivers write, built from parts.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::xml::{Element, XmlStream};
use crate::engine::calibration::ProfileAccess;

/// One client connection, as the script sees it. Writes are serialised so
/// a delayed reply never splits another.
#[derive(Clone)]
pub(crate) struct Peer(Arc<Mutex<TcpStream>>);

impl Peer {
    pub(crate) fn send(&self, xml: &str) {
        let _ = self.0.lock().unwrap().write_all(xml.as_bytes());
    }

    /// Sends `xml` from another thread `ms` later.
    pub(crate) fn send_after(&self, ms: u64, xml: String) {
        let peer = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(ms));
            peer.send(&xml);
        });
    }
}

type Script = Box<dyn FnMut(&Element, &Peer) + Send>;

struct Inner {
    got: Mutex<Vec<Element>>,
    cond: Condvar,
    peers: Mutex<Vec<Peer>>,
    accepted: AtomicUsize,
    script: Mutex<Script>,
}

pub(crate) struct Fake {
    pub(crate) port: u16,
    inner: Arc<Inner>,
}

impl Fake {
    pub(crate) fn start(script: impl FnMut(&Element, &Peer) + Send + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let inner = Arc::new(Inner {
            got: Mutex::default(),
            cond: Condvar::new(),
            peers: Mutex::default(),
            accepted: AtomicUsize::new(0),
            script: Mutex::new(Box::new(script)),
        });
        let acc = Arc::clone(&inner);
        thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let peer = Peer(Arc::new(Mutex::new(sock.try_clone().unwrap())));
                acc.peers.lock().unwrap().push(peer.clone());
                acc.accepted.fetch_add(1, Ordering::SeqCst);
                let inner = Arc::clone(&acc);
                thread::spawn(move || serve(&inner, sock, &peer));
            }
        });
        Fake { port, inner }
    }

    /// Sends to every live connection.
    pub(crate) fn push(&self, xml: &str) {
        for p in self.inner.peers.lock().unwrap().iter() {
            p.send(xml);
        }
    }

    /// Closes every live connection: the server went away.
    pub(crate) fn drop_conns(&self) {
        for p in self.inner.peers.lock().unwrap().drain(..) {
            let _ = p.0.lock().unwrap().shutdown(Shutdown::Both);
        }
    }

    pub(crate) fn accepted(&self) -> usize {
        self.inner.accepted.load(Ordering::SeqCst)
    }

    /// Every element the clients sent, in order.
    pub(crate) fn received(&self) -> Vec<Element> {
        self.inner.got.lock().unwrap().clone()
    }

    /// Waits up to 3 s for what was received to satisfy `pred`.
    pub(crate) fn wait_for(&self, pred: impl Fn(&[Element]) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got = self.inner.got.lock().unwrap();
        while !pred(&got) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            got = self.inner.cond.wait_timeout(got, deadline - now).unwrap().0;
        }
        true
    }
}

fn serve(inner: &Inner, mut sock: TcpStream, peer: &Peer) {
    let mut stream = XmlStream::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = sock.read(&mut buf) {
        if n == 0 {
            break;
        }
        for e in stream.push(&buf[..n]) {
            inner.got.lock().unwrap().push(e.clone());
            inner.cond.notify_all();
            (inner.script.lock().unwrap())(&e, peer);
        }
    }
}

/// An element the client sent, on one line: `newNumberVector Dev.PROP A=1 B=0`,
/// or the tag, its attributes and its text.
pub(crate) fn summary(e: &Element) -> String {
    if e.name.starts_with("new") {
        let mut s =
            format!("{} {}.{}", e.name, e.attr("device").unwrap_or_default(), e.attr("name").unwrap_or_default());
        for c in &e.children {
            s += &format!(" {}={}", c.attr("name").unwrap_or_default(), c.text_str());
        }
        return s;
    }
    let mut s = e.name.clone();
    for (k, v) in &e.attrs {
        s += &format!(" {k}={v}");
    }
    let t = e.text_str();
    if !t.is_empty() {
        s += &format!(" {t}");
    }
    s
}

/// The members of a `newNumberVector`; none for another element.
pub(crate) fn numbers(e: &Element) -> Vec<(String, f64)> {
    let value = |c: &Element| c.text_str().parse().ok().map(|v| (c.attr("name").unwrap_or_default().to_owned(), v));
    if e.name == "newNumberVector" { e.children.iter().filter_map(value).collect() } else { Vec::new() }
}

/// `(name, value, min, max)` per member.
pub(crate) fn def_number(dev: &str, name: &str, state: &str, members: &[(&str, f64, f64, f64)]) -> String {
    let body: String = members
        .iter()
        .map(|(n, v, min, max)| {
            format!("<defNumber name='{n}' label='{n}' format='%g' min='{min}' max='{max}' step='1'>{v}</defNumber>")
        })
        .collect();
    format!(
        "<defNumberVector device='{dev}' name='{name}' label='{name}' group='Main' state='{state}' perm='rw' \
         timeout='60'>{body}</defNumberVector>\n"
    )
}

pub(crate) fn def_switch(dev: &str, name: &str, rule: &str, members: &[(&str, bool)]) -> String {
    let body: String = members
        .iter()
        .map(|(n, on)| format!("<defSwitch name='{n}' label='{n}'>{}</defSwitch>", if *on { "On" } else { "Off" }))
        .collect();
    format!(
        "<defSwitchVector device='{dev}' name='{name}' label='{name}' group='Main' state='Idle' perm='rw' \
         rule='{rule}' timeout='60'>{body}</defSwitchVector>\n"
    )
}

pub(crate) fn def_text(dev: &str, name: &str, members: &[(&str, &str)]) -> String {
    let body: String = members.iter().map(|(n, t)| format!("<defText name='{n}' label='{n}'>{t}</defText>")).collect();
    format!(
        "<defTextVector device='{dev}' name='{name}' label='{name}' group='Main' state='Idle' perm='ro' \
         timeout='60'>{body}</defTextVector>\n"
    )
}

pub(crate) fn def_light(dev: &str, name: &str, members: &[(&str, &str)]) -> String {
    let body: String =
        members.iter().map(|(n, s)| format!("<defLight name='{n}' label='{n}'>{s}</defLight>")).collect();
    format!(
        "<defLightVector device='{dev}' name='{name}' label='{name}' group='Main' state='Idle'>{body}</defLightVector>\n"
    )
}

pub(crate) fn def_blob(dev: &str, name: &str, members: &[&str]) -> String {
    let body: String = members.iter().map(|n| format!("<defBLOB name='{n}' label='{n}'/>")).collect();
    format!(
        "<defBLOBVector device='{dev}' name='{name}' label='{name}' group='Main' state='Idle' perm='ro' \
         timeout='60'>{body}</defBLOBVector>\n"
    )
}

pub(crate) fn set_number(dev: &str, name: &str, state: &str, members: &[(&str, f64)]) -> String {
    let body: String = members.iter().map(|(n, v)| format!("<oneNumber name='{n}'>{v}</oneNumber>")).collect();
    format!("<setNumberVector device='{dev}' name='{name}' state='{state}' timeout='60'>{body}</setNumberVector>\n")
}

pub(crate) fn set_switch(dev: &str, name: &str, state: &str, members: &[(&str, bool)]) -> String {
    let body: String = members
        .iter()
        .map(|(n, on)| format!("<oneSwitch name='{n}'>{}</oneSwitch>", if *on { "On" } else { "Off" }))
        .collect();
    format!("<setSwitchVector device='{dev}' name='{name}' state='{state}' timeout='60'>{body}</setSwitchVector>\n")
}

/// The answer of a driver to `CONNECTION`: the switch as asked, Ok.
pub(crate) fn connection_reply(e: &Element) -> Option<String> {
    let on = e.children.iter().any(|c| c.attr("name") == Some("CONNECT") && c.text_str() == "On");
    let dev = e.attr("device")?;
    (e.name == "newSwitchVector" && e.attr("name") == Some("CONNECTION"))
        .then(|| set_switch(dev, "CONNECTION", "Ok", &[("CONNECT", on), ("DISCONNECT", !on)]))
}

pub(crate) fn set_blob(dev: &str, name: &str, member: &str, format: &str, data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut enc = String::new();
    for c in data.chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            enc.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    format!(
        "<setBLOBVector device='{dev}' name='{name}' state='Ok' timeout='60'><oneBLOB name='{member}' size='{}' \
         enclen='{}' format='{format}'>{enc}</oneBLOB></setBLOBVector>\n",
        data.len(),
        enc.len()
    )
}

/// `CONNECTION` and `DRIVER_INFO`, the first two properties of a driver.
pub(crate) fn driver(dev: &str, interfaces: u16, connected: bool) -> String {
    def_switch(dev, "CONNECTION", "OneOfMany", &[("CONNECT", connected), ("DISCONNECT", !connected)])
        + &def_text(
            dev,
            "DRIVER_INFO",
            &[
                ("DRIVER_NAME", dev),
                ("DRIVER_EXEC", "indi_fake"),
                ("DRIVER_VERSION", "1.0"),
                ("DRIVER_INTERFACE", &interfaces.to_string()),
            ],
        )
}

/// A 16-bit FITS as the CCD Simulator writes one (`BZERO 32768`).
pub(crate) fn fits(w: usize, h: usize, px: &[u16]) -> Vec<u8> {
    let mut hdr = String::new();
    for c in [
        "SIMPLE  =                    T".to_string(),
        "BITPIX  =                   16".into(),
        "NAXIS   =                    2".into(),
        format!("NAXIS1  = {w:20}"),
        format!("NAXIS2  = {h:20}"),
        "BZERO   =                32768".into(),
        "END".into(),
    ] {
        hdr += &format!("{c:80}");
    }
    let mut bytes = format!("{hdr:2880}").into_bytes();
    for &v in px {
        bytes.extend_from_slice(&((v as i32 - 32768) as i16).to_be_bytes());
    }
    bytes.resize(bytes.len().div_ceil(2880) * 2880, 0);
    bytes
}

/// A profile in memory, every value as text.
#[derive(Default)]
pub(crate) struct Profile(pub(crate) HashMap<String, String>);

impl ProfileAccess for Profile {
    fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }
    fn get_f64(&self, key: &str, default: f64) -> f64 {
        self.0.get(key).map_or(default, |v| v.parse().unwrap())
    }
    fn get_i32(&self, key: &str, default: i32) -> i32 {
        self.0.get(key).map_or(default, |v| v.parse().unwrap())
    }
    fn get_str(&self, key: &str, default: &str) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| default.to_owned())
    }
    fn set_f64(&mut self, key: &str, value: f64) {
        self.0.insert(key.to_owned(), value.to_string());
    }
    fn set_i32(&mut self, key: &str, value: i32) {
        self.0.insert(key.to_owned(), value.to_string());
    }
    fn set_str(&mut self, key: &str, value: &str) {
        self.0.insert(key.to_owned(), value.to_owned());
    }
}
