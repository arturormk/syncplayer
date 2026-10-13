//! Bounded control protocol for subscription and the shared-clock prototype.

use crate::program::ProgramHash;
use crate::time::{ClockNs, MediaPositionNs};
use std::fmt;
use std::io::{self, Read, Write};
use std::str::FromStr;

pub const PROTOCOL_VERSION: u16 = 6;
pub const MAX_LINE_BYTES: usize = 4_096;
pub const MAX_SUBSCRIPTION_CUES: usize = 128;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_SOFTWARE_VERSION_BYTES: usize = 64;
const PREFIX: &str = "SYNCPLAYER-LAB";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities(u64);

impl Capabilities {
    pub const NETWORK_CLOCK: u64 = 1 << 0;
    pub const PREPARED_RELEASE: u64 = 1 << 1;
    pub const STATE_OVER_CONTROL: u64 = 1 << 2;
    pub const REQUIRED: u64 =
        Self::NETWORK_CLOCK | Self::PREPARED_RELEASE | Self::STATE_OVER_CONTROL;

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
    /// UDP state port. Zero means state snapshots use this control connection.
    pub state_port: u16,
    pub cue_durations_ns: Vec<u64>,
}

/// Identifies one run of the authoritative Master process.
///
/// This is deliberately independent of the transport generation: accepting a
/// new Slave does not create a new timeline.
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

impl FromStr for MasterSession {
    type Err = ParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ParseError::InvalidSession);
        }
        u128::from_str_radix(value, 16)
            .map(Self)
            .map_err(|_| ParseError::InvalidSession)
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

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Start => "START",
            Self::Resume => "RESUME",
            Self::Goto => "GOTO",
            Self::Pause => "PAUSE",
            Self::Stop => "STOP",
            Self::Loop => "LOOP",
            Self::Unloop => "UNLOOP",
            Self::Load => "LOAD",
        })
    }
}

impl FromStr for Operation {
    type Err = ParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "START" => Ok(Self::Start),
            "RESUME" => Ok(Self::Resume),
            "GOTO" => Ok(Self::Goto),
            "PAUSE" => Ok(Self::Pause),
            "STOP" => Ok(Self::Stop),
            "LOOP" => Ok(Self::Loop),
            "UNLOOP" => Ok(Self::Unloop),
            "LOAD" => Ok(Self::Load),
            _ => Err(ParseError::UnknownOperation(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportState {
    Waiting,
    Paused,
    Playing,
}

impl fmt::Display for TransportState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Waiting => "WAITING",
            Self::Paused => "PAUSED",
            Self::Playing => "PLAYING",
        })
    }
}

