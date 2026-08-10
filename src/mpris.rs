//! MPRIS2 over D-Bus, marshalled by hand.
//!
//! Desktop shells, headset buttons and `playerctl` all speak MPRIS, and MPRIS is D-Bus, so a
//! terminal player that wants a media key has to put a bus name on the session bus. The obvious
//! route is `zbus`, which drags in a full async runtime and around eighty crates to send a few
//! hundred bytes a second down a Unix socket the player already knows how to open. The argument that
//! made the mpv IPC hand-rolled applies here: the wire format is small, fixed and documented, the
//! traffic is trivial, and a bug in it is visible immediately in `playerctl`.
//!
//! So this is the whole thing in one file, std only: the SASL EXTERNAL handshake, a marshaller for
//! the dozen type codes MPRIS actually uses, and a dispatcher for the three interfaces a player
//! must expose. The marshaller is the part worth reading twice - every D-Bus type has an alignment,
//! padding is counted from the start of the *message* rather than from the start of the value, and
//! a single missing pad byte turns into "Invalid or incomplete message" with no hint of where.
//!
//! The socket lives on its own thread. `publish` drops a snapshot into a mutex and returns; the
//! thread notices it within a tick, diffs it against what the bus was last told, and emits
//! `PropertiesChanged` only for the properties that really moved. Nothing on the render path ever
//! waits for the bus, and a bus that dies mid-session degrades to a silent no-op rather than
//! taking the player with it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Well-known name to claim. MPRIS clients scan for the `org.mpris.MediaPlayer2.*` prefix.
const BUS_NAME: &str = "org.mpris.MediaPlayer2.ytmplayer";
/// The one object path the specification allows a player to serve.
const OBJ_PATH: &str = "/org/mpris/MediaPlayer2";
const IFACE_ROOT: &str = "org.mpris.MediaPlayer2";
const IFACE_PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const IFACE_PROPS: &str = "org.freedesktop.DBus.Properties";
const IFACE_INTROSPECT: &str = "org.freedesktop.DBus.Introspectable";
const IFACE_PEER: &str = "org.freedesktop.DBus.Peer";

const MSG_METHOD_CALL: u8 = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_ERROR: u8 = 3;
const MSG_SIGNAL: u8 = 4;
/// The caller does not want a reply and must not be sent one.
const FLAG_NO_REPLY: u8 = 1;

/// How long the reader parks before it looks at the publish slot again. This is the worst-case age
/// of anything the desktop reads back, and it also bounds how long `start` and the thread take
/// to notice that the player is shutting down. Bus traffic wakes the read early, so command latency
/// does not depend on it.
const TICK: Duration = Duration::from_millis(40);
/// Startup is allowed to block for this long in total; a session bus is a local socket, so anything
/// slower than this is wedged and the player is better off without MPRIS than stalled behind it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// A write that cannot complete in this long means the daemon has stopped reading. There is no
/// useful recovery, so the connection is dropped and every later call becomes a no-op.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Refuse to buffer a frame larger than the specification's own maximum, so a corrupt length
/// field cannot turn into an allocation the size of the address space.
const MAX_MESSAGE: usize = 128 * 1024 * 1024;
/// Position jump, in microseconds, past which a change is a seek rather than ordinary playback.
/// Frame-to-frame drift on the mpv side is a few milliseconds; a keypress moves whole seconds.
const SEEK_EPSILON_US: i64 = 1_000_000;
/// Cap on undelivered commands. A UI that stops draining must not let a chatty desktop grow this
/// without bound; dropping the newest keeps the oldest (and so the causally first) intact.
const MAX_PENDING_COMMANDS: usize = 64;

/// Recover the guard instead of propagating a poisoned lock. The data behind these mutexes is a
/// plain snapshot with no invariant to break, and a panic elsewhere must not take out the bus
/// thread as well.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Marshalling
// ---------------------------------------------------------------------------------------------

/// A D-Bus value, restricted to the type codes MPRIS needs.
///
/// Keeping the whole tree in one enum is what makes `a{sv}` and nested variants a three-line case
/// in the encoder rather than a family of hand-written builders, and it lets the tests round-trip
/// arbitrary shapes through both directions.
#[derive(Clone, Debug, PartialEq)]
enum Val {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Path(String),
    Sig(String),
    /// Element signature plus the elements themselves; the signature is needed even when empty.
    Array(String, Vec<Val>),
    Struct(Vec<Val>),
    /// `a{sv}` specifically: the only dictionary MPRIS uses, and always string-keyed.
    Dict(Vec<(String, Val)>),
    Variant(Box<Val>),
}

impl Val {
    fn write_signature(&self, out: &mut String) {
        match self {
            Val::Byte(_) => out.push('y'),
            Val::Bool(_) => out.push('b'),
            Val::I16(_) => out.push('n'),
            Val::U16(_) => out.push('q'),
            Val::I32(_) => out.push('i'),
            Val::U32(_) => out.push('u'),
            Val::I64(_) => out.push('x'),
            Val::U64(_) => out.push('t'),
            Val::F64(_) => out.push('d'),
            Val::Str(_) => out.push('s'),
            Val::Path(_) => out.push('o'),
            Val::Sig(_) => out.push('g'),
            Val::Array(elem, _) => {
                out.push('a');
                out.push_str(elem);
            }
            Val::Struct(fields) => {
                out.push('(');
                for f in fields {
                    f.write_signature(out);
                }
                out.push(')');
            }
            Val::Dict(_) => out.push_str("a{sv}"),
            Val::Variant(_) => out.push('v'),
        }
    }

    fn signature(&self) -> String {
        let mut s = String::new();
        self.write_signature(&mut s);
        s
    }
}

/// Concatenated signature of a message body.
fn signature_of(vals: &[Val]) -> String {
    let mut s = String::new();
    for v in vals {
        v.write_signature(&mut s);
    }
    s
}

/// Alignment of the type `sig` starts with. Everything in the format hangs off this table: get one
/// entry wrong and the value decodes as garbage several fields later, where the mistake is
/// invisible.
fn alignment(sig: &str) -> usize {
    match sig.as_bytes().first() {
        Some(b'n' | b'q') => 2,
        Some(b'b' | b'i' | b'u' | b's' | b'o' | b'a' | b'h') => 4,
        Some(b'x' | b't' | b'd' | b'(' | b'{' | b'r' | b'e') => 8,
        // y, g, v and anything unrecognised.
        _ => 1,
    }
}

/// The type codes that stand on their own, `h` included so a signature carrying a file descriptor
/// can still be skipped past even though nothing here ever sends one.
const BASIC_TYPES: &[u8] = b"ybnqiuxtdsogvh";

/// Split one complete type off the front of a signature, returning it and the rest.
///
/// Needed because a body signature is a concatenation with no separators, and container types
/// nest: `a{sv}as` is two types, not seven.
fn split_type(sig: &str) -> Option<(&str, &str)> {
    let b = sig.as_bytes();
    let mut at = 0usize;
    // Any number of array markers, then exactly one complete element type.
    while b.get(at) == Some(&b'a') {
        at += 1;
    }
    match *b.get(at)? {
        b'(' | b'{' => {
            // A struct may contain a dict entry and the reverse, so both bracket kinds are counted
            // together or the scan stops at the wrong closer. Signatures arrive off the wire, so
            // an unbalanced one has to fail rather than underflow.
            let mut depth = 0usize;
            loop {
                match *b.get(at)? {
                    b'(' | b'{' => depth += 1,
                    b')' | b'}' => depth = depth.checked_sub(1)?,
                    _ => {}
                }
                at += 1;
                if depth == 0 {
                    break;
                }
            }
        }
        // Any other basic type code is one character. An unrecognised one is not a type at all,
        // and guessing would decode the rest of the message against the wrong layout.
        c if BASIC_TYPES.contains(&c) => at += 1,
        _ => return None,
    }
    Some(sig.split_at(at))
}

