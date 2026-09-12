//! HAProxy-side SPOP 2.0 client used by the benchmark.
//!
//! It sends the same frames as `tests/benchmark_spoa.py` (a HAPROXY-HELLO handshake, then one
//! `check-client-ip` NOTIFY per request) and validates the AGENT-ACK as strictly: exactly one
//! `set-var` action on the session scope named `ip_bad` carrying a boolean.

use haproxy_spoe::TypedData;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

pub const MESSAGE: &str = "check-client-ip";
pub const ARGUMENT: &str = "ip";
pub const VARIABLE: &str = "ip_bad";

const FRAME_HAPROXY_HELLO: u8 = 0x01;
const FRAME_NOTIFY: u8 = 0x03;
const FRAME_AGENT_HELLO: u8 = 0x65;
const FRAME_AGENT_ACK: u8 = 0x67;
const FLAG_FIN: u32 = 1;
const MAX_FRAME_SIZE: usize = 16_384;
/// Smallest valid frame body: type, flags and two one-byte varints.
const MIN_FRAME_SIZE: usize = 7;
const ACTION_SET_VAR: u8 = 0x01;
const SCOPE_SESSION: u8 = 0x01;
const BOOLEAN_FALSE: u8 = 0x01;
const BOOLEAN_TRUE: u8 = 0x11;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Protocol(String),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn protocol<T>(msg: String) -> Result<T> {
    Err(Error::Protocol(msg))
}

/// SPOE variable-length integer (the library's `varint` module is private).
fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    if n < 240 {
        out.push(n as u8);
        return;
    }
    out.push((n as u8) | 0xF0);
    n = (n - 240) >> 4;
    while n >= 128 {
        out.push((n as u8) | 0x80);
        n = (n - 128) >> 7;
    }
    out.push(n as u8);
}

fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let first = u64::from(*buf.get(*pos)?);
    *pos += 1;
    if first < 240 {
        return Some(first);
    }
    let mut value = first;
    let mut shift = 4;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value += u64::from(byte) << shift;
        shift += 7;
        if byte < 128 {
            return Some(value);
        }
    }
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// Appends `[length][type][flags][stream-id][frame-id][payload]` to `out`.
fn put_frame(out: &mut Vec<u8>, kind: u8, stream_id: u64, frame_id: u64, payload: &[u8]) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    out.push(kind);
    out.extend_from_slice(&FLAG_FIN.to_be_bytes());
    put_varint(out, stream_id);
    put_varint(out, frame_id);
    out.extend_from_slice(payload);
    let length = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&length.to_be_bytes());
}

/// HAPROXY-HELLO payload: the exact key/value list sent by the Python client.
fn hello_payload() -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    put_string(&mut payload, "supported-versions");
    TypedData::String("2.0".into()).encode(&mut payload);
    put_string(&mut payload, "max-frame-size");
    TypedData::UInt32(MAX_FRAME_SIZE as u32).encode(&mut payload);
    put_string(&mut payload, "capabilities");
    TypedData::String(String::new()).encode(&mut payload);
    payload
}

/// NOTIFY payload prefix: message name and one argument named `ip` (the typed value follows).
fn message_prefix() -> Vec<u8> {
    let mut prefix = Vec::with_capacity(32);
    put_string(&mut prefix, MESSAGE);
    prefix.push(1);
    put_string(&mut prefix, ARGUMENT);
    prefix
}

/// Expected ACK action without its final boolean byte: `set-var`, 3 fields, session scope, name.
fn expected_action() -> Vec<u8> {
    let mut action = vec![ACTION_SET_VAR, 3, SCOPE_SESSION];
    put_string(&mut action, VARIABLE);
    action
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Outcome of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    /// Value of `ip_bad` returned by the agent.
    pub blocked: bool,
    /// Bytes sent, framing included.
    pub tx: usize,
    /// Bytes received, framing included.
    pub rx: usize,
}

/// Outcome of one pipelined batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchReply {
    pub blocked: u64,
    pub tx: usize,
    pub rx: usize,
}

/// One HAProxy → agent connection after the SPOP handshake.
pub struct Connection {
    stream: TcpStream,
    stream_id: u64,
    next_frame_id: u64,
    message_prefix: Vec<u8>,
    expected_action: Vec<u8>,
    payload: Vec<u8>,
    out: Vec<u8>,
    inbuf: Vec<u8>,
}