impl FromStr for TransportState {
    type Err = ParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "WAITING" => Ok(Self::Waiting),
            "PAUSED" => Ok(Self::Paused),
            "PLAYING" => Ok(Self::Playing),
            _ => Err(ParseError::UnknownTransportState(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleasePlan {
    pub generation: u64,
    pub operation: Operation,
    /// Absolute position from the beginning of the complete media sequence.
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

impl ReleasePlan {
    /// Checks ordering and ensures the release deadline has not elapsed.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or a deadline at/before `now`.
    pub fn validate(
        self,
        now: ClockNs,
        previous_generation: Option<u64>,
    ) -> Result<Self, ParseError> {
        if previous_generation.is_some_and(|previous| self.generation < previous) {
            return Err(ParseError::StaleGeneration {
                received: self.generation,
                previous: previous_generation.unwrap_or_default(),
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
        master_cue_durations_ns: Vec<u64>,
    },
    Ready,
    Release(ReleasePlan),
    Prepared {
        session: MasterSession,
        generation: u64,
    },
    Heartbeat {
        session: MasterSession,
        sequence: u64,
        master_clock: ClockNs,
        state: StateSnapshot,
    },
    Alive {
        session: MasterSession,
        sequence: u64,
    },
    Error(String),
}

impl Message {
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Subscribe(subscription) => format!(
                "{PREFIX} {PROTOCOL_VERSION} SUBSCRIBE {} {} {} {} {:016x} {} {}",
                encode_text(&subscription.player_id),
                encode_text(&subscription.program_id),
                subscription.program_hash,
                encode_text(&subscription.software_version),
                subscription.capabilities.bits(),
                subscription.state_port,
                encode_durations(&subscription.cue_durations_ns)
            ),
            Self::Subscribed {
                session,
                generation,
                clock_port,
                master_cue_durations_ns,
            } => format!(
                "{PREFIX} {PROTOCOL_VERSION} SUBSCRIBED {session} {generation} {clock_port} {}",
                encode_durations(master_cue_durations_ns)
            ),
            Self::Ready => format!("{PREFIX} {PROTOCOL_VERSION} READY"),
            Self::Release(plan) => format!(
                "{PREFIX} {PROTOCOL_VERSION} RELEASE {} {} {} {}",
                plan.generation, plan.operation, plan.media_position, plan.release_at
            ),
            Self::Prepared {
                session,
                generation,
            } => format!("{PREFIX} {PROTOCOL_VERSION} PREPARED {session} {generation}"),
            Self::Heartbeat {
                session,
                sequence,
                master_clock,
                state,
            } => format!(
                "{PREFIX} {PROTOCOL_VERSION} HEARTBEAT {session} {sequence} {master_clock} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                state.generation,
                encode_text(&state.program_id),
                state.program_hash,
                state.transport_state,
                state.operation,
                state.media_position,
                state.anchor_clock,
                state.item_index,
                encode_text(&state.cue_id),
                state.cue_occurrence,
                state.item_position,
                state.cue_duration,
                state.playlist_iteration,
                encode_debug_loop(state.debug_loop)
            ),
            Self::Alive { session, sequence } => {
                format!("{PREFIX} {PROTOCOL_VERSION} ALIVE {session} {sequence}")
            }
            Self::Error(message) => {
                let safe = message.replace(['\r', '\n'], " ");
                format!("{PREFIX} {PROTOCOL_VERSION} ERROR {safe}")
            }
        }
    }

    /// Parses one protocol line without its newline terminator.
    ///
    /// # Errors
    ///
    /// Returns an error when the message is oversized, malformed, unsupported,
    /// or contains fields not defined for its message type.
    pub fn decode(line: &str) -> Result<Self, ParseError> {
        if line.len() > MAX_LINE_BYTES {
            return Err(ParseError::TooLong);
        }
        let mut fields = line.split_ascii_whitespace();
        expect(&mut fields, PREFIX)?;
        let version = parse::<u16>(next(&mut fields)?, "version")?;
        if version != PROTOCOL_VERSION {
            return Err(ParseError::UnsupportedVersion(version));
        }
        let kind = next(&mut fields)?;
        let message = match kind {
            "SUBSCRIBE" => Self::Subscribe(Subscription {
                player_id: decode_text(next(&mut fields)?, MAX_ID_BYTES)?,
                program_id: decode_text(next(&mut fields)?, MAX_ID_BYTES)?,
                program_hash: next(&mut fields)?
                    .parse()
                    .map_err(|_| ParseError::InvalidProgramHash)?,
                software_version: decode_text(next(&mut fields)?, MAX_SOFTWARE_VERSION_BYTES)?,
                capabilities: Capabilities::from_bits(
                    u64::from_str_radix(next(&mut fields)?, 16)
                        .map_err(|_| ParseError::InvalidCapabilities)?,
                ),
                state_port: parse(next(&mut fields)?, "state port")?,
                cue_durations_ns: parse_durations(next(&mut fields)?)?,
            }),
            "SUBSCRIBED" => Self::Subscribed {
                session: next(&mut fields)?.parse()?,
                generation: parse(next(&mut fields)?, "generation")?,
                clock_port: parse(next(&mut fields)?, "clock port")?,
                master_cue_durations_ns: parse_durations(next(&mut fields)?)?,
            },
            "READY" => Self::Ready,
            "RELEASE" => Self::Release(ReleasePlan {
                generation: parse(next(&mut fields)?, "generation")?,
                operation: next(&mut fields)?.parse()?,
                media_position: MediaPositionNs::new(parse(next(&mut fields)?, "position")?),
                release_at: ClockNs::new(parse(next(&mut fields)?, "release timestamp")?),
            }),
            "PREPARED" => Self::Prepared {
                session: next(&mut fields)?.parse()?,
                generation: parse(next(&mut fields)?, "generation")?,
            },
            "HEARTBEAT" => Self::Heartbeat {
                session: next(&mut fields)?.parse()?,
                sequence: parse(next(&mut fields)?, "heartbeat sequence")?,
                master_clock: ClockNs::new(parse(next(&mut fields)?, "Master clock")?),
                state: StateSnapshot {
                    generation: parse(next(&mut fields)?, "generation")?,
                    program_id: decode_text(next(&mut fields)?, MAX_ID_BYTES)?,
                    program_hash: next(&mut fields)?
                        .parse()
                        .map_err(|_| ParseError::InvalidProgramHash)?,
                    transport_state: next(&mut fields)?.parse()?,
                    operation: next(&mut fields)?.parse()?,
                    media_position: MediaPositionNs::new(parse(next(&mut fields)?, "position")?),
                    anchor_clock: ClockNs::new(parse(next(&mut fields)?, "anchor clock")?),
                    item_index: parse(next(&mut fields)?, "item index")?,
                    cue_id: decode_text(next(&mut fields)?, MAX_ID_BYTES)?,
                    cue_occurrence: parse(next(&mut fields)?, "cue occurrence")?,
                    item_position: MediaPositionNs::new(parse(
                        next(&mut fields)?,
                        "item position",
                    )?),
                    cue_duration: MediaPositionNs::new(parse(next(&mut fields)?, "cue duration")?),
                    playlist_iteration: parse(next(&mut fields)?, "playlist iteration")?,
                    debug_loop: parse_debug_loop(next(&mut fields)?)?,
                },
            },
            "ALIVE" => Self::Alive {
                session: next(&mut fields)?.parse()?,
                sequence: parse(next(&mut fields)?, "heartbeat sequence")?,
            },
            "ERROR" => return Ok(Self::Error(fields.collect::<Vec<_>>().join(" "))),
            _ => return Err(ParseError::UnknownMessage(kind.to_owned())),
        };
        if fields.next().is_some() {
            return Err(ParseError::TrailingFields);
        }
        Ok(message)
    }
}

/// Writes and flushes one newline-delimited message.
///
/// # Errors
///
/// Propagates errors from the underlying writer.
pub fn write_message(writer: &mut impl Write, message: &Message) -> io::Result<()> {
    let encoded = message.encode();
    if encoded.len() > MAX_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("protocol message exceeds {MAX_LINE_BYTES} bytes"),
        ));
    }
    writer.write_all(encoded.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Reads one bounded newline-delimited message.
///
/// # Errors
///
/// Returns an error for I/O failure, EOF, truncation, invalid UTF-8, an
/// oversized line, or a malformed message.
pub fn read_message(reader: &mut impl Read) -> Result<Message, ReadError> {
    let mut bytes = Vec::with_capacity(64);
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) if bytes.is_empty() => return Err(ReadError::Eof),
            Ok(0) => return Err(ReadError::Truncated),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                if bytes.len() == MAX_LINE_BYTES {
                    return Err(ReadError::TooLong);
                }
                bytes.push(byte[0]);
            }
            Err(error) => return Err(ReadError::Io(error)),
        }
    }
    let line = std::str::from_utf8(&bytes).map_err(|_| ReadError::NotUtf8)?;
    Message::decode(line.trim_end_matches('\r')).map_err(ReadError::Parse)
}

