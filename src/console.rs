//! Parsing for the deliberately small, line-oriented installation console.

use std::fmt;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum ConsoleCommand {
    Help,
    Status,
    Diagnostics,
    Pause,
    Play { delay: Option<Duration> },
    Stop,
    Goto { cue: Option<String>, time: Duration },
    Loop { duration: Duration },
    Unloop,
    Osd(OsdCommand),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OsdCommand {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsoleParseError(pub String);

impl fmt::Display for ConsoleParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConsoleParseError {}

/// Parses one complete console command line.
///
/// # Errors
///
/// Returns an error for an unknown command, invalid arity, or malformed time.
pub fn parse_command(line: &str) -> Result<ConsoleCommand, ConsoleParseError> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    match fields.as_slice() {
        ["help" | "?"] => Ok(ConsoleCommand::Help),
        ["status"] => Ok(ConsoleCommand::Status),
        ["diagnostics" | "diag"] => Ok(ConsoleCommand::Diagnostics),
        ["pause"] => Ok(ConsoleCommand::Pause),
        ["play"] => Ok(ConsoleCommand::Play { delay: None }),
        ["play", delay] => Ok(ConsoleCommand::Play {
            delay: Some(parse_time(delay)?),
        }),
        ["stop"] => Ok(ConsoleCommand::Stop),
        ["goto", time] => Ok(ConsoleCommand::Goto {
            cue: None,
            time: parse_time(time)?,
        }),
        ["goto", cue, time] => Ok(ConsoleCommand::Goto {
            cue: Some((*cue).to_owned()),
            time: parse_time(time)?,
        }),
        ["loop", duration] => Ok(ConsoleCommand::Loop {
            duration: parse_time(duration)?,
        }),
        ["unloop"] => Ok(ConsoleCommand::Unloop),
        ["osd", "on"] => Ok(ConsoleCommand::Osd(OsdCommand::On)),
        ["osd", "off"] => Ok(ConsoleCommand::Osd(OsdCommand::Off)),
        ["osd", "toggle"] => Ok(ConsoleCommand::Osd(OsdCommand::Toggle)),
        [] => Err(ConsoleParseError("empty command".to_owned())),
        _ => Err(ConsoleParseError(format!(
            "unknown command or invalid arguments: {line}"
        ))),
    }
}

/// Parses seconds, unit-suffixed values, or `MM:SS`/`HH:MM:SS` notation.
///
/// # Errors
///
/// Returns an error for negative, non-finite, malformed, or overflowing values.
pub fn parse_time(value: &str) -> Result<Duration, ConsoleParseError> {
    if value.contains(':') {
        let parts = value.split(':').collect::<Vec<_>>();
        if !(2..=3).contains(&parts.len()) {
            return Err(invalid_time(value));
        }
        let mut seconds = 0.0;
        for (index, part) in parts.iter().enumerate() {
            let component = part.parse::<f64>().map_err(|_| invalid_time(value))?;
            if !component.is_finite() || component < 0.0 {
                return Err(invalid_time(value));
            }
            if index + 1 != parts.len() && component.fract() != 0.0 {
                return Err(invalid_time(value));
            }
            seconds = seconds * 60.0 + component;
        }
        return duration(seconds, value);
    }

    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1.0)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60.0)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3600.0)
    } else {
        (value, 1.0)
    };
    let scalar = number.parse::<f64>().map_err(|_| invalid_time(value))?;
    duration(scalar * multiplier, value)
}

fn duration(seconds: f64, original: &str) -> Result<Duration, ConsoleParseError> {
    if !seconds.is_finite() || seconds < 0.0 {
        Err(invalid_time(original))
    } else {
        Duration::try_from_secs_f64(seconds).map_err(|_| invalid_time(original))
    }
}

fn invalid_time(value: &str) -> ConsoleParseError {
    ConsoleParseError(format!("invalid time '{value}'"))
}

pub const HELP: &str = "commands: help, status, diagnostics, pause, play [DELAY], stop, goto [CUE] TIME, loop SECONDS, unloop, osd on|off|toggle";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_human_friendly_times() {
        assert_eq!(parse_time("1.5").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_time("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_time("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_time("1:02.5").unwrap(), Duration::from_millis(62_500));
        assert_eq!(parse_time("1:02:03").unwrap(), Duration::from_secs(3723));
    }

    #[test]
    fn parses_goto_forms() {
        assert_eq!(
            parse_command("goto main 01:02").unwrap(),
            ConsoleCommand::Goto {
                cue: Some("main".to_owned()),
                time: Duration::from_secs(62)
            }
        );
    }

    #[test]
    fn parses_osd_commands() {
        assert_eq!(
            parse_command("osd on").unwrap(),
            ConsoleCommand::Osd(OsdCommand::On)
        );
        assert_eq!(
            parse_command("osd off").unwrap(),
            ConsoleCommand::Osd(OsdCommand::Off)
        );
        assert_eq!(
            parse_command("osd toggle").unwrap(),
            ConsoleCommand::Osd(OsdCommand::Toggle)
        );
    }
}
