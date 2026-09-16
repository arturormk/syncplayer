//! A tiny, bounded, newline-delimited protocol used only by the clock lab.

use crate::time::{ClockNs, MediaPositionNs};
use std::fmt;
use std::io::{self, Read, Write};
use std::str::FromStr;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_LINE_BYTES: usize = 256;
const PREFIX: &str = "SYNCPLAYER-LAB";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Start,
    Resume,
    Goto,
}

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Start => "START",
            Self::Resume => "RESUME",
            Self::Goto => "GOTO",
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
            _ => Err(ParseError::UnknownOperation(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleasePlan {
    pub generation: u64,
    pub operation: Operation,
    pub media_position: MediaPositionNs,
    pub release_at: ClockNs,
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
        if previous_generation.is_some_and(|previous| self.generation <= previous) {
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
    Clock { port: u16 },
    Ready,
    Release(ReleasePlan),
    Error(String),
}

impl Message {
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Clock { port } => format!("{PREFIX} {PROTOCOL_VERSION} CLOCK {port}"),
            Self::Ready => format!("{PREFIX} {PROTOCOL_VERSION} READY"),
            Self::Release(plan) => format!(
                "{PREFIX} {PROTOCOL_VERSION} RELEASE {} {} {} {}",
                plan.generation, plan.operation, plan.media_position, plan.release_at
            ),
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
            "CLOCK" => Self::Clock {
                port: parse(next(&mut fields)?, "clock port")?,
            },
            "READY" => Self::Ready,
            "RELEASE" => Self::Release(ReleasePlan {
                generation: parse(next(&mut fields)?, "generation")?,
                operation: next(&mut fields)?.parse()?,
                media_position: MediaPositionNs::new(parse(next(&mut fields)?, "position")?),
                release_at: ClockNs::new(parse(next(&mut fields)?, "release timestamp")?),
            }),
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
    writer.write_all(message.encode().as_bytes())?;
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
    UnsupportedVersion(u16),
    UnknownMessage(String),
    UnknownOperation(String),
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

    #[test]
    fn messages_round_trip() {
        for message in [Message::Clock { port: 4_601 }, Message::Ready, release()] {
            assert_eq!(Message::decode(&message.encode()), Ok(message));
        }
    }

    #[test]
    fn rejects_unknown_version_and_trailing_data() {
        assert_eq!(
            Message::decode("SYNCPLAYER-LAB 99 READY"),
            Err(ParseError::UnsupportedVersion(99))
        );
        assert_eq!(
            Message::decode("SYNCPLAYER-LAB 1 READY surprise"),
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
        assert!(matches!(
            plan.validate(ClockNs::new(4_000), Some(2)),
            Err(ParseError::StaleGeneration { .. })
        ));
        assert_eq!(
            plan.validate(ClockNs::new(5_000), None),
            Err(ParseError::ReleaseNotFuture)
        );
    }
}