impl Connection {
    /// Connects, performs the handshake and checks the AGENT-HELLO header like the Python client.
    pub fn connect(addr: SocketAddr, stream_id: u64, timeout: Duration) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut connection = Self {
            stream,
            stream_id,
            next_frame_id: 1,
            message_prefix: message_prefix(),
            expected_action: expected_action(),
            payload: Vec::with_capacity(64),
            out: Vec::with_capacity(4096),
            inbuf: Vec::with_capacity(4096),
        };
        connection.handshake()?;
        Ok(connection)
    }

    fn handshake(&mut self) -> Result<()> {
        self.out.clear();
        put_frame(&mut self.out, FRAME_HAPROXY_HELLO, 0, 0, &hello_payload());
        self.stream.write_all(&self.out)?;
        self.read_frame()?;
        // AGENT-HELLO, flags = FIN, stream 0, frame 0.
        if !self
            .inbuf
            .starts_with(&[FRAME_AGENT_HELLO, 0, 0, 0, 1, 0, 0])
        {
            return protocol(format!(
                "expected AGENT-HELLO, received {}",
                hex(&self.inbuf)
            ));
        }
        Ok(())
    }

    /// Reads one frame into `inbuf` (type byte first, length prefix stripped).
    fn read_frame(&mut self) -> Result<()> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header) as usize;
        if !(MIN_FRAME_SIZE..=MAX_FRAME_SIZE).contains(&length) {
            return protocol(format!("invalid response frame size: {length}"));
        }
        self.inbuf.resize(length, 0);
        self.stream.read_exact(&mut self.inbuf)?;
        Ok(())
    }

    fn write_notify(&mut self, frame_id: u64, ip: IpAddr) {
        self.payload.clear();
        self.payload.extend_from_slice(&self.message_prefix);
        match ip {
            IpAddr::V4(v4) => TypedData::IPv4(v4).encode(&mut self.payload),
            IpAddr::V6(v6) => TypedData::IPv6(v6).encode(&mut self.payload),
        }
        put_frame(
            &mut self.out,
            FRAME_NOTIFY,
            self.stream_id,
            frame_id,
            &self.payload,
        );
    }

    /// Sends one NOTIFY for `ip` and waits for its ACK (one outstanding request, like the
    /// Python client).
    pub fn check(&mut self, ip: IpAddr) -> Result<Reply> {
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;
        self.out.clear();
        self.write_notify(frame_id, ip);
        self.stream.write_all(&self.out)?;
        let tx = self.out.len();

        self.read_frame()?;
        let (acked_frame, blocked) =
            decode_ack(&self.inbuf, self.stream_id, &self.expected_action)?;
        if acked_frame != frame_id {
            return protocol(format!(
                "ACK for frame {acked_frame} while waiting for frame {frame_id}"
            ));
        }
        Ok(Reply {
            blocked,
            tx,
            rx: 4 + self.inbuf.len(),
        })
    }

    /// Sends one NOTIFY per address back to back, then reads every ACK (pipelining). ACKs may
    /// arrive in any order; each must belong to the batch exactly once.
    pub fn check_pipelined(&mut self, ips: &[IpAddr]) -> Result<BatchReply> {
        assert!(
            !ips.is_empty() && ips.len() <= 64,
            "pipeline depth must be 1..=64"
        );
        let first_frame = self.next_frame_id;
        self.out.clear();
        for &ip in ips {
            let frame_id = self.next_frame_id;
            self.next_frame_id += 1;
            self.write_notify(frame_id, ip);
        }
        self.stream.write_all(&self.out)?;
        let mut reply = BatchReply {
            tx: self.out.len(),
            ..BatchReply::default()
        };

        let mut pending: u64 = if ips.len() == 64 {
            u64::MAX
        } else {
            (1u64 << ips.len()) - 1
        };
        for _ in ips {
            self.read_frame()?;
            reply.rx += 4 + self.inbuf.len();
            let (frame_id, blocked) =
                decode_ack(&self.inbuf, self.stream_id, &self.expected_action)?;
            let index = frame_id
                .checked_sub(first_frame)
                .filter(|index| *index < ips.len() as u64)
                .filter(|index| pending & (1u64 << index) != 0);
            let Some(index) = index else {
                return protocol(format!("unexpected ACK for frame {frame_id}"));
            };
            pending &= !(1u64 << index);
            reply.blocked += u64::from(blocked);
        }
        Ok(reply)
    }
}

/// Validates an AGENT-ACK body and returns `(frame_id, ip_bad)`.
fn decode_ack(body: &[u8], stream_id: u64, expected_action: &[u8]) -> Result<(u64, bool)> {
    if body.len() < MIN_FRAME_SIZE {
        return protocol(format!("truncated frame {}", hex(body)));
    }
    if body[0] != FRAME_AGENT_ACK {
        return protocol(format!(
            "expected AGENT-ACK, received type {:#04x}",
            body[0]
        ));
    }
    let flags = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
    if flags != FLAG_FIN {
        return protocol(format!("unexpected ACK flags {flags:#010x}"));
    }
    let mut pos = 5;
    let (Some(acked_stream), Some(frame_id)) =
        (get_varint(body, &mut pos), get_varint(body, &mut pos))
    else {
        return protocol(format!("truncated ACK header {}", hex(body)));
    };
    if acked_stream != stream_id {
        return protocol(format!(
            "ACK for stream {acked_stream}, expected {stream_id}"
        ));
    }
    let actions = &body[pos..];
    let Some((&value, action)) = actions.split_last() else {
        return protocol("ACK without action".to_owned());
    };
    if action != expected_action {
        return protocol(format!("unexpected ACK actions {}", hex(actions)));
    }
    match value {
        BOOLEAN_FALSE => Ok((frame_id, false)),
        BOOLEAN_TRUE => Ok((frame_id, true)),
        other => protocol(format!("ip_bad is not a boolean: {other:#04x}")),
    }
}
