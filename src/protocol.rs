//! Versioned, bounded binary control and state protocols.

use crate::program::ProgramHash;
use crate::time::{ClockNs, MediaPositionNs};
use std::fmt;
use std::io::{self, Read, Write};
use std::str::FromStr;

pub const PROTOCOL_VERSION: u16 = 8;
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 16_384;
pub const MAX_STATE_PACKET_BYTES: usize = 1_024;
pub const MAX_SUBSCRIPTION_CUES: usize = 128;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_SOFTWARE_VERSION_BYTES: usize = 64;
const MAX_ERROR_BYTES: usize = 1_024;
const MAGIC: [u8; 4] = *b"SYNP";
const HEADER_BYTES: usize = 12;
const STATE_KIND: u8 = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities(u64);

impl Capabilities {
    pub const NETWORK_CLOCK: u64 = 1 << 0;
    pub const PREPARED_RELEASE: u64 = 1 << 1;
    pub const STATE_DATAGRAM: u64 = 1 << 2;
    pub const BINARY_CONTROL: u64 = 1 << 3;
    pub const DIAGNOSTIC_TELEMETRY: u64 = 1 << 4;
    pub const REQUIRED: u64 = Self::NETWORK_CLOCK
        | Self::PREPARED_RELEASE
        | Self::STATE_DATAGRAM
        | Self::BINARY_CONTROL
        | Self::DIAGNOSTIC_TELEMETRY;
    #[must_use]
    pub const fn current() -> Self {
        Self(Self::REQUIRED)
    }
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }
    #[must_use]
    pub const fn contains(self, required: u64) -> bool {
        self.0 & required == required
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    pub player_id: String,
    pub program_id: String,
    pub program_hash: ProgramHash,
    pub software_version: String,
    pub capabilities: Capabilities,
    pub state_port: u16,
    pub cue_durations_ns: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MasterSession(u128);
impl MasterSession {
    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}
impl fmt::Display for MasterSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}
impl FromStr for MasterSession {
    type Err = ParseError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ParseError::InvalidValue("session"));
        }
        u128::from_str_radix(value, 16)
            .map(Self)
            .map_err(|_| ParseError::InvalidValue("session"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Start,
    Resume,
    Goto,
    Pause,
    Stop,
    Loop,
    Unloop,
    Load,
}
impl Operation {
    const fn code(self) -> u8 {
        match self {
            Self::Start => 0,
            Self::Resume => 1,
            Self::Goto => 2,
            Self::Pause => 3,
            Self::Stop => 4,
            Self::Loop => 5,
            Self::Unloop => 6,
            Self::Load => 7,
        }
    }
    fn decode(value: u8) -> Result<Self, ParseError> {
        match value {
            0 => Ok(Self::Start),
            1 => Ok(Self::Resume),
            2 => Ok(Self::Goto),
            3 => Ok(Self::Pause),
            4 => Ok(Self::Stop),
            5 => Ok(Self::Loop),
            6 => Ok(Self::Unloop),
            7 => Ok(Self::Load),
            _ => Err(ParseError::InvalidValue("operation")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportState {
    Waiting,
    Paused,
    Playing,
}
impl TransportState {
    const fn code(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Paused => 1,
            Self::Playing => 2,
        }
    }
    fn decode(value: u8) -> Result<Self, ParseError> {
        match value {
            0 => Ok(Self::Waiting),
            1 => Ok(Self::Paused),
            2 => Ok(Self::Playing),
            _ => Err(ParseError::InvalidValue("transport state")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleasePlan {
    pub generation: u64,
    pub operation: Operation,
    pub transport_state: TransportState,
    pub media_position: MediaPositionNs,
    pub release_at: ClockNs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DebugLoopState {
    pub start: MediaPositionNs,
    pub end: MediaPositionNs,
    pub events_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateSnapshot {
    pub generation: u64,
    pub program_id: String,
    pub program_hash: ProgramHash,
    pub transport_state: TransportState,
    pub operation: Operation,
    pub media_position: MediaPositionNs,
    pub anchor_clock: ClockNs,
    pub item_index: u64,
    pub cue_id: String,
    pub cue_occurrence: u64,
    pub item_position: MediaPositionNs,
    pub cue_duration: MediaPositionNs,
    pub playlist_iteration: u64,
    pub debug_loop: Option<DebugLoopState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateDatagram {
    pub session: MasterSession,
    pub sequence: u64,
    pub master_clock: ClockNs,
    pub state: StateSnapshot,
}

/// A bounded, periodic observation made in the shared clock domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticTelemetry {
    pub clock_time: ClockNs,
    pub generation: u64,
    pub expected_position: MediaPositionNs,
    pub actual_position: MediaPositionNs,
    pub error_ns: i64,
    pub cue_index: u64,
    pub cue_position: MediaPositionNs,
    pub pipeline_playing: bool,
    pub transition_in_progress: bool,
    pub clock_healthy: bool,
    pub disconnect_count: u64,
    pub recovery_count: u64,
}

impl ReleasePlan {
    /// Rejects stale generations and release deadlines that have already elapsed.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::StaleGeneration`] or [`ParseError::ReleaseNotFuture`].
    pub fn validate(self, now: ClockNs, previous: Option<u64>) -> Result<Self, ParseError> {
        if previous.is_some_and(|value| self.generation < value) {
            return Err(ParseError::StaleGeneration {
                received: self.generation,
                previous: previous.unwrap_or_default(),
            });
        }
        if self.release_at <= now {
            return Err(ParseError::ReleaseNotFuture);
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Message {
    Subscribe(Subscription),
    Subscribed {
        session: MasterSession,
        generation: u64,
        clock_port: u16,
        state_interval_ms: u32,
        master_cue_durations_ns: Vec<u64>,
    },
    Ready,
    Release(ReleasePlan),
    Prepared {
        session: MasterSession,
        generation: u64,
    },
    Keepalive {
        session: MasterSession,
        sequence: u64,
    },
    KeepaliveAck {
        session: MasterSession,
        sequence: u64,
    },
    Telemetry {
        session: MasterSession,
        sample: DiagnosticTelemetry,
    },
    Error(String),
}

impl Message {
    const fn kind(&self) -> u8 {
        match self {
            Self::Subscribe(_) => 1,
            Self::Subscribed { .. } => 2,
            Self::Ready => 3,
            Self::Release(_) => 4,
            Self::Prepared { .. } => 5,
            Self::Keepalive { .. } => 6,
            Self::KeepaliveAck { .. } => 7,
            Self::Telemetry { .. } => 8,
            Self::Error(_) => 255,
        }
    }
    fn payload(&self) -> Result<Vec<u8>, ParseError> {
        let mut out = Vec::new();
        match self {
            Self::Subscribe(v) => {
                string(&mut out, &v.player_id, MAX_ID_BYTES)?;
                string(&mut out, &v.program_id, MAX_ID_BYTES)?;
                out.extend_from_slice(v.program_hash.as_bytes());
                string(&mut out, &v.software_version, MAX_SOFTWARE_VERSION_BYTES)?;
                u64v(&mut out, v.capabilities.bits());
                u16v(&mut out, v.state_port);
                durations(&mut out, &v.cue_durations_ns)?;
            }
            Self::Subscribed {
                session,
                generation,
                clock_port,
                state_interval_ms,
                master_cue_durations_ns,
            } => {
                u128v(&mut out, session.get());
                u64v(&mut out, *generation);
                u16v(&mut out, *clock_port);
                u32v(&mut out, *state_interval_ms);
                durations(&mut out, master_cue_durations_ns)?;
            }
            Self::Ready => {}
            Self::Release(v) => {
                u64v(&mut out, v.generation);
                out.push(v.operation.code());
                out.push(v.transport_state.code());
                u64v(&mut out, v.media_position.get());
                u64v(&mut out, v.release_at.get());
            }
            Self::Prepared {
                session,
                generation,
            } => {
                u128v(&mut out, session.get());
                u64v(&mut out, *generation);
            }
            Self::Keepalive { session, sequence } | Self::KeepaliveAck { session, sequence } => {
                u128v(&mut out, session.get());
                u64v(&mut out, *sequence);
            }
            Self::Telemetry { session, sample } => {
                u128v(&mut out, session.get());
                u64v(&mut out, sample.clock_time.get());
                u64v(&mut out, sample.generation);
                u64v(&mut out, sample.expected_position.get());
                u64v(&mut out, sample.actual_position.get());
                out.extend_from_slice(&sample.error_ns.to_be_bytes());
                u64v(&mut out, sample.cue_index);
                u64v(&mut out, sample.cue_position.get());
                out.push(u8::from(sample.pipeline_playing));
                out.push(u8::from(sample.transition_in_progress));
                out.push(u8::from(sample.clock_healthy));
                u64v(&mut out, sample.disconnect_count);
                u64v(&mut out, sample.recovery_count);
            }
            Self::Error(value) => string(&mut out, value, MAX_ERROR_BYTES)?,
        }
        if out.len() > MAX_CONTROL_PAYLOAD_BYTES {
            Err(ParseError::TooLong)
        } else {
            Ok(out)
        }
    }
    fn from_payload(kind: u8, payload: &[u8]) -> Result<Self, ParseError> {
        let mut d = Decoder::new(payload);
        let value = match kind {
            1 => Self::Subscribe(Subscription {
                player_id: d.string(MAX_ID_BYTES)?,
                program_id: d.string(MAX_ID_BYTES)?,
                program_hash: ProgramHash::from_bytes(d.array()?),
                software_version: d.string(MAX_SOFTWARE_VERSION_BYTES)?,
                capabilities: Capabilities::from_bits(d.u64()?),
                state_port: d.u16()?,
                cue_durations_ns: d.durations()?,
            }),
            2 => Self::Subscribed {
                session: MasterSession::new(d.u128()?),
                generation: d.u64()?,
                clock_port: d.u16()?,
                state_interval_ms: d.u32()?,
                master_cue_durations_ns: d.durations()?,
            },
            3 => Self::Ready,
            4 => Self::Release(ReleasePlan {
                generation: d.u64()?,
                operation: Operation::decode(d.u8()?)?,
                transport_state: TransportState::decode(d.u8()?)?,
                media_position: MediaPositionNs::new(d.u64()?),
                release_at: ClockNs::new(d.u64()?),
            }),
            5 => Self::Prepared {
                session: MasterSession::new(d.u128()?),
                generation: d.u64()?,
            },
            6 => Self::Keepalive {
                session: MasterSession::new(d.u128()?),
                sequence: d.u64()?,
            },
            7 => Self::KeepaliveAck {
                session: MasterSession::new(d.u128()?),
                sequence: d.u64()?,
            },
            8 => Self::Telemetry {
                session: MasterSession::new(d.u128()?),
                sample: DiagnosticTelemetry {
                    clock_time: ClockNs::new(d.u64()?),
                    generation: d.u64()?,
                    expected_position: MediaPositionNs::new(d.u64()?),
                    actual_position: MediaPositionNs::new(d.u64()?),
                    error_ns: i64::from_be_bytes(d.array()?),
                    cue_index: d.u64()?,
                    cue_position: MediaPositionNs::new(d.u64()?),
                    pipeline_playing: d.boolean("pipeline playing")?,
                    transition_in_progress: d.boolean("transition in progress")?,
                    clock_healthy: d.boolean("clock healthy")?,
                    disconnect_count: d.u64()?,
                    recovery_count: d.u64()?,
                },
            },
            255 => Self::Error(d.string(MAX_ERROR_BYTES)?),
            _ => return Err(ParseError::UnknownMessage(kind)),
        };
        d.finish()?;
        Ok(value)
    }
}

/// Writes one length-delimited binary control frame.
///
/// # Errors
///
/// Returns an I/O error if encoding or writing the bounded frame fails.
pub fn write_message(writer: &mut impl Write, message: &Message) -> io::Result<()> {
    let payload = message.payload().map_err(invalid)?;
    let length = u32::try_from(payload.len()).map_err(|_| invalid(ParseError::TooLong))?;
    writer.write_all(&header(message.kind(), length))?;
    writer.write_all(&payload)?;
    writer.flush()
}
/// Reads and validates one length-delimited binary control frame.
///
/// # Errors
///
/// Returns an I/O or parse error for incomplete, oversized, or malformed input.
pub fn read_message(reader: &mut impl Read) -> Result<Message, ReadError> {
    let mut h = [0; HEADER_BYTES];
    reader.read_exact(&mut h).map_err(ReadError::Io)?;
    let (kind, length) = parse_header(&h).map_err(ReadError::Parse)?;
    if length > MAX_CONTROL_PAYLOAD_BYTES {
        return Err(ReadError::Parse(ParseError::TooLong));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).map_err(ReadError::Io)?;
    Message::from_payload(kind, &payload).map_err(ReadError::Parse)
}

/// Encodes one complete bounded UDP state snapshot.
///
/// # Errors
///
/// Returns a parse error when a field or the complete packet exceeds its bound.
pub fn encode_state_datagram(packet: &StateDatagram) -> Result<Vec<u8>, ParseError> {
    let mut out = Vec::with_capacity(256);
    u128v(&mut out, packet.session.get());
    u64v(&mut out, packet.sequence);
    u64v(&mut out, packet.master_clock.get());
    let s = &packet.state;
    u64v(&mut out, s.generation);
    string(&mut out, &s.program_id, MAX_ID_BYTES)?;
    out.extend_from_slice(s.program_hash.as_bytes());
    out.push(s.transport_state.code());
    out.push(s.operation.code());
    u64v(&mut out, s.media_position.get());
    u64v(&mut out, s.anchor_clock.get());
    u64v(&mut out, s.item_index);
    string(&mut out, &s.cue_id, MAX_ID_BYTES)?;
    u64v(&mut out, s.cue_occurrence);
    u64v(&mut out, s.item_position.get());
    u64v(&mut out, s.cue_duration.get());
    u64v(&mut out, s.playlist_iteration);
    match s.debug_loop {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            u64v(&mut out, v.start.get());
            u64v(&mut out, v.end.get());
            out.push(u8::from(v.events_enabled));
        }
    }
    if HEADER_BYTES + out.len() > MAX_STATE_PACKET_BYTES {
        return Err(ParseError::TooLong);
    }
    let mut packet = Vec::with_capacity(HEADER_BYTES + out.len());
    let payload_len = u32::try_from(out.len()).map_err(|_| ParseError::TooLong)?;
    packet.extend_from_slice(&header(STATE_KIND, payload_len));
    packet.extend_from_slice(&out);
    Ok(packet)
}

/// Decodes and fully validates one UDP state snapshot.
///
/// # Errors
///
/// Returns a parse error for incompatible, oversized, truncated, or malformed input.
pub fn decode_state_datagram(packet: &[u8]) -> Result<StateDatagram, ParseError> {
    if packet.len() < HEADER_BYTES || packet.len() > MAX_STATE_PACKET_BYTES {
        return Err(ParseError::InvalidLength);
    }
    let h: &[u8; HEADER_BYTES] = packet[..HEADER_BYTES]
        .try_into()
        .map_err(|_| ParseError::InvalidLength)?;
    let (kind, length) = parse_header(h)?;
    if kind != STATE_KIND || length != packet.len() - HEADER_BYTES {
        return Err(ParseError::InvalidLength);
    }
    let mut d = Decoder::new(&packet[HEADER_BYTES..]);
    let session = MasterSession::new(d.u128()?);
    let sequence = d.u64()?;
    let master_clock = ClockNs::new(d.u64()?);
    let generation = d.u64()?;
    let program_id = d.string(MAX_ID_BYTES)?;
    let program_hash = ProgramHash::from_bytes(d.array()?);
    let transport_state = TransportState::decode(d.u8()?)?;
    let operation = Operation::decode(d.u8()?)?;
    let media_position = MediaPositionNs::new(d.u64()?);
    let anchor_clock = ClockNs::new(d.u64()?);
    let item_index = d.u64()?;
    let cue_id = d.string(MAX_ID_BYTES)?;
    let cue_occurrence = d.u64()?;
    let item_position = MediaPositionNs::new(d.u64()?);
    let cue_duration = MediaPositionNs::new(d.u64()?);
    let playlist_iteration = d.u64()?;
    let debug_loop = match d.u8()? {
        0 => None,
        1 => {
            let start = MediaPositionNs::new(d.u64()?);
            let end = MediaPositionNs::new(d.u64()?);
            let events_enabled = match d.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ParseError::InvalidValue("debug events")),
            };
            if start >= end {
                return Err(ParseError::InvalidValue("debug loop"));
            }
            Some(DebugLoopState {
                start,
                end,
                events_enabled,
            })
        }
        _ => return Err(ParseError::InvalidValue("debug loop presence")),
    };
    d.finish()?;
    Ok(StateDatagram {
        session,
        sequence,
        master_clock,
        state: StateSnapshot {
            generation,
            program_id,
            program_hash,
            transport_state,
            operation,
            media_position,
            anchor_clock,
            item_index,
            cue_id,
            cue_occurrence,
            item_position,
            cue_duration,
            playlist_iteration,
            debug_loop,
        },
    })
}

#[derive(Debug, Eq, PartialEq)]
pub enum ParseError {
    InvalidMagic,
    UnsupportedVersion(u16),
    UnknownMessage(u8),
    InvalidLength,
    InvalidValue(&'static str),
    InvalidUtf8,
    TextTooLong,
    TooManyCues,
    TooLong,
    TrailingBytes,
    StaleGeneration { received: u64, previous: u64 },
    ReleaseNotFuture,
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ParseError {}
#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    Parse(ParseError),
}
impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ReadError {}

fn header(kind: u8, length: u32) -> [u8; HEADER_BYTES] {
    let mut h = [0; HEADER_BYTES];
    h[..4].copy_from_slice(&MAGIC);
    h[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    h[6] = kind;
    h[8..12].copy_from_slice(&length.to_be_bytes());
    h
}
fn parse_header(h: &[u8; HEADER_BYTES]) -> Result<(u8, usize), ParseError> {
    if h[..4] != MAGIC {
        return Err(ParseError::InvalidMagic);
    }
    let version = u16::from_be_bytes([h[4], h[5]]);
    if version != PROTOCOL_VERSION {
        return Err(ParseError::UnsupportedVersion(version));
    }
    if h[7] != 0 {
        return Err(ParseError::InvalidValue("header flags"));
    }
    Ok((
        h[6],
        u32::from_be_bytes(h[8..12].try_into().unwrap()) as usize,
    ))
}
fn u16v(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn u32v(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn u64v(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn u128v(out: &mut Vec<u8>, v: u128) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn string(out: &mut Vec<u8>, v: &str, max: usize) -> Result<(), ParseError> {
    if v.len() > max {
        return Err(ParseError::TextTooLong);
    }
    u16v(
        out,
        u16::try_from(v.len()).map_err(|_| ParseError::TextTooLong)?,
    );
    out.extend_from_slice(v.as_bytes());
    Ok(())
}
fn durations(out: &mut Vec<u8>, v: &[u64]) -> Result<(), ParseError> {
    if v.is_empty() || v.len() > MAX_SUBSCRIPTION_CUES || v.contains(&0) {
        return Err(ParseError::InvalidValue("cue durations"));
    }
    u16v(
        out,
        u16::try_from(v.len()).map_err(|_| ParseError::TooManyCues)?,
    );
    for n in v {
        u64v(out, *n);
    }
    Ok(())
}
struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or(ParseError::InvalidLength)?;
        let v = self
            .bytes
            .get(self.offset..end)
            .ok_or(ParseError::InvalidLength)?;
        self.offset = end;
        Ok(v)
    }
    fn u8(&mut self) -> Result<u8, ParseError> {
        Ok(self.take(1)?[0])
    }
    fn boolean(&mut self, field: &'static str) -> Result<bool, ParseError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ParseError::InvalidValue(field)),
        }
    }
    fn u16(&mut self) -> Result<u16, ParseError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, ParseError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ParseError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u128(&mut self) -> Result<u128, ParseError> {
        Ok(u128::from_be_bytes(self.take(16)?.try_into().unwrap()))
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ParseError> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn string(&mut self, max: usize) -> Result<String, ParseError> {
        let n = usize::from(self.u16()?);
        if n > max {
            return Err(ParseError::TextTooLong);
        }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| ParseError::InvalidUtf8)
    }
    fn durations(&mut self) -> Result<Vec<u64>, ParseError> {
        let n = usize::from(self.u16()?);
        if n == 0 {
            return Err(ParseError::InvalidValue("cue durations"));
        }
        if n > MAX_SUBSCRIPTION_CUES {
            return Err(ParseError::TooManyCues);
        }
        (0..n)
            .map(|_| {
                let v = self.u64()?;
                if v == 0 {
                    Err(ParseError::InvalidValue("cue duration"))
                } else {
                    Ok(v)
                }
            })
            .collect()
    }
    fn finish(self) -> Result<(), ParseError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ParseError::TrailingBytes)
        }
    }
}
fn invalid(e: ParseError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn session() -> MasterSession {
        MasterSession::new(1)
    }
    fn snapshot() -> StateSnapshot {
        StateSnapshot {
            generation: 7,
            program_id: "show".into(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            transport_state: TransportState::Playing,
            operation: Operation::Start,
            media_position: MediaPositionNs::new(3),
            anchor_clock: ClockNs::new(2),
            item_index: 0,
            cue_id: "first".into(),
            cue_occurrence: 0,
            item_position: MediaPositionNs::new(3),
            cue_duration: MediaPositionNs::new(10),
            playlist_iteration: 0,
            debug_loop: None,
        }
    }
    #[test]
    fn control_round_trip() {
        let values = [
            Message::Subscribe(Subscription {
                player_id: "left".into(),
                program_id: "show".into(),
                program_hash: ProgramHash::from_bytes([1; 32]),
                software_version: "0.1".into(),
                capabilities: Capabilities::current(),
                state_port: 46002,
                cue_durations_ns: vec![10],
            }),
            Message::Subscribed {
                session: session(),
                generation: 7,
                clock_port: 46001,
                state_interval_ms: 100,
                master_cue_durations_ns: vec![10],
            },
            Message::Ready,
            Message::Release(ReleasePlan {
                generation: 7,
                operation: Operation::Start,
                transport_state: TransportState::Playing,
                media_position: MediaPositionNs::new(0),
                release_at: ClockNs::new(10),
            }),
            Message::Prepared {
                session: session(),
                generation: 7,
            },
            Message::Keepalive {
                session: session(),
                sequence: 1,
            },
            Message::KeepaliveAck {
                session: session(),
                sequence: 1,
            },
            Message::Telemetry {
                session: session(),
                sample: DiagnosticTelemetry {
                    clock_time: ClockNs::new(100),
                    generation: 7,
                    expected_position: MediaPositionNs::new(50),
                    actual_position: MediaPositionNs::new(49),
                    error_ns: -1,
                    cue_index: 2,
                    cue_position: MediaPositionNs::new(9),
                    pipeline_playing: true,
                    transition_in_progress: false,
                    clock_healthy: true,
                    disconnect_count: 3,
                    recovery_count: 4,
                },
            },
            Message::Error("bad".into()),
        ];
        for value in values {
            let mut bytes = Vec::new();
            write_message(&mut bytes, &value).unwrap();
            assert_eq!(read_message(&mut Cursor::new(bytes)).unwrap(), value);
        }
    }
    #[test]
    fn state_round_trip() {
        let value = StateDatagram {
            session: session(),
            sequence: 4,
            master_clock: ClockNs::new(9),
            state: snapshot(),
        };
        let bytes = encode_state_datagram(&value).unwrap();
        assert!(bytes.len() <= MAX_STATE_PACKET_BYTES);
        assert_eq!(decode_state_datagram(&bytes), Ok(value));
    }
    #[test]
    fn rejects_bad_packet() {
        let value = StateDatagram {
            session: session(),
            sequence: 4,
            master_clock: ClockNs::new(9),
            state: snapshot(),
        };
        let mut bytes = encode_state_datagram(&value).unwrap();
        bytes[0] = 0;
        assert_eq!(decode_state_datagram(&bytes), Err(ParseError::InvalidMagic));

        let mut bytes = encode_state_datagram(&value).unwrap();
        bytes[4..6].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        assert_eq!(
            decode_state_datagram(&bytes),
            Err(ParseError::UnsupportedVersion(PROTOCOL_VERSION + 1))
        );

        let mut bytes = encode_state_datagram(&value).unwrap();
        bytes.pop();
        assert_eq!(
            decode_state_datagram(&bytes),
            Err(ParseError::InvalidLength)
        );
    }
}