#[derive(Debug, Eq, PartialEq)]
pub enum ParseError {
    MissingField,
    UnexpectedField {
        expected: &'static str,
        actual: String,
    },
    InvalidNumber(&'static str),
    InvalidSession,
    InvalidProgramHash,
    InvalidCapabilities,
    InvalidHex,
    InvalidUtf8,
    TextTooLong,
    InvalidCueDurations,
    InvalidDebugLoop,
    TooManyCues,
    UnsupportedVersion(u16),
    UnknownMessage(String),
    UnknownOperation(String),
    UnknownTransportState(String),
    TrailingFields,
    TooLong,
    StaleGeneration {
        received: u64,
        previous: u64,
    },
    ReleaseNotFuture,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum ReadError {
    Eof,
    Truncated,
    TooLong,
    NotUtf8,
    Io(io::Error),
    Parse(ParseError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ReadError {}

fn next<'a>(fields: &mut impl Iterator<Item = &'a str>) -> Result<&'a str, ParseError> {
    fields.next().ok_or(ParseError::MissingField)
}

fn expect<'a>(
    fields: &mut impl Iterator<Item = &'a str>,
    expected: &'static str,
) -> Result<(), ParseError> {
    let actual = next(fields)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ParseError::UnexpectedField {
            expected,
            actual: actual.to_owned(),
        })
    }
}