/// Little-endian message writer.
///
/// Offsets are counted from index 0 of this buffer, which is only correct because every buffer
/// handed to it starts at an eight-byte boundary of the finished message: the header at 0, the body
/// straight after the header's own pad to 8.
#[derive(Default)]
struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    fn pad(&mut self, align: usize) {
        while !self.buf.len().is_multiple_of(align) {
            self.buf.push(0);
        }
    }

    fn byte(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// String-like: length prefix, bytes, and a NUL the length does not count.
    fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    /// Signatures carry a single byte of length, so they can never exceed 255 characters.
    fn sig(&mut self, s: &str) {
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    /// Array framing: a byte count, then padding up to the element alignment, then the elements.
    /// The padding sits *outside* the count, which is the trap - an empty array of structs is four
    /// bytes of zero length followed by four bytes of padding.
    fn array<F: FnOnce(&mut Enc)>(&mut self, elem_align: usize, fill: F) {
        self.u32(0);
        let len_at = self.buf.len() - 4;
        self.pad(elem_align);
        let start = self.buf.len();
        fill(self);
        let len = (self.buf.len() - start) as u32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    }

    fn value(&mut self, v: &Val) {
        match v {
            Val::Byte(x) => self.buf.push(*x),
            Val::Bool(x) => self.u32(u32::from(*x)),
            Val::I16(x) => {
                self.pad(2);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Val::U16(x) => {
                self.pad(2);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Val::I32(x) => self.u32(*x as u32),
            Val::U32(x) => self.u32(*x),
            Val::I64(x) => {
                self.pad(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Val::U64(x) => {
                self.pad(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Val::F64(x) => {
                self.pad(8);
                self.buf.extend_from_slice(&x.to_le_bytes());
            }
            Val::Str(s) | Val::Path(s) => self.string(s),
            Val::Sig(s) => self.sig(s),
            Val::Array(elem, items) => {
                let align = alignment(elem);
                self.array(align, |e| {
                    for it in items {
                        e.value(it);
                    }
                });
            }
            Val::Struct(fields) => {
                self.pad(8);
                for f in fields {
                    self.value(f);
                }
            }
            Val::Dict(entries) => self.array(8, |e| {
                for (k, v) in entries {
                    // Dict entries align like structs even though they are written inline.
                    e.pad(8);
                    e.string(k);
                    e.sig(&v.signature());
                    e.value(v);
                }
            }),
            Val::Variant(inner) => {
                self.sig(&inner.signature());
                self.value(inner);
            }
        }
    }
}

/// Message reader. Carries the sender's byte order because the bus forwards messages untouched and
/// a big-endian peer is legal, if unlikely on the hardware this runs on.
struct Dec<'a> {
    buf: &'a [u8],
    at: usize,
    le: bool,
}

impl<'a> Dec<'a> {
    fn new(buf: &'a [u8], le: bool) -> Self {
        Dec { buf, at: 0, le }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.buf.get(self.at..end)?;
        self.at = end;
        Some(s)
    }

    /// Skipped padding must be inside the buffer too, otherwise a truncated message reads as an
    /// empty one instead of an error.
    fn pad(&mut self, align: usize) -> Option<()> {
        let want = self.at.next_multiple_of(align);
        if want > self.buf.len() {
            return None;
        }
        self.at = want;
        Some(())
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.pad(2)?;
        let b: [u8; 2] = self.take(2)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&mut self) -> Option<u32> {
        self.pad(4)?;
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn u64(&mut self) -> Option<u64> {
        self.pad(8)?;
        let b: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(if self.le {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let s = self.take(len)?;
        // The trailing NUL is not part of the string but must be present.
        if self.u8()? != 0 {
            return None;
        }
        String::from_utf8(s.to_vec()).ok()
    }

    fn sig(&mut self) -> Option<String> {
        let len = self.u8()? as usize;
        let s = self.take(len)?;
        if self.u8()? != 0 {
            return None;
        }
        String::from_utf8(s.to_vec()).ok()
    }

    /// Read exactly one value of the type `sig` begins with.
    fn value(&mut self, sig: &str) -> Option<Val> {
        let (ty, _) = split_type(sig)?;
        let b = ty.as_bytes();
        Some(match b[0] {
            b'y' => Val::Byte(self.u8()?),
            b'b' => Val::Bool(self.u32()? != 0),
            b'n' => Val::I16(self.u16()? as i16),
            b'q' => Val::U16(self.u16()?),
            b'i' => Val::I32(self.u32()? as i32),
            b'u' => Val::U32(self.u32()?),
            b'x' => Val::I64(self.u64()? as i64),
            b't' => Val::U64(self.u64()?),
            b'd' => Val::F64(f64::from_bits(self.u64()?)),
            b's' => Val::Str(self.string()?),
            b'o' => Val::Path(self.string()?),
            b'g' => Val::Sig(self.sig()?),
            b'v' => {
                let inner = self.sig()?;
                Val::Variant(Box::new(self.value(&inner)?))
            }
            b'a' => {
                let elem = &ty[1..];
                let len = self.u32()? as usize;
                self.pad(alignment(elem))?;
                let end = self.at.checked_add(len)?;
                if end > self.buf.len() {
                    return None;
                }
                let dict = elem.starts_with('{');
                let mut items = Vec::new();
                let mut pairs = Vec::new();
                while self.at < end {
                    if dict {
                        // `{sv}` unwraps into the string-keyed form the rest of the module uses.
                        self.pad(8)?;
                        let inner = &elem[1..elem.len() - 1];
                        let (kt, vt) = split_type(inner)?;
                        let key = match self.value(kt)? {
                            Val::Str(s) => s,
                            _ => return None,
                        };
                        // The dictionary form this module uses is `a{sv}` by definition, so the
                        // variant wrapper carries no information the caller can act on; dropping
                        // it here is what makes an encoded dictionary decode back to itself.
                        let val = match self.value(vt)? {
                            Val::Variant(inner) => *inner,
                            other => other,
                        };
                        pairs.push((key, val));
                    } else {
                        items.push(self.value(elem)?);
                    }
                }
                if self.at != end {
                    return None;
                }
                if dict {
                    Val::Dict(pairs)
                } else {
                    Val::Array(elem.to_string(), items)
                }
            }
            b'(' => {
                self.pad(8)?;
                let mut rest = &ty[1..ty.len() - 1];
                let mut fields = Vec::new();
                while !rest.is_empty() {
                    let (one, tail) = split_type(rest)?;
                    fields.push(self.value(one)?);
                    rest = tail;
                }
                Val::Struct(fields)
            }
            _ => return None,
        })
    }

    /// Read a whole body: every type in `sig`, in order.
    fn values(&mut self, sig: &str) -> Option<Vec<Val>> {
        let mut rest = sig;
        let mut out = Vec::new();
        while !rest.is_empty() {
            let (one, tail) = split_type(rest)?;
            out.push(self.value(one)?);
            rest = tail;
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// Every header field this module sets. The codes come from the specification's table.
const F_PATH: u8 = 1;
const F_INTERFACE: u8 = 2;
const F_MEMBER: u8 = 3;
const F_ERROR_NAME: u8 = 4;
const F_REPLY_SERIAL: u8 = 5;
const F_DESTINATION: u8 = 6;
const F_SENDER: u8 = 7;
const F_SIGNATURE: u8 = 8;

/// Everything a message header can say, as a struct so building one stays a single expression.
#[derive(Default)]
struct Head<'a> {
    kind: u8,
    flags: u8,
    path: Option<&'a str>,
    interface: Option<&'a str>,
    member: Option<&'a str>,
    error_name: Option<&'a str>,
    reply_serial: Option<u32>,
    destination: Option<&'a str>,
}

/// Encode a complete message.
///
/// The body is encoded first because its length goes into the fixed header, and it can be encoded
/// standalone because the header is padded to eight before it is appended - so offset zero of the
/// body buffer is an eight-byte boundary of the finished message and every pad inside it lands
/// where the reader expects.
fn build(head: &Head, serial: u32, body: &[Val]) -> Vec<u8> {
    let mut b = Enc::default();
    for v in body {
        b.value(v);
    }
    let signature = signature_of(body);

    let mut m = Enc::default();
    m.byte(b'l');
    m.byte(head.kind);
    m.byte(head.flags);
    // Protocol version. Still 1, twenty years in.
    m.byte(1);
    m.u32(b.buf.len() as u32);
    m.u32(serial);
    // The field array starts at offset 16, which is already eight-aligned, so its `(yv)` elements
    // need no leading pad here - unlike the same array encoded on its own.
    m.array(8, |e| {
        let mut field = |code: u8, v: Val| {
            e.pad(8);
            e.byte(code);
            e.value(&Val::Variant(Box::new(v)));
        };
        if let Some(p) = head.path {
            field(F_PATH, Val::Path(p.to_string()));
        }
        if let Some(i) = head.interface {
            field(F_INTERFACE, Val::Str(i.to_string()));
        }
        if let Some(mm) = head.member {
            field(F_MEMBER, Val::Str(mm.to_string()));
        }
        if let Some(n) = head.error_name {
            field(F_ERROR_NAME, Val::Str(n.to_string()));
        }
        if let Some(s) = head.reply_serial {
            field(F_REPLY_SERIAL, Val::U32(s));
        }
        if let Some(d) = head.destination {
            field(F_DESTINATION, Val::Str(d.to_string()));
        }
        if !signature.is_empty() {
            field(F_SIGNATURE, Val::Sig(signature.clone()));
        }
    });
    m.pad(8);
    m.buf.extend_from_slice(&b.buf);
    m.buf
}

/// A decoded message. Absent header fields read as empty strings, which is what every call site
/// wants: an empty interface matches nothing and falls through to the unknown-method reply.
#[derive(Default)]
struct Msg {
    kind: u8,
    flags: u8,
    serial: u32,
    reply_serial: u32,
    path: String,
    interface: String,
    member: String,
    sender: String,
    signature: String,
    body: Vec<u8>,
    le: bool,
}

impl Msg {
    /// Decoded body arguments, or nothing at all if the body does not match its own signature.
    /// A malformed call is answered as if it had no arguments, never by panicking.
    fn args(&self) -> Vec<Val> {
        Dec::new(&self.body, self.le)
            .values(&self.signature)
            .unwrap_or_default()
    }

    fn arg(&self, n: usize) -> Option<Val> {
        self.args().into_iter().nth(n)
    }
}

/// Total length of the frame at the front of `buf`, or `None` while the fixed header is still
/// arriving. Reading this without decoding anything is what lets the socket loop use a plain
/// read timeout instead of a message-boundary-aware one.
fn frame_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < 16 {
        return None;
    }
    let le = buf[0] == b'l';
    let read = |at: usize| -> u32 {
        let b = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let body = read(4) as usize;
    let fields = read(12) as usize;
    Some((16 + fields).next_multiple_of(8) + body)
}

fn parse(frame: &[u8]) -> Option<Msg> {
    let le = match frame.first()? {
        b'l' => true,
        b'B' => false,
        _ => return None,
    };
    let mut m = Msg {
        kind: *frame.get(1)?,
        flags: *frame.get(2)?,
        le,
        ..Msg::default()
    };
    let mut d = Dec::new(frame, le);
    d.at = 4;
    let body_len = d.u32()? as usize;
    m.serial = d.u32()?;
    let Val::Array(_, fields) = d.value("a(yv)")? else {
        return None;
    };
    for f in fields {
        let Val::Struct(pair) = f else { continue };
        let (Some(Val::Byte(code)), Some(Val::Variant(v))) = (pair.first(), pair.get(1)) else {
            continue;
        };
        match (*code, v.as_ref()) {
            (F_PATH, Val::Path(s)) => m.path = s.clone(),
            (F_INTERFACE, Val::Str(s)) => m.interface = s.clone(),
            (F_MEMBER, Val::Str(s)) => m.member = s.clone(),
            (F_REPLY_SERIAL, Val::U32(s)) => m.reply_serial = *s,
            (F_SENDER, Val::Str(s)) => m.sender = s.clone(),
            (F_SIGNATURE, Val::Sig(s)) => m.signature = s.clone(),
            _ => {}
        }
    }
    d.pad(8)?;
    m.body = frame.get(d.at..d.at + body_len)?.to_vec();
    Some(m)
}

// ---------------------------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------------------------

/// Undo the `%xx` escaping D-Bus addresses use, so a runtime directory with an unusual character
/// in it still resolves. Anything that is not a valid escape is passed through unchanged, which is
/// what every other implementation does with a malformed address.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where the session bus lives, as either a filesystem path or an abstract socket name.
enum BusAddr {
    Path(String),
    Abstract(String),
}

/// Pick the first understandable `unix:` address out of `DBUS_SESSION_BUS_ADDRESS`.
///
/// The variable is a semicolon-separated list of alternatives and may name transports this module
/// has no business speaking (`tcp:`, `nonce-tcp:`), so unknown entries are skipped, not refused.
/// With no variable at all the systemd-user default is worth one try: a player started from a
/// non-login shell or a service file often has an empty environment and a perfectly good bus.
fn bus_address() -> Option<BusAddr> {
    if let Ok(list) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        for entry in list.split(';') {
            let Some((transport, args)) = entry.split_once(':') else {
                continue;
            };
            if transport != "unix" {
                continue;
            }
            for kv in args.split(',') {
                match kv.split_once('=') {
                    Some(("path", v)) => return Some(BusAddr::Path(unescape(v))),
                    Some(("abstract", v)) => return Some(BusAddr::Abstract(unescape(v))),
                    _ => {}
                }
            }
        }
    }
    let fallback = format!("/run/user/{}/bus", uid()?);
    std::fs::metadata(&fallback)
        .ok()
        .map(|_| BusAddr::Path(fallback))
}

/// The process's real user id, needed as hex for SASL EXTERNAL.
///
/// `getuid` is one libc call away and this crate links no libc, so it is read off the kernel
/// instead: the owner of `/proc/self` is the process's real uid by definition. `$HOME` is the
/// fallback for the rare mount namespace without `/proc`.
fn uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    if let Ok(md) = std::fs::metadata("/proc/self") {
        return Some(md.uid());
    }
    let home = std::env::var("HOME").ok()?;
    std::fs::metadata(home).ok().map(|md| md.uid())
}

fn connect(addr: &BusAddr) -> Option<UnixStream> {
    match addr {
        BusAddr::Path(p) => UnixStream::connect(p).ok(),
        BusAddr::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt;
            let sa = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).ok()?;
            UnixStream::connect_addr(&sa).ok()
        }
    }
}

/// Read one `\r\n`-terminated SASL line, leaving anything past it in `rx`.
///
/// The auth phase is line based and the binary phase is not, so the leftovers matter: a bus that
/// pipelines its greeting with the first message would otherwise lose that message.
fn read_line(stream: &mut UnixStream, rx: &mut Vec<u8>) -> Option<String> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut chunk = [0u8; 512];
    loop {
        if let Some(at) = rx.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8(rx[..at].to_vec()).ok();
            rx.drain(..at + 2);
            return line;
        }
        if Instant::now() >= deadline || rx.len() > 4096 {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => rx.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// SASL EXTERNAL: the leading NUL, the uid as hex, then straight to `BEGIN`.
///
/// The NUL is not padding - it is the byte that carries the process credentials over
/// `SO_PASSCRED`, and the kernel is what tells the daemon who is calling; the hex uid only has to
/// agree with it.
/// `NEGOTIATE_UNIX_FD` is skipped deliberately: MPRIS passes no file descriptors, so the round trip
/// buys nothing and adds a reply to get wrong.
fn authenticate(stream: &mut UnixStream, rx: &mut Vec<u8>) -> Option<()> {
    let uid = uid()?;
    let hex: String = uid
        .to_string()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    stream.write_all(b"\0").ok()?;
    stream
        .write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes())
        .ok()?;
    let reply = read_line(stream, rx)?;
    if !reply.starts_with("OK") {
        return None;
    }
    stream.write_all(b"BEGIN\r\n").ok()?;
    Some(())
}

// ---------------------------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------------------------

/// What the player is doing right now, borrowed straight from whatever the UI already holds.
///
/// Every field is a copy or a borrow of state the render loop has to hand anyway, so building one
/// of these per frame costs nothing and the caller never has to track what MPRIS was last told.
pub struct NowPlaying<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    /// `file://` or `http(s)://`. Anything else is dropped rather than shown, because a shell that
    /// cannot fetch it will draw a broken thumbnail instead of falling back to its own icon.
    pub art_url: Option<&'a str>,
    pub position_us: i64,
    /// Zero while the length is still unknown, which is the normal state of a stream that has only
    /// just started resolving.
    pub length_us: i64,
    pub playing: bool,
    /// Nothing loaded at all; reported as `Stopped` with empty metadata.
    pub idle: bool,
    pub can_next: bool,
    pub can_prev: bool,
    /// Linear 0.0..=1.0, which is what MPRIS means by volume. Anything outside is clamped.
    pub volume: f64,
    /// Bumped by the caller whenever the track changes. It is the only way this module can tell a
    /// new track from a seek in an old one, since titles repeat and positions rewind.
    pub track_seq: u64,
}

/// Something the desktop asked the player to do.
#[derive(Clone, Debug, PartialEq)]
pub enum MprisCommand {
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Relative, in microseconds, and signed: a negative value seeks backwards.
    Seek(i64),
    /// Absolute, in microseconds, already checked against the track the caller meant.
    SetPosition(i64),
    /// Linear 0.0..=1.0, already clamped.
    SetVolume(f64),
    OpenUri(String),
    Raise,
    Quit,
}

/// Owned copy of a `NowPlaying`, stamped with the moment the UI produced it.
///
/// The timestamp is what makes `Position` honest between publishes and what separates a seek from
/// ordinary playback drift: without it, a slow frame looks exactly like a small jump.
#[derive(Clone)]
struct Snapshot {
    title: String,
    artist: String,
    album: String,
    art_url: Option<String>,
    position_us: i64,
    length_us: i64,
    playing: bool,
    idle: bool,
    can_next: bool,
    can_prev: bool,
    volume: f64,
    track_seq: u64,
    at: Instant,
}

impl Snapshot {
    /// The state the bus is told about before the player has published anything.
    fn idle() -> Snapshot {
        Snapshot {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            art_url: None,
            position_us: 0,
            length_us: 0,
            playing: false,
            idle: true,
            can_next: false,
            can_prev: false,
            volume: 1.0,
            track_seq: 0,
            at: Instant::now(),
        }
    }

    fn from(now: &NowPlaying) -> Snapshot {
        Snapshot {
            title: now.title.to_string(),
            artist: now.artist.to_string(),
            album: now.album.to_string(),
            art_url: now
                .art_url
                .filter(|u| {
                    u.starts_with("file://")
                        || u.starts_with("http://")
                        || u.starts_with("https://")
                })
                .map(str::to_string),
            position_us: now.position_us.max(0),
            length_us: now.length_us.max(0),
            playing: now.playing && !now.idle,
            idle: now.idle,
            can_next: now.can_next,
            can_prev: now.can_prev,
            volume: now.volume.clamp(0.0, 1.0),
            track_seq: now.track_seq,
            at: Instant::now(),
        }
    }

    fn status(&self) -> &'static str {
        if self.idle {
            "Stopped"
        } else if self.playing {
            "Playing"
        } else {
            "Paused"
        }
    }

    /// A distinct object path per track. Shells key their "now playing" popups off this, so it has
    /// to change exactly when the track does - hence the caller-supplied sequence number rather
    /// than a hash of the title, which would collide across a repeat of the same song.
    fn track_id(&self) -> String {
        format!("/org/ytmplayer/track/{}", self.track_seq)
    }

    /// Position extrapolated to now. mpv is the authority, but the UI only publishes a few dozen
    /// times a second and a client polling in between should not see the clock stand still.
    fn position_now(&self) -> i64 {
        if !self.playing {
            return self.position_us;
        }
        let elapsed = self.at.elapsed().as_micros().min(i64::MAX as u128) as i64;
        let pos = self.position_us.saturating_add(elapsed);
        if self.length_us > 0 {
            pos.min(self.length_us)
        } else {
            pos
        }
    }

    fn metadata(&self) -> Val {
        if self.idle {
            return Val::Dict(Vec::new());
        }
        let mut d = vec![("mpris:trackid".to_string(), Val::Path(self.track_id()))];
        if self.length_us > 0 {
            d.push(("mpris:length".to_string(), Val::I64(self.length_us)));
        }
        if let Some(art) = &self.art_url {
            d.push(("mpris:artUrl".to_string(), Val::Str(art.clone())));
        }
        d.push(("xesam:title".to_string(), Val::Str(self.title.clone())));
        // Artist is a list in xesam even when there is one of them, and clients that ask for the
        // first element get an error rather than a string if it is sent as `s`.
        d.push((
            "xesam:artist".to_string(),
            Val::Array("s".to_string(), vec![Val::Str(self.artist.clone())]),
        ));
        d.push(("xesam:album".to_string(), Val::Str(self.album.clone())));
        Val::Dict(d)
    }
}

/// Shared between the UI thread and the bus thread. Two short-lived locks and a flag: the render
/// path must never wait on the socket, so nothing here is ever held across I/O.
struct Shared {
    /// Latest published state, overwritten in place. Older snapshots are worthless, so a UI that
    /// outruns the bus thread simply loses the frames in between.
    pending: Mutex<Option<Snapshot>>,
    commands: Mutex<Vec<MprisCommand>>,
    /// Cleared by `Drop` to stop the thread, and by the thread when the bus goes away. Both mean
    /// the same thing to the caller: nothing more will happen.
    live: AtomicBool,
}

/// A live MPRIS registration. Dropping it takes the name off the bus.
pub struct Mpris {
    shared: Arc<Shared>,
}

impl Mpris {
    /// Connect, authenticate and claim the bus name, then hand the socket to a background thread.
    ///
    /// `None` covers every way this can fail - no session bus, a refused handshake, a name that
    /// cannot be owned - because none of them are worth reporting to someone who only wanted to
    /// play music. The player runs identically without it.
    pub fn start() -> Option<Mpris> {
        let addr = bus_address()?;
        Mpris::start_on(connect(&addr)?)
    }

    /// The half of `start` that does not care where the socket came from, so the tests can drive a
    /// whole session over a `UnixStream::pair` instead of a real daemon.
    fn start_on(mut stream: UnixStream) -> Option<Mpris> {
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok()?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok()?;

        let mut rx = Vec::new();
        authenticate(&mut stream, &mut rx)?;

        let shared = Arc::new(Shared {
            pending: Mutex::new(None),
            commands: Mutex::new(Vec::new()),
            live: AtomicBool::new(true),
        });
        let mut conn = Conn {
            stream,
            rx,
            tx: Vec::new(),
            serial: 0,
            seen: false,
            state: Snapshot::idle(),
            shared: shared.clone(),
        };
        conn.hello()?;
        conn.request_name()?;
        conn.stream.set_read_timeout(Some(TICK)).ok()?;

        std::thread::Builder::new()
            .name("mpris".to_string())
            .spawn(move || conn.run())
            .ok()?;
        Some(Mpris { shared })
    }

    /// Hand the current state to the bus thread. Takes one uncontended lock and returns; the
    /// diffing, the marshalling and the socket write all happen somewhere else.
    pub fn publish(&self, now: &NowPlaying) {
        if !self.shared.live.load(Ordering::Relaxed) {
            return;
        }
        *lock(&self.shared.pending) = Some(Snapshot::from(now));
    }

    /// Everything the desktop asked for since the last call, oldest first.
    ///
    /// Still drains after the bus has gone: a command that arrived is a command the user pressed,
    /// and losing the last one because the daemon died a moment later would be the wrong kind of
    /// tidy. Nothing refills the queue once the thread is gone, so it empties and stays empty.
    pub fn take_commands(&self) -> Vec<MprisCommand> {
        std::mem::take(&mut *lock(&self.shared.commands))
    }
}

impl Drop for Mpris {
    /// The thread owns the socket, so releasing the name means telling it to stop and letting the
    /// close do the talking. It notices within one tick; nothing waits for it, because a process
    /// on its way out has better things to do than join a thread parked on a read.
    fn drop(&mut self) {
        self.shared.live.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

const ERR_UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const ERR_UNKNOWN_PROPERTY: &str = "org.freedesktop.DBus.Error.UnknownProperty";
const ERR_UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
const ERR_INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const ERR_READ_ONLY: &str = "org.freedesktop.DBus.Error.PropertyReadOnly";
/// `RequestName`: do not queue behind an existing owner, fail immediately so the caller can pick
/// another name instead of silently owning nothing.
const NAME_FLAG_DO_NOT_QUEUE: u32 = 4;
const NAME_REPLY_PRIMARY_OWNER: u32 = 1;
const NAME_REPLY_ALREADY_OWNER: u32 = 4;

const XML_HEADER: &str = concat!(
    "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\"\n",
    " \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n"
);

/// Introspection data for the one interesting object. Written out rather than generated: it is the
/// contract clients read before they call anything, so it should be inspectable as text, and a
/// generator for a fixed document would be more code than the document.
const XML_PLAYER: &str = r#"<node>
 <interface name="org.freedesktop.DBus.Introspectable">
  <method name="Introspect"><arg name="xml_data" type="s" direction="out"/></method>
 </interface>
 <interface name="org.freedesktop.DBus.Peer">
  <method name="Ping"/>
  <method name="GetMachineId"><arg name="machine_uuid" type="s" direction="out"/></method>
 </interface>
 <interface name="org.freedesktop.DBus.Properties">
  <method name="Get">
   <arg name="interface_name" type="s" direction="in"/>
   <arg name="property_name" type="s" direction="in"/>
   <arg name="value" type="v" direction="out"/>
  </method>
  <method name="GetAll">
   <arg name="interface_name" type="s" direction="in"/>
   <arg name="properties" type="a{sv}" direction="out"/>
  </method>
  <method name="Set">
   <arg name="interface_name" type="s" direction="in"/>
   <arg name="property_name" type="s" direction="in"/>
   <arg name="value" type="v" direction="in"/>
  </method>
  <signal name="PropertiesChanged">
   <arg name="interface_name" type="s"/>
   <arg name="changed_properties" type="a{sv}"/>
   <arg name="invalidated_properties" type="as"/>
  </signal>
 </interface>
 <interface name="org.mpris.MediaPlayer2">
  <method name="Raise"/>
  <method name="Quit"/>
  <property name="CanQuit" type="b" access="read"/>
  <property name="CanRaise" type="b" access="read"/>
  <property name="HasTrackList" type="b" access="read"/>
  <property name="Identity" type="s" access="read"/>
  <property name="DesktopEntry" type="s" access="read"/>
  <property name="SupportedUriSchemes" type="as" access="read"/>
  <property name="SupportedMimeTypes" type="as" access="read"/>
 </interface>
 <interface name="org.mpris.MediaPlayer2.Player">
  <method name="Next"/>
  <method name="Previous"/>
  <method name="Pause"/>
  <method name="PlayPause"/>
  <method name="Stop"/>
  <method name="Play"/>
  <method name="Seek"><arg name="Offset" type="x" direction="in"/></method>
  <method name="SetPosition">
   <arg name="TrackId" type="o" direction="in"/>
   <arg name="Position" type="x" direction="in"/>
  </method>
  <method name="OpenUri"><arg name="Uri" type="s" direction="in"/></method>
  <signal name="Seeked"><arg name="Position" type="x"/></signal>
  <property name="PlaybackStatus" type="s" access="read"/>
  <property name="Rate" type="d" access="readwrite"/>
  <property name="Metadata" type="a{sv}" access="read"/>
  <property name="Volume" type="d" access="readwrite"/>
  <property name="Position" type="x" access="read">
   <annotation name="org.freedesktop.DBus.Property.EmitsChangedSignal" value="false"/>
  </property>
  <property name="MinimumRate" type="d" access="read"/>
  <property name="MaximumRate" type="d" access="read"/>
  <property name="CanGoNext" type="b" access="read"/>
  <property name="CanGoPrevious" type="b" access="read"/>
  <property name="CanPlay" type="b" access="read"/>
  <property name="CanPause" type="b" access="read"/>
  <property name="CanSeek" type="b" access="read"/>
  <property name="CanControl" type="b" access="read">
   <annotation name="org.freedesktop.DBus.Property.EmitsChangedSignal" value="false"/>
  </property>
 </interface>
</node>
"#;

/// Introspection for any path, so `busctl tree` can walk down to the object instead of finding an
/// empty root and stopping there.
fn introspect(path: &str) -> String {
    let child = match path {
        "/" => "org",
        "/org" => "mpris",
        "/org/mpris" => "MediaPlayer2",
        OBJ_PATH => return format!("{XML_HEADER}{XML_PLAYER}"),
        _ => return format!("{XML_HEADER}<node/>\n"),
    };
    format!("{XML_HEADER}<node>\n <node name=\"{child}\"/>\n</node>\n")
}

/// The host's D-Bus machine id. Only `Peer.GetMachineId` wants it, and only some tools ask, so a
/// missing file is answered with zeroes rather than an error - the value is advisory.
fn machine_id() -> String {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let id = s.trim();
            if !id.is_empty() {
                return id.to_string();
            }
        }
    }
    "0".repeat(32)
}

/// Numeric variants coerced to a double, because clients are not consistent about sending `d` for
/// a volume, and rejecting an integer would only look like a bug at this end.
fn as_f64(v: &Val) -> Option<f64> {
    Some(match v {
        Val::F64(x) => *x,
        Val::I32(x) => f64::from(*x),
        Val::U32(x) => f64::from(*x),
        Val::I64(x) => *x as f64,
        Val::U64(x) => *x as f64,
        _ => return None,
    })
}

fn as_i64(v: &Val) -> Option<i64> {
    Some(match v {
        Val::I64(x) => *x,
        Val::U64(x) => *x as i64,
        Val::I32(x) => i64::from(*x),
        Val::U32(x) => i64::from(*x),
        _ => return None,
    })
}

/// Outcome of looking for a whole message at the front of the receive buffer.
enum Framed {
    Ready(Msg),
    /// Not all of it has arrived yet; read more.
    Need,
    /// The stream no longer makes sense and cannot be resynchronised.
    Broken,
}

/// The bus connection and everything that hangs off it. Lives entirely on the bus thread once
/// `start` has handed it over, so nothing here needs a lock except the two shared slots.
struct Conn {
    stream: UnixStream,
    rx: Vec<u8>,
    tx: Vec<u8>,
    serial: u32,
    /// Whether a real snapshot has arrived yet. Until it has, a position that is not zero is the
    /// player starting up, not somebody seeking.
    seen: bool,
    state: Snapshot,
    shared: Arc<Shared>,
}

impl Conn {
    /// Serials identify replies and must never be zero; the wrap is unreachable in practice but
    /// costs one comparison to make impossible.
    fn emit(&mut self, head: &Head, body: &[Val]) {
        self.serial = self.serial.wrapping_add(1);
        if self.serial == 0 {
            self.serial = 1;
        }
        let bytes = build(head, self.serial, body);
        self.tx.extend_from_slice(&bytes);
    }

    fn reply(&mut self, m: &Msg, body: &[Val]) {
        if m.flags & FLAG_NO_REPLY != 0 {
            return;
        }
        let dest = (!m.sender.is_empty()).then_some(m.sender.as_str());
        self.emit(
            &Head {
                kind: MSG_METHOD_RETURN,
                reply_serial: Some(m.serial),
                destination: dest,
                ..Head::default()
            },
            body,
        );
    }

    fn error(&mut self, m: &Msg, name: &str, text: &str) {
        if m.flags & FLAG_NO_REPLY != 0 {
            return;
        }
        let dest = (!m.sender.is_empty()).then_some(m.sender.as_str());
        self.emit(
            &Head {
                kind: MSG_ERROR,
                error_name: Some(name),
                reply_serial: Some(m.serial),
                destination: dest,
                ..Head::default()
            },
            &[Val::Str(text.to_string())],
        );
    }

    fn signal(&mut self, interface: &str, member: &str, body: &[Val]) {
        self.emit(
            &Head {
                kind: MSG_SIGNAL,
                path: Some(OBJ_PATH),
                interface: Some(interface),
                member: Some(member),
                ..Head::default()
            },
            body,
        );
    }

    fn push(&self, c: MprisCommand) {
        let mut q = lock(&self.shared.commands);
        if q.len() < MAX_PENDING_COMMANDS {
            q.push(c);
        }
    }

    /// One read into the buffer. A timeout is not an error - it is the loop's idle tick.
    fn fill(&mut self) -> Option<()> {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => None,
            Ok(n) => {
                self.rx.extend_from_slice(&chunk[..n]);
                Some(())
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted => Some(()),
                _ => None,
            },
        }
    }

    fn flush(&mut self) -> Option<()> {
        if self.tx.is_empty() {
            return Some(());
        }
        let out = std::mem::take(&mut self.tx);
        self.stream.write_all(&out).ok()
    }

    fn take_frame(&mut self) -> Framed {
        let Some(total) = frame_len(&self.rx) else {
            return Framed::Need;
        };
        if total > MAX_MESSAGE {
            return Framed::Broken;
        }
        if self.rx.len() < total {
            return Framed::Need;
        }
        let frame: Vec<u8> = self.rx.drain(..total).collect();
        // A frame that will not decode is fatal: the length was believable, so the stream is still
        // aligned, but nothing good comes of guessing what the sender meant.
        match parse(&frame) {
            Some(m) => Framed::Ready(m),
            None => Framed::Broken,
        }
    }

    fn read_frame(&mut self, deadline: Instant) -> Option<Msg> {
        loop {
            match self.take_frame() {
                Framed::Ready(m) => return Some(m),
                Framed::Broken => return None,
                Framed::Need => {}
            }
            if Instant::now() >= deadline {
                return None;
            }
            self.fill()?;
        }
    }

    /// A blocking call to the bus daemon itself. Only used during setup, where there is nothing
    /// else to do anyway and no UI waiting on the answer.
    fn call_daemon(&mut self, member: &str, body: &[Val]) -> Option<Msg> {
        self.emit(
            &Head {
                kind: MSG_METHOD_CALL,
                path: Some("/org/freedesktop/DBus"),
                interface: Some("org.freedesktop.DBus"),
                member: Some(member),
                destination: Some("org.freedesktop.DBus"),
                ..Head::default()
            },
            body,
        );
        let want = self.serial;
        self.flush()?;
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            let m = self.read_frame(deadline)?;
            // Signals arrive in here too - NameAcquired turns up before the reply does.
            if m.reply_serial == want && matches!(m.kind, MSG_METHOD_RETURN | MSG_ERROR) {
                return Some(m);
            }
        }
    }

    /// Say hello, which is what turns a socket into a bus connection with a unique name. Nothing
    /// uses the name it returns; the point is that the daemon accepted the connection.
    fn hello(&mut self) -> Option<()> {
        let m = self.call_daemon("Hello", &[])?;
        (m.kind == MSG_METHOD_RETURN).then_some(())
    }

    /// Claim the well-known name, falling back to the per-instance form the specification reserves
    /// for exactly this case: a second copy of the player must still be visible, not invisible.
    fn request_name(&mut self) -> Option<()> {
        let candidates = [
            BUS_NAME.to_string(),
            format!("{BUS_NAME}.instance{}", std::process::id()),
        ];
        for name in candidates {
            let reply = self.call_daemon(
                "RequestName",
                &[Val::Str(name.clone()), Val::U32(NAME_FLAG_DO_NOT_QUEUE)],
            )?;
            if let Some(Val::U32(code)) = reply.arg(0)
                && (code == NAME_REPLY_PRIMARY_OWNER || code == NAME_REPLY_ALREADY_OWNER)
            {
                return Some(());
            }
        }
        None
    }

    fn run(mut self) {
        while self.shared.live.load(Ordering::Relaxed) {
            if self.tick().is_none() {
                break;
            }
        }
        // Whether the bus went away or the player did, callers get the same answer from here on:
        // nothing. Dropping the socket releases the name.
        self.shared.live.store(false, Ordering::Relaxed);
    }

    fn tick(&mut self) -> Option<()> {
        self.fill()?;
        loop {
            match self.take_frame() {
                Framed::Ready(m) => self.dispatch(&m),
                Framed::Need => break,
                Framed::Broken => return None,
            }
        }
        self.apply_pending();
        self.flush()
    }

    fn dispatch(&mut self, m: &Msg) {
        if m.kind != MSG_METHOD_CALL {
            return;
        }
        // Introspection answers for every path so the tree can be walked; everything else exists
        // only on the player object.
        if m.interface == IFACE_INTROSPECT && m.member == "Introspect" {
            let xml = introspect(&m.path);
            return self.reply(m, &[Val::Str(xml)]);
        }
        if m.interface == IFACE_PEER {
            return match m.member.as_str() {
                "Ping" => self.reply(m, &[]),
                "GetMachineId" => self.reply(m, &[Val::Str(machine_id())]),
                _ => self.error(m, ERR_UNKNOWN_METHOD, "no such method on Peer"),
            };
        }
        if m.path != OBJ_PATH {
            return self.error(m, ERR_UNKNOWN_OBJECT, "only /org/mpris/MediaPlayer2 exists");
        }
        match m.interface.as_str() {
            IFACE_PROPS => self.properties(m),
            IFACE_ROOT => match m.member.as_str() {
                "Raise" => {
                    self.push(MprisCommand::Raise);
                    self.reply(m, &[]);
                }
                "Quit" => {
                    self.push(MprisCommand::Quit);
                    self.reply(m, &[]);
                }
                _ => self.error(m, ERR_UNKNOWN_METHOD, "no such method"),
            },
            IFACE_PLAYER => self.player_method(m),
            _ => self.error(m, ERR_UNKNOWN_METHOD, "no such interface"),
        }
    }

    fn player_method(&mut self, m: &Msg) {
        let args = m.args();
        let cmd = match m.member.as_str() {
            "PlayPause" => Some(MprisCommand::PlayPause),
            "Play" => Some(MprisCommand::Play),
            "Pause" => Some(MprisCommand::Pause),
            "Stop" => Some(MprisCommand::Stop),
            "Next" => Some(MprisCommand::Next),
            "Previous" => Some(MprisCommand::Previous),
            "Seek" => match args.first().and_then(as_i64) {
                Some(us) => Some(MprisCommand::Seek(us)),
                None => return self.error(m, ERR_INVALID_ARGS, "Seek takes (x)"),
            },
            "SetPosition" => match (args.first(), args.get(1).and_then(as_i64)) {
                (Some(Val::Path(track)), Some(us)) => {
                    // The track id guards against a stale client seeking the song that just ended.
                    // A mismatch is specified as a no-op, not an error.
                    set_position(&self.state, track, us)
                }
                _ => return self.error(m, ERR_INVALID_ARGS, "SetPosition takes (ox)"),
            },
            "OpenUri" => match args.first() {
                Some(Val::Str(uri)) => Some(MprisCommand::OpenUri(uri.clone())),
                _ => return self.error(m, ERR_INVALID_ARGS, "OpenUri takes (s)"),
            },
            _ => return self.error(m, ERR_UNKNOWN_METHOD, "no such method"),
        };
        if let Some(c) = cmd {
            self.push(c);
        }
        self.reply(m, &[]);
    }

    fn properties(&mut self, m: &Msg) {
        let args = m.args();
        match m.member.as_str() {
            "Get" => match (args.first(), args.get(1)) {
                (Some(Val::Str(iface)), Some(Val::Str(prop))) => match self.property(iface, prop) {
                    Some(v) => self.reply(m, &[Val::Variant(Box::new(v))]),
                    None => self.error(m, ERR_UNKNOWN_PROPERTY, prop),
                },
                _ => self.error(m, ERR_INVALID_ARGS, "Get takes (ss)"),
            },
            "GetAll" => match args.first() {
                Some(Val::Str(iface)) => {
                    let all = self.all_properties(iface);
                    self.reply(m, &[Val::Dict(all)]);
                }
                _ => self.error(m, ERR_INVALID_ARGS, "GetAll takes (s)"),
            },
            "Set" => match (args.first(), args.get(1), args.get(2)) {
                (Some(Val::Str(iface)), Some(Val::Str(prop)), Some(Val::Variant(v))) => {
                    self.set_property(m, iface, prop, v);
                }
                _ => self.error(m, ERR_INVALID_ARGS, "Set takes (ssv)"),
            },
            _ => self.error(m, ERR_UNKNOWN_METHOD, "no such method on Properties"),
        }
    }

    fn set_property(&mut self, m: &Msg, iface: &str, prop: &str, v: &Val) {
        if iface != IFACE_PLAYER {
            return self.error(m, ERR_UNKNOWN_PROPERTY, prop);
        }
        match prop {
            "Volume" => match as_f64(v) {
                Some(x) => {
                    self.push(MprisCommand::SetVolume(x.clamp(0.0, 1.0)));
                    self.reply(m, &[]);
                }
                None => self.error(m, ERR_INVALID_ARGS, "Volume takes a double"),
            },
            // Rate is advertised writable because clients expect the property to exist, but the
            // range is pinned at 1.0: mpv could resample, the UI has no control for it, and
            // silently accepting the write is friendlier than an error nobody handles.
            "Rate" => self.reply(m, &[]),
            "Position" => self.error(m, ERR_READ_ONLY, "Position is set with SetPosition"),
            _ => self.error(m, ERR_UNKNOWN_PROPERTY, prop),
        }
    }

    fn property(&self, iface: &str, prop: &str) -> Option<Val> {
        let s = &self.state;
        Some(match (iface, prop) {
            (IFACE_ROOT, "CanQuit") => Val::Bool(true),
            (IFACE_ROOT, "CanRaise") => Val::Bool(true),
            (IFACE_ROOT, "HasTrackList") => Val::Bool(false),
            (IFACE_ROOT, "Identity") => Val::Str("YTM-Player".to_string()),
            (IFACE_ROOT, "DesktopEntry") => Val::Str("ytmplayer".to_string()),
            (IFACE_ROOT, "SupportedUriSchemes") => strings(&["http", "https", "file"]),
            (IFACE_ROOT, "SupportedMimeTypes") => strings(&[
                "audio/mpeg",
                "audio/mp4",
                "audio/ogg",
                "audio/flac",
                "audio/wav",
                "audio/webm",
                "video/mp4",
                "video/webm",
            ]),
            (IFACE_PLAYER, "PlaybackStatus") => Val::Str(s.status().to_string()),
            (IFACE_PLAYER, "Metadata") => s.metadata(),
            (IFACE_PLAYER, "Position") => Val::I64(s.position_now()),
            (IFACE_PLAYER, "Volume") => Val::F64(s.volume),
            (IFACE_PLAYER, "Rate" | "MinimumRate" | "MaximumRate") => Val::F64(1.0),
            (IFACE_PLAYER, "CanGoNext") => Val::Bool(s.can_next),
            (IFACE_PLAYER, "CanGoPrevious") => Val::Bool(s.can_prev),
            (IFACE_PLAYER, "CanPlay" | "CanPause") => Val::Bool(!s.idle),
            (IFACE_PLAYER, "CanSeek") => Val::Bool(!s.idle && s.length_us > 0),
            // Control is about the interface being live at all, not about there being a track.
            (IFACE_PLAYER, "CanControl") => Val::Bool(true),
            _ => return None,
        })
    }

    /// Every property of an interface at once.
    ///
    /// An interface this object does not implement answers with an empty dictionary, not an error,
    /// because `busctl introspect` calls this for each interface in the XML - including the three
    /// standard ones that have no properties - and prints an error line for every refusal.
    fn all_properties(&self, iface: &str) -> Vec<(String, Val)> {
        let names: &[&str] = match iface {
            IFACE_ROOT => &[
                "CanQuit",
                "CanRaise",
                "HasTrackList",
                "Identity",
                "DesktopEntry",
                "SupportedUriSchemes",
                "SupportedMimeTypes",
            ],
            IFACE_PLAYER => &[
                "PlaybackStatus",
                "Metadata",
                "Position",
                "Volume",
                "Rate",
                "MinimumRate",
                "MaximumRate",
                "CanGoNext",
                "CanGoPrevious",
                "CanPlay",
                "CanPause",
                "CanSeek",
                "CanControl",
            ],
            _ => &[],
        };
        names
            .iter()
            .filter_map(|n| self.property(iface, n).map(|v| ((*n).to_string(), v)))
            .collect()
    }

    /// Adopt the newest snapshot and tell the bus what actually moved.
    ///
    /// Everything here is a comparison against what was last *sent*, not against the previous
    /// frame: the UI publishes continuously, and a shell that redraws its panel on every
    /// `PropertiesChanged` would otherwise spin at the frame rate for no reason.
    fn apply_pending(&mut self) {
        let Some(next) = lock(&self.shared.pending).take() else {
            return;
        };
        let prev = std::mem::replace(&mut self.state, next);
        let now = &self.state;

        let mut changed: Vec<(String, Val)> = Vec::new();
        if prev.status() != now.status() {
            changed.push((
                "PlaybackStatus".to_string(),
                Val::Str(now.status().to_string()),
            ));
        }
        if metadata_differs(&prev, now) {
            changed.push(("Metadata".to_string(), now.metadata()));
        }
        // Volume arrives as a float from mpv and jitters in the last bits; an exact comparison
        // would emit a signal on every publish.
        if (prev.volume - now.volume).abs() > 1e-4 {
            changed.push(("Volume".to_string(), Val::F64(now.volume)));
        }
        if prev.can_next != now.can_next {
            changed.push(("CanGoNext".to_string(), Val::Bool(now.can_next)));
        }
        if prev.can_prev != now.can_prev {
            changed.push(("CanGoPrevious".to_string(), Val::Bool(now.can_prev)));
        }
        if prev.idle != now.idle {
            changed.push(("CanPlay".to_string(), Val::Bool(!now.idle)));
            changed.push(("CanPause".to_string(), Val::Bool(!now.idle)));
        }
        let seekable = |s: &Snapshot| !s.idle && s.length_us > 0;
        if seekable(&prev) != seekable(now) {
            changed.push(("CanSeek".to_string(), Val::Bool(seekable(now))));
        }

        // A discontinuity is a position that is not where playback would have carried it. Clients
        // poll Position rather than being told about it, so this signal is the only way they learn
        // that their progress bar is now wrong.
        let elapsed = now.at.saturating_duration_since(prev.at).as_micros() as i64;
        let expected = if prev.playing {
            prev.position_us.saturating_add(elapsed)
        } else {
            prev.position_us
        };
        let jumped = self.seen
            && prev.track_seq == now.track_seq
            && (now.position_us - expected).abs() > SEEK_EPSILON_US;
        let position = now.position_us;
        self.seen = true;

        if !changed.is_empty() {
            self.signal(
                IFACE_PROPS,
                "PropertiesChanged",
                &[
                    Val::Str(IFACE_PLAYER.to_string()),
                    Val::Dict(changed),
                    Val::Array("s".to_string(), Vec::new()),
                ],
            );
        }
        if jumped {
            self.signal(IFACE_PLAYER, "Seeked", &[Val::I64(position)]);
        }
    }
}

/// Whether anything the metadata dictionary is built from has moved. Comparing the built
/// dictionaries would allocate a dozen strings per frame to answer "no" almost every time.
fn metadata_differs(a: &Snapshot, b: &Snapshot) -> bool {
    a.track_seq != b.track_seq
        || a.idle != b.idle
        || a.length_us != b.length_us
        || a.title != b.title
        || a.artist != b.artist
        || a.album != b.album
        || a.art_url != b.art_url
}

/// Absolute seek, filtered the way the specification asks: a wrong track id or an out-of-range
/// position is dropped on the floor rather than answered with an error.
fn set_position(state: &Snapshot, track: &str, us: i64) -> Option<MprisCommand> {
    if track != state.track_id() || us < 0 {
        return None;
    }
    if state.length_us > 0 && us > state.length_us {
        return None;
    }
    Some(MprisCommand::SetPosition(us))
}

fn strings(items: &[&str]) -> Val {
    Val::Array(
        "s".to_string(),
        items.iter().map(|s| Val::Str((*s).to_string())).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(v: &Val) -> Vec<u8> {
        let mut e = Enc::default();
        e.value(v);
        e.buf
    }

    /// Encode then decode with the value's own signature. Anything that survives this is at least
    /// self-consistent; the byte-level tests below are what pin it to the specification.
    fn roundtrip(v: &Val) -> Option<Val> {
        let bytes = encode(v);
        let mut d = Dec::new(&bytes, true);
        let out = d.value(&v.signature())?;
        // A decoder that stops short has read the wrong number of pad bytes somewhere.
        (d.at == bytes.len()).then_some(out)
    }

    /// Read one whole message off a socket, or give up. The test's half of the framing, kept as
    /// dumb as possible so a failure points at the module rather than at the harness.
    fn recv(sock: &mut UnixStream, buf: &mut Vec<u8>) -> Option<Msg> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(total) = frame_len(buf)
                && buf.len() >= total
            {
                let frame: Vec<u8> = buf.drain(..total).collect();
                return parse(&frame);
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
        }
    }

    fn recv_line(sock: &mut UnixStream, buf: &mut Vec<u8>) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(at) = buf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&buf[..at]).into_owned();
                buf.drain(..at + 2);
                return Some(line);
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut chunk = [0u8; 256];
            match sock.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
        }
    }

    /// Wait for a particular signal, discarding whatever else turns up on the way.
    fn wait_signal(sock: &mut UnixStream, buf: &mut Vec<u8>, member: &str) -> Option<Msg> {
        for _ in 0..16 {
            let m = recv(sock, buf)?;
            if m.kind == MSG_SIGNAL && m.member == member {
                return Some(m);
            }
        }
        None
    }

    /// Play the daemon's part of a connection: authenticate the client and answer the two calls
    /// every client makes before it can serve anything.
    fn accept_client(sock: &mut UnixStream, buf: &mut Vec<u8>) {
        let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
        let auth = recv_line(sock, buf).expect("auth line");
        assert!(auth.contains("AUTH EXTERNAL"), "{auth}");
        assert!(auth.starts_with('\0'), "the credentials byte comes first");
        sock.write_all(b"OK 0123456789abcdef\r\n").expect("greet");
        assert_eq!(recv_line(sock, buf).as_deref(), Some("BEGIN"));

        let hello = recv(sock, buf).expect("Hello");
        assert_eq!(hello.member, "Hello");
        let reply = build(
            &Head {
                kind: MSG_METHOD_RETURN,
                reply_serial: Some(hello.serial),
                ..Head::default()
            },
            1,
            &[Val::Str(":1.99".into())],
        );
        sock.write_all(&reply).expect("reply");

        let request = recv(sock, buf).expect("RequestName");
        assert_eq!(request.member, "RequestName");
        assert_eq!(request.arg(0), Some(Val::Str(BUS_NAME.into())));
        let reply = build(
            &Head {
                kind: MSG_METHOD_RETURN,
                reply_serial: Some(request.serial),
                ..Head::default()
            },
            2,
            &[Val::U32(NAME_REPLY_PRIMARY_OWNER)],
        );
        sock.write_all(&reply).expect("reply");
    }

    fn track(seq: u64, playing: bool) -> NowPlaying<'static> {
        NowPlaying {
            title: "Blue Monday",
            artist: "New Order",
            album: "Substance",
            art_url: None,
            position_us: 5_000_000,
            length_us: 442_000_000,
            playing,
            idle: false,
            can_next: true,
            can_prev: false,
            volume: 0.5,
            track_seq: seq,
        }
    }

    #[test]
    fn a_whole_session_works_over_a_socket_pair() {
        let (ours, mut bus) = UnixStream::pair().expect("socketpair");
        let daemon = std::thread::spawn(move || {
            let mut buf = Vec::new();
            accept_client(&mut bus, &mut buf);
            // The first publish announces itself; waiting for it makes the rest deterministic.
            let changed = wait_signal(&mut bus, &mut buf, "PropertiesChanged").map(|m| m.args());

            // Ask for a property the way any client would, then read the answer back.
            let call = build(
                &Head {
                    kind: MSG_METHOD_CALL,
                    path: Some(OBJ_PATH),
                    interface: Some(IFACE_PROPS),
                    member: Some("Get"),
                    ..Head::default()
                },
                77,
                &[
                    Val::Str(IFACE_PLAYER.into()),
                    Val::Str("PlaybackStatus".into()),
                ],
            );
            bus.write_all(&call).expect("call");

            let mut status = None;
            for _ in 0..8 {
                let Some(m) = recv(&mut bus, &mut buf) else {
                    break;
                };
                if m.kind == MSG_METHOD_RETURN && m.reply_serial == 77 {
                    status = m.arg(0);
                    break;
                }
            }
            (status, changed)
        });

        let mpris = Mpris::start_on(ours).expect("handshake and name");
        mpris.publish(&track(1, true));
        let (status, changed) = daemon.join().expect("daemon");

        assert_eq!(
            status,
            Some(Val::Variant(Box::new(Val::Str("Playing".into())))),
            "Properties.Get answered with a variant"
        );
        let changed = changed.expect("PropertiesChanged");
        assert_eq!(changed.first(), Some(&Val::Str(IFACE_PLAYER.into())));
        let Some(Val::Dict(props)) = changed.get(1) else {
            panic!("changed properties are a dictionary");
        };
        assert!(props.iter().any(|(k, _)| k == "PlaybackStatus"));
        assert!(props.iter().any(|(k, _)| k == "Metadata"));
        // Position is polled, never signalled; announcing it would wake every panel on the desktop
        // thirty times a second.
        assert!(!props.iter().any(|(k, _)| k == "Position"));
    }

    #[test]
    fn a_method_call_becomes_a_command_and_gets_an_empty_reply() {
        let (ours, mut bus) = UnixStream::pair().expect("socketpair");
        let daemon = std::thread::spawn(move || {
            let mut buf = Vec::new();
            accept_client(&mut bus, &mut buf);
            // SetPosition is checked against the published track id, so the state has to be there
            // before the call is made - the first PropertiesChanged is the proof that it is.
            wait_signal(&mut bus, &mut buf, "PropertiesChanged").expect("first publish");
            for (serial, member, args) in [
                (10u32, "PlayPause", Vec::new()),
                (11, "Seek", vec![Val::I64(-5_000_000)]),
                (
                    12,
                    "SetPosition",
                    vec![
                        Val::Path("/org/ytmplayer/track/1".into()),
                        Val::I64(30_000_000),
                    ],
                ),
                (13, "OpenUri", vec![Val::Str("https://a.invalid/x".into())]),
            ] {
                let call = build(
                    &Head {
                        kind: MSG_METHOD_CALL,
                        path: Some(OBJ_PATH),
                        interface: Some(IFACE_PLAYER),
                        member: Some(member),
                        ..Head::default()
                    },
                    serial,
                    &args,
                );
                bus.write_all(&call).expect("call");
            }
            let mut replies = Vec::new();
            while replies.len() < 4 {
                let Some(m) = recv(&mut bus, &mut buf) else {
                    break;
                };
                if m.kind == MSG_METHOD_RETURN {
                    replies.push(m.reply_serial);
                }
                assert_ne!(m.kind, MSG_ERROR, "{:?}", m.member);
            }
            replies
        });

        let mpris = Mpris::start_on(ours).expect("handshake and name");
        // The track id has to match or the seek is dropped, which is the point of publishing first.
        mpris.publish(&track(1, true));
        let replies = daemon.join().expect("daemon");
        assert_eq!(replies, vec![10, 11, 12, 13], "every call is answered");

        assert_eq!(
            mpris.take_commands(),
            vec![
                MprisCommand::PlayPause,
                MprisCommand::Seek(-5_000_000),
                MprisCommand::SetPosition(30_000_000),
                MprisCommand::OpenUri("https://a.invalid/x".into()),
            ]
        );
        // Draining is destructive; a second call must not repeat the same commands.
        assert!(mpris.take_commands().is_empty());
    }

    #[test]
    fn a_socket_that_says_nothing_is_not_a_bus() {
        let (ours, bus) = UnixStream::pair().expect("socketpair");
        drop(bus);
        assert!(Mpris::start_on(ours).is_none());
    }

    #[test]
    fn a_bus_that_dies_leaves_a_silent_no_op_behind() {
        let (ours, mut bus) = UnixStream::pair().expect("socketpair");
        let daemon = std::thread::spawn(move || {
            let mut buf = Vec::new();
            accept_client(&mut bus, &mut buf);
        });
        let mpris = Mpris::start_on(ours).expect("handshake and name");
        daemon.join().expect("daemon");

        // The daemon's socket is gone. Publishing must stay cheap and quiet rather than blocking
        // the render thread on a write that can never land.
        let start = Instant::now();
        for _ in 0..50 {
            mpris.publish(&track(1, true));
            assert!(mpris.take_commands().is_empty());
        }
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "publish blocked"
        );
    }

    #[test]
    fn strings_carry_a_length_and_a_nul_the_length_ignores() {
        assert_eq!(encode(&Val::Str("ab".into())), b"\x02\x00\x00\x00ab\x00");
        assert_eq!(encode(&Val::Str(String::new())), b"\x00\x00\x00\x00\x00");
        // Signatures are the odd one out: a single byte of length, not four.
        assert_eq!(encode(&Val::Sig("a{sv}".into())), b"\x05a{sv}\x00");
    }

    #[test]
    fn a_struct_pads_its_fields_to_their_own_alignment() {
        // The byte leaves the cursor at 1; the string that follows must start at 4.
        let v = Val::Struct(vec![Val::Byte(1), Val::Str("a".into())]);
        assert_eq!(
            encode(&v),
            b"\x01\x00\x00\x00\x01\x00\x00\x00a\x00",
            "field padding inside a struct"
        );
        assert_eq!(roundtrip(&v), Some(v));
    }

    #[test]
    fn an_eight_byte_value_after_a_byte_is_padded_by_seven() {
        let v = Val::Struct(vec![Val::Byte(0xff), Val::I64(-1)]);
        let bytes = encode(&v);
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[1..8], &[0; 7], "pad from 1 to 8");
        assert_eq!(&bytes[8..], &[0xff; 8]);
    }

    #[test]
    fn an_arrays_padding_sits_outside_its_own_length() {
        // The classic: an empty array of structs is four bytes of zero followed by four bytes of
        // padding, and a reader that counts the padding sees an eight-byte array of nothing.
        let bytes = encode(&Val::Array("(i)".into(), Vec::new()));
        assert_eq!(bytes, vec![0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[..4], &0u32.to_le_bytes());

        let one = encode(&Val::Array(
            "(i)".into(),
            vec![Val::Struct(vec![Val::I32(7)])],
        ));
        assert_eq!(u32::from_le_bytes([one[0], one[1], one[2], one[3]]), 4);
        assert_eq!(one.len(), 12);
        assert_eq!(&one[8..], &7i32.to_le_bytes());
    }

    #[test]
    fn a_dictionary_aligns_every_entry_to_eight() {
        let v = Val::Dict(vec![("k".into(), Val::I64(5))]);
        let bytes = encode(&v);
        // len(4) + pad(4) + "k"(6) + sig "x"(3) + pad(7) + i64(8)
        assert_eq!(bytes.len(), 32);
        assert_eq!(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            24
        );
        assert_eq!(&bytes[4..8], &[0; 4], "pad up to the first dict entry");
        assert_eq!(&bytes[8..14], b"\x01\x00\x00\x00k\x00");
        assert_eq!(&bytes[14..17], b"\x01x\x00");
        assert_eq!(&bytes[17..24], &[0; 7], "pad from 17 to the i64 at 24");
        assert_eq!(&bytes[24..], &5i64.to_le_bytes());
        assert_eq!(roundtrip(&v), Some(v));
    }

    #[test]
    fn a_second_dict_entry_realigns_to_eight() {
        let v = Val::Dict(vec![
            ("a".into(), Val::Str("x".into())),
            ("b".into(), Val::Bool(true)),
        ]);
        let bytes = encode(&v);
        // The first entry runs 8..26: key "a" (6), signature "s" (3), pad to 20, string "x" (6).
        // The second entry therefore starts at 32, not at 26.
        assert_eq!(&bytes[24..26], b"x\x00");
        assert_eq!(&bytes[26..32], &[0; 6], "pad from 26 to the next entry");
        assert_eq!(&bytes[32..38], b"\x01\x00\x00\x00b\x00");
        assert_eq!(roundtrip(&v), Some(v));
    }

    #[test]
    fn a_variant_carries_its_own_signature_and_realigns() {
        let v = Val::Variant(Box::new(Val::F64(0.5)));
        let bytes = encode(&v);
        assert_eq!(&bytes[..3], b"\x01d\x00");
        assert_eq!(&bytes[3..8], &[0; 5], "the double realigns to 8");
        assert_eq!(&bytes[8..], &0.5f64.to_le_bytes());
        assert_eq!(roundtrip(&v), Some(v));
    }

    #[test]
    fn variants_nest() {
        let inner = Val::Dict(vec![
            ("mpris:trackid".into(), Val::Path("/t/1".into())),
            (
                "xesam:artist".into(),
                Val::Array("s".into(), vec![Val::Str("Someone".into())]),
            ),
        ]);
        let v = Val::Variant(Box::new(Val::Struct(vec![
            Val::Variant(Box::new(inner.clone())),
            Val::Byte(9),
        ])));
        assert_eq!(roundtrip(&v), Some(v));
        assert_eq!(roundtrip(&inner), Some(inner));
    }

    #[test]
    fn every_scalar_survives_a_round_trip() {
        for v in [
            Val::Byte(0xab),
            Val::Bool(true),
            Val::Bool(false),
            Val::I16(-2),
            Val::U16(9),
            Val::I32(-70000),
            Val::U32(70000),
            Val::I64(-5_000_000_000),
            Val::U64(5_000_000_000),
            Val::F64(-1.25),
            Val::Str("héllo".into()),
            Val::Path("/org/mpris/MediaPlayer2".into()),
            Val::Sig("a{sv}".into()),
        ] {
            assert_eq!(roundtrip(&v), Some(v.clone()), "{v:?}");
        }
    }

    #[test]
    fn signatures_split_one_complete_type_at_a_time() {
        assert_eq!(split_type("sa{sv}as"), Some(("s", "a{sv}as")));
        assert_eq!(split_type("a{sv}as"), Some(("a{sv}", "as")));
        assert_eq!(split_type("aa(is)b"), Some(("aa(is)", "b")));
        assert_eq!(split_type("(i(ss)a{sv})x"), Some(("(i(ss)a{sv})", "x")));
        assert_eq!(split_type(""), None);
        // Off the wire, so it has to fail rather than run off the end or underflow.
        assert_eq!(split_type("(is"), None);
        assert_eq!(split_type(")"), None);
        assert_eq!(split_type("a"), None);
    }

    #[test]
    fn alignments_match_the_specification_table() {
        for (sig, want) in [
            ("y", 1),
            ("g", 1),
            ("v", 1),
            ("n", 2),
            ("q", 2),
            ("b", 4),
            ("i", 4),
            ("u", 4),
            ("s", 4),
            ("o", 4),
            ("as", 4),
            ("x", 8),
            ("t", 8),
            ("d", 8),
            ("(y)", 8),
            ("{sv}", 8),
        ] {
            assert_eq!(alignment(sig), want, "{sig}");
        }
    }

    #[test]
    fn the_header_field_array_starts_where_the_fixed_header_ends() {
        // Encoded on its own an `a(yv)` would pad four bytes between the length and the first
        // struct. In a message it must not: offset 16 is already eight-aligned.
        let msg = build(
            &Head {
                kind: MSG_METHOD_CALL,
                path: Some(OBJ_PATH),
                interface: Some(IFACE_PLAYER),
                member: Some("PlayPause"),
                destination: Some("org.mpris.MediaPlayer2.ytmplayer"),
                ..Head::default()
            },
            7,
            &[],
        );
        assert_eq!(msg[0], b'l');
        assert_eq!(msg[3], 1, "protocol version");
        assert_eq!(u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]), 0);
        assert_eq!(u32::from_le_bytes([msg[8], msg[9], msg[10], msg[11]]), 7);
        assert_eq!(msg[16], F_PATH, "first header field, unpadded");
        assert_eq!(&msg[17..20], b"\x01o\x00");
    }

    #[test]
    fn a_built_message_parses_back_into_its_own_fields() {
        let body = [Val::Str(IFACE_PLAYER.into()), Val::I64(-42)];
        let msg = build(
            &Head {
                kind: MSG_SIGNAL,
                path: Some(OBJ_PATH),
                interface: Some(IFACE_PROPS),
                member: Some("PropertiesChanged"),
                ..Head::default()
            },
            11,
            &body,
        );
        assert_eq!(
            frame_len(&msg),
            Some(msg.len()),
            "framing agrees with itself"
        );
        let m = parse(&msg).expect("parses");
        assert_eq!(m.kind, MSG_SIGNAL);
        assert_eq!(m.serial, 11);
        assert_eq!(m.path, OBJ_PATH);
        assert_eq!(m.interface, IFACE_PROPS);
        assert_eq!(m.member, "PropertiesChanged");
        assert_eq!(m.signature, "sx");
        assert_eq!(m.args(), body);
    }

    #[test]
    fn the_body_starts_on_an_eight_byte_boundary() {
        // The header is padded to 8 whatever its fields add up to, and an i64 first thing in the
        // body is the value that catches it when it is not.
        for member in ["A", "AB", "ABC", "ABCD", "ABCDE", "ABCDEF", "ABCDEFG"] {
            let msg = build(
                &Head {
                    kind: MSG_SIGNAL,
                    path: Some(OBJ_PATH),
                    interface: Some(IFACE_PLAYER),
                    member: Some(member),
                    ..Head::default()
                },
                1,
                &[Val::I64(0x0102_0304_0506_0708)],
            );
            let m = parse(&msg).expect("parses");
            assert!(m.body.len().is_multiple_of(8), "{member}");
            assert_eq!(m.args(), vec![Val::I64(0x0102_0304_0506_0708)], "{member}");
        }
    }

    #[test]
    fn a_properties_changed_body_marshals_as_sa_sv_as() {
        let state = Snapshot {
            title: "Track".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            art_url: Some("https://example.invalid/a.jpg".into()),
            position_us: 1_000_000,
            length_us: 200_000_000,
            playing: true,
            idle: false,
            can_next: true,
            can_prev: false,
            volume: 0.5,
            track_seq: 3,
            at: Instant::now(),
        };
        let body = [
            Val::Str(IFACE_PLAYER.into()),
            Val::Dict(vec![
                ("PlaybackStatus".into(), Val::Str("Playing".into())),
                ("Metadata".into(), state.metadata()),
                ("Volume".into(), Val::F64(0.5)),
            ]),
            Val::Array("s".into(), Vec::new()),
        ];
        assert_eq!(signature_of(&body), "sa{sv}as");
        let msg = build(
            &Head {
                kind: MSG_SIGNAL,
                path: Some(OBJ_PATH),
                interface: Some(IFACE_PROPS),
                member: Some("PropertiesChanged"),
                ..Head::default()
            },
            1,
            &body,
        );
        let m = parse(&msg).expect("parses");
        assert_eq!(m.args(), body, "the whole signal survives the wire");
    }

    #[test]
    fn a_truncated_message_decodes_to_nothing_instead_of_panicking() {
        let msg = build(
            &Head {
                kind: MSG_METHOD_CALL,
                path: Some(OBJ_PATH),
                interface: Some(IFACE_PROPS),
                member: Some("Get"),
                ..Head::default()
            },
            1,
            &[Val::Str(IFACE_PLAYER.into()), Val::Str("Metadata".into())],
        );
        for cut in 0..msg.len() {
            assert!(parse(&msg[..cut]).is_none() || frame_len(&msg[..cut]) != Some(cut));
        }
        // Nonsense lengths must not become allocations.
        let mut bad = msg.clone();
        bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(frame_len(&bad).unwrap_or(0) > MAX_MESSAGE);
    }

    #[test]
    fn a_big_endian_sender_is_decoded_in_its_own_byte_order() {
        // Nothing on this machine sends big-endian, but the bus forwards messages untouched and
        // the endianness byte is the only thing that says so.
        let mut d = Dec::new(&[0, 0, 0, 7], false);
        assert_eq!(d.u32(), Some(7));
        let mut d = Dec::new(&[0, 0, 0, 4, b'n', b'a', b'm', b'e', 0], false);
        assert_eq!(d.string().as_deref(), Some("name"));
    }

    #[test]
    fn metadata_is_empty_when_nothing_is_loaded() {
        assert_eq!(Snapshot::idle().metadata(), Val::Dict(Vec::new()));
        assert_eq!(Snapshot::idle().status(), "Stopped");
    }

    #[test]
    fn metadata_types_are_the_ones_clients_expect() {
        let now = NowPlaying {
            title: "T",
            artist: "A",
            album: "B",
            art_url: Some("file:///tmp/a.png"),
            position_us: 0,
            length_us: 90_000_000,
            playing: true,
            idle: false,
            can_next: true,
            can_prev: true,
            volume: 0.25,
            track_seq: 12,
        };
        let Val::Dict(d) = Snapshot::from(&now).metadata() else {
            panic!("metadata is a dictionary");
        };
        let get = |k: &str| d.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(
            get("mpris:trackid"),
            Some(Val::Path("/org/ytmplayer/track/12".into()))
        );
        assert_eq!(get("mpris:length"), Some(Val::I64(90_000_000)));
        assert_eq!(get("xesam:title"), Some(Val::Str("T".into())));
        // A single artist is still a list; clients index into it.
        assert_eq!(
            get("xesam:artist"),
            Some(Val::Array("s".into(), vec![Val::Str("A".into())]))
        );
        assert_eq!(
            get("mpris:artUrl"),
            Some(Val::Str("file:///tmp/a.png".into()))
        );
    }

    #[test]
    fn an_unfetchable_art_url_is_dropped_rather_than_advertised() {
        let mut now = NowPlaying {
            title: "T",
            artist: "A",
            album: "B",
            art_url: Some("/home/me/cover.png"),
            position_us: 0,
            length_us: 0,
            playing: true,
            idle: false,
            can_next: false,
            can_prev: false,
            volume: 2.0,
            track_seq: 1,
        };
        assert_eq!(Snapshot::from(&now).art_url, None);
        now.art_url = Some("file:///home/me/cover.png");
        assert!(Snapshot::from(&now).art_url.is_some());
        // Out-of-range volume is clamped rather than passed on to confuse a shell's slider.
        assert_eq!(Snapshot::from(&now).volume, 1.0);
        // An unknown length must not appear as a zero-length track.
        let Val::Dict(d) = Snapshot::from(&now).metadata() else {
            panic!("metadata is a dictionary");
        };
        assert!(!d.iter().any(|(k, _)| k == "mpris:length"));
    }

    #[test]
    fn position_advances_between_publishes_only_while_playing() {
        let mut s = Snapshot::idle();
        s.idle = false;
        s.position_us = 1_000_000;
        s.length_us = 2_000_000;
        s.at = Instant::now() - Duration::from_millis(500);
        assert_eq!(s.position_now(), 1_000_000, "a paused clock does not run");
        s.playing = true;
        assert!(s.position_now() > 1_000_000);
        // Extrapolation must never run past the end of the track.
        s.at = Instant::now() - Duration::from_secs(30);
        assert_eq!(s.position_now(), 2_000_000);
    }

    #[test]
    fn a_seek_for_the_wrong_track_is_ignored() {
        let mut s = Snapshot::idle();
        s.idle = false;
        s.track_seq = 4;
        s.length_us = 10_000_000;
        assert_eq!(
            set_position(&s, "/org/ytmplayer/track/4", 5_000_000),
            Some(MprisCommand::SetPosition(5_000_000))
        );
        assert_eq!(set_position(&s, "/org/ytmplayer/track/3", 5_000_000), None);
        assert_eq!(set_position(&s, "/org/ytmplayer/track/4", -1), None);
        assert_eq!(set_position(&s, "/org/ytmplayer/track/4", 99_000_000), None);
    }

    #[test]
    fn only_a_real_change_counts_as_a_change() {
        let a = Snapshot::from(&NowPlaying {
            title: "T",
            artist: "A",
            album: "B",
            art_url: None,
            position_us: 1,
            length_us: 10,
            playing: true,
            idle: false,
            can_next: true,
            can_prev: true,
            volume: 1.0,
            track_seq: 1,
        });
        let mut b = a.clone();
        b.position_us = 9_000_000;
        assert!(
            !metadata_differs(&a, &b),
            "the clock moving is not a metadata change"
        );
        b.track_seq = 2;
        assert!(metadata_differs(&a, &b));
    }

    #[test]
    fn addresses_are_unescaped() {
        assert_eq!(unescape("/run/user/1000/bus"), "/run/user/1000/bus");
        assert_eq!(unescape("/tmp/a%20b"), "/tmp/a b");
        assert_eq!(unescape("100%"), "100%");
    }

    #[test]
    fn introspection_is_a_walkable_tree() {
        assert!(introspect("/").contains("name=\"org\""));
        assert!(introspect("/org/mpris").contains("name=\"MediaPlayer2\""));
        let xml = introspect(OBJ_PATH);
        assert!(xml.contains("org.mpris.MediaPlayer2.Player"));
        assert!(xml.contains("<method name=\"PlayPause\"/>"));
        assert!(xml.contains("<signal name=\"Seeked\">"));
        // Nothing else exists, and saying so is better than pretending.
        assert!(introspect("/nope").contains("<node/>"));
    }
}