fn parse<T: FromStr>(value: &str, name: &'static str) -> Result<T, ParseError> {
    value.parse().map_err(|_| ParseError::InvalidNumber(name))
}

fn encode_text(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn decode_text(value: &str, max_bytes: usize) -> Result<String, ParseError> {
    if value.is_empty() || value.len() & 1 == 1 {
        return Err(ParseError::InvalidHex);
    }
    let byte_count = value.len() / 2;
    if byte_count > max_bytes {
        return Err(ParseError::TextTooLong);
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16).map_err(|_| ParseError::InvalidHex)
        })
        .collect::<Result<Vec<_>, _>>()?;
    String::from_utf8(bytes).map_err(|_| ParseError::InvalidUtf8)
}

fn encode_durations(durations: &[u64]) -> String {
    durations
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_durations(value: &str) -> Result<Vec<u64>, ParseError> {
    if value.is_empty() {
        return Err(ParseError::InvalidCueDurations);
    }
    let durations = value
        .split(',')
        .map(|duration| {
            duration
                .parse::<u64>()
                .ok()
                .filter(|duration| *duration > 0)
                .ok_or(ParseError::InvalidCueDurations)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if durations.len() > MAX_SUBSCRIPTION_CUES {
        return Err(ParseError::TooManyCues);
    }
    Ok(durations)
}

fn encode_debug_loop(debug_loop: Option<DebugLoopState>) -> String {
    debug_loop.map_or_else(
        || "-".to_owned(),
        |value| {
            format!(
                "{},{},{}",
                value.start.get(),
                value.end.get(),
                u8::from(value.events_enabled)
            )
        },
    )
}

fn parse_debug_loop(value: &str) -> Result<Option<DebugLoopState>, ParseError> {
    if value == "-" {
        return Ok(None);
    }
    let mut fields = value.split(',');
    let start = parse(
        fields.next().ok_or(ParseError::InvalidDebugLoop)?,
        "loop start",
    )?;
    let end = parse(
        fields.next().ok_or(ParseError::InvalidDebugLoop)?,
        "loop end",
    )?;
    let events_enabled = match fields.next() {
        Some("0") => false,
        Some("1") => true,
        _ => return Err(ParseError::InvalidDebugLoop),
    };
    if fields.next().is_some() || start >= end {
        return Err(ParseError::InvalidDebugLoop);
    }
    Ok(Some(DebugLoopState {
        start: MediaPositionNs::new(start),
        end: MediaPositionNs::new(end),
        events_enabled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn release() -> Message {
        Message::Release(ReleasePlan {
            generation: 2,
            operation: Operation::Goto,
            media_position: MediaPositionNs::new(3_000),
            release_at: ClockNs::new(5_000),
        })
    }

    fn session() -> MasterSession {
        MasterSession::new(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
    }

    #[test]
    fn messages_round_trip() {
        for message in [
            Message::Subscribe(Subscription {
                player_id: "wall left".to_owned(),
                program_id: "museum show".to_owned(),
                program_hash: ProgramHash::from_bytes([0xab; 32]),
                software_version: "0.1.0-dev".to_owned(),
                capabilities: Capabilities::current(),
                state_port: 0,
                cue_durations_ns: vec![1, 2, u64::MAX],
            }),
            Message::Subscribed {
                session: session(),
                generation: 7,
                clock_port: 4_601,
                master_cue_durations_ns: vec![1, 2, u64::MAX],
            },
            Message::Ready,
            release(),
            Message::Prepared {
                session: session(),
                generation: 7,
            },
            Message::Heartbeat {
                session: session(),
                sequence: 42,
                master_clock: ClockNs::new(9_000),
                state: StateSnapshot {
                    generation: 7,
                    program_id: "museum show".to_owned(),
                    program_hash: ProgramHash::from_bytes([0xab; 32]),
                    transport_state: TransportState::Playing,
                    operation: Operation::Start,
                    media_position: MediaPositionNs::new(3_000),
                    anchor_clock: ClockNs::new(8_000),
                    item_index: 2,
                    cue_id: "main".to_owned(),
                    cue_occurrence: 14,
                    item_position: MediaPositionNs::new(400),
                    cue_duration: MediaPositionNs::new(5_000),
                    playlist_iteration: 4,
                    debug_loop: Some(DebugLoopState {
                        start: MediaPositionNs::new(100),
                        end: MediaPositionNs::new(200),
                        events_enabled: false,
                    }),
                },
            },
            Message::Alive {
                session: session(),
                sequence: 42,
            },
            Message::Error("program mismatch".to_owned()),
        ] {
            assert_eq!(Message::decode(&message.encode()), Ok(message));
        }
    }

    #[test]
    fn rejects_malformed_session_and_heartbeat() {
        assert_eq!(
            Message::decode(
                "SYNCPLAYER-LAB 6 HEARTBEAT not-a-session 1 2 3 4 5 PLAYING START 6 7 8 9 10 11 12 13 -"
            ),
            Err(ParseError::InvalidSession)
        );
        assert_eq!(
            Message::decode("SYNCPLAYER-LAB 6 ALIVE 0123456789abcdef0123456789abcdef 1 surprise"),
            Err(ParseError::TrailingFields)
        );
    }

    #[test]
    fn maximum_snapshot_heartbeat_remains_bounded() {
        let message = Message::Heartbeat {
            session: MasterSession::new(u128::MAX),
            sequence: u64::MAX,
            master_clock: ClockNs::new(u64::MAX),
            state: StateSnapshot {
                generation: u64::MAX,
                program_id: "p".repeat(MAX_ID_BYTES),
                program_hash: ProgramHash::from_bytes([0xff; 32]),
                transport_state: TransportState::Playing,
                operation: Operation::Resume,
                media_position: MediaPositionNs::new(u64::MAX),
                anchor_clock: ClockNs::new(u64::MAX),
                item_index: u64::MAX,
                cue_id: "c".repeat(MAX_ID_BYTES),
                cue_occurrence: u64::MAX,
                item_position: MediaPositionNs::new(u64::MAX),
                cue_duration: MediaPositionNs::new(u64::MAX),
                playlist_iteration: u64::MAX,
                debug_loop: Some(DebugLoopState {
                    start: MediaPositionNs::new(u64::MAX - 1),
                    end: MediaPositionNs::new(u64::MAX),
                    events_enabled: true,
                }),
            },
        };
        assert!(message.encode().len() <= MAX_LINE_BYTES);
    }

    #[test]
    fn rejects_unknown_version_and_trailing_data() {
        assert_eq!(
            Message::decode("SYNCPLAYER-LAB 99 READY"),
            Err(ParseError::UnsupportedVersion(99))
        );
        assert_eq!(
            Message::decode("SYNCPLAYER-LAB 6 READY surprise"),
            Err(ParseError::TrailingFields)
        );
    }

    #[test]
    fn bounded_reader_rejects_oversized_and_truncated_lines() {
        let oversized = vec![b'x'; MAX_LINE_BYTES + 2];
        assert!(matches!(
            read_message(&mut Cursor::new(oversized)),
            Err(ReadError::TooLong)
        ));
        assert!(matches!(
            read_message(&mut Cursor::new(b"SYNCPLAYER-LAB 1 READY")),
            Err(ReadError::Truncated)
        ));
    }

    #[test]
    fn release_must_be_new_and_in_the_future() {
        let Message::Release(plan) = release() else {
            unreachable!()
        };
        assert!(plan.validate(ClockNs::new(4_000), Some(1)).is_ok());
        assert!(plan.validate(ClockNs::new(4_000), Some(2)).is_ok());
        assert!(matches!(
            plan.validate(ClockNs::new(4_000), Some(3)),
            Err(ParseError::StaleGeneration { .. })
        ));
        assert_eq!(
            plan.validate(ClockNs::new(5_000), None),
            Err(ParseError::ReleaseNotFuture)
        );
    }

    #[test]
    fn subscription_rejects_invalid_and_excessive_fields() {
        let hash = ProgramHash::from_bytes([0; 32]);
        let prefix = format!(
            "SYNCPLAYER-LAB {PROTOCOL_VERSION} SUBSCRIBE 61 62 {hash} 63 0000000000000007 0 "
        );
        assert_eq!(
            Message::decode(&format!("{prefix}0")),
            Err(ParseError::InvalidCueDurations)
        );
        assert_eq!(
            Message::decode(&format!("{prefix}1,not-a-duration")),
            Err(ParseError::InvalidCueDurations)
        );
        let too_many = vec!["1"; MAX_SUBSCRIPTION_CUES + 1].join(",");
        assert_eq!(
            Message::decode(&format!("{prefix}{too_many}")),
            Err(ParseError::TooManyCues)
        );
    }
}
