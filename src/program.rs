//! Shared, versioned program configuration and local-player resolution.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, de};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const PROGRAM_VERSION: u64 = 1;
const CANONICAL_FORMAT: &[u8] = b"syncplayer-program-v1\0";

#[derive(Clone, Debug, PartialEq)]
pub struct ProgramConfig {
    pub version: u64,
    pub program: Program,
    pub aliases: BTreeMap<String, String>,
    pub players: BTreeMap<String, BTreeMap<String, PlayerCue>>,
    source_path: PathBuf,
    base_directory: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub id: String,
    #[serde(rename = "loop")]
    pub looping: bool,
    #[serde(rename = "cue")]
    pub cues: Vec<Cue>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cue {
    pub id: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlayerCue {
    pub video: Option<String>,
    pub audio: Option<String>,
    pub mute: Option<bool>,
    pub gain_db: Option<f64>,
    #[serde(rename = "event")]
    pub events: Option<Vec<Event>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: String,
    pub timing: EventTiming,
    pub action: EventAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventTiming {
    At(u64),
    BeforeEnd(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventAction {
    Process {
        command: Vec<String>,
    },
    Udp {
        host: IpAddr,
        port: u16,
        payload: Vec<u8>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PlayerId(String);

impl PlayerId {
    /// Creates a validated logical player identifier.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty value or one containing control characters.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_identifier("player ID", &value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlayerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProgramHash([u8; 32]);

impl ProgramHash {
    #[must_use]
    pub const fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ProgramHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ProgramHash {
    type Err = ProgramHashParseError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ProgramHashParseError);
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *byte = u8::from_str_radix(&value[offset..offset + 2], 16)
                .map_err(|_| ProgramHashParseError)?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgramHashParseError;

impl fmt::Display for ProgramHashParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("program hash must contain exactly 64 hexadecimal digits")
    }
}

impl std::error::Error for ProgramHashParseError {}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedProgram {
    pub program_id: String,
    pub hash: ProgramHash,
    pub player_id: PlayerId,
    pub looping: bool,
    pub cues: Vec<ResolvedCue>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedCue {
    pub id: String,
    pub video: PathBuf,
    pub audio: Option<PathBuf>,
    pub mute: bool,
    pub gain_db: f64,
    pub events: Vec<Event>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramDocument {
    version: u64,
    program: Program,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    players: BTreeMap<String, BTreeMap<String, PlayerCue>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct EventDocument {
    id: String,
    at: Option<String>,
    before_end: Option<String>,
    #[serde(rename = "type")]
    action_type: EventActionType,
    command: Option<Vec<String>>,
    host: Option<String>,
    port: Option<u16>,
    payload: Option<String>,
    payload_hex: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum EventActionType {
    Process,
    Udp,
}

impl ProgramConfig {
    /// Loads and structurally validates a program definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, TOML cannot be parsed,
    /// or the semantic program is invalid.
    pub fn load(path: &Path) -> Result<Self> {
        let source_path = path
            .canonicalize()
            .with_context(|| format!("cannot resolve program file {}", path.display()))?;
        let text = fs::read_to_string(&source_path)
            .with_context(|| format!("cannot read program file {}", source_path.display()))?;
        let base_directory = source_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::parse(&text, source_path, base_directory)
    }

    fn parse(text: &str, source_path: PathBuf, base_directory: PathBuf) -> Result<Self> {
        let document: ProgramDocument = toml::from_str(text).context("invalid program TOML")?;
        if document.version != PROGRAM_VERSION {
            bail!(
                "unsupported program version {}; expected version {PROGRAM_VERSION}",
                document.version
            );
        }
        let config = Self {
            version: document.version,
            program: document.program,
            aliases: document.aliases,
            players: document.players,
            source_path,
            base_directory,
        };
        config.validate_structure()?;
        Ok(config)
    }

    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Returns the deterministic semantic encoding covered by the program hash.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut writer = CanonicalWriter::new();
        writer.bytes(CANONICAL_FORMAT);
        writer.u64(self.version);
        writer.string(&self.program.id);
        writer.boolean(self.program.looping);
        writer.u64(self.program.cues.len() as u64);
        for cue in &self.program.cues {
            writer.string(&cue.id);
        }
        writer.string_map(&self.aliases);
        writer.u64(self.players.len() as u64);
        for (player, cues) in &self.players {
            writer.string(player);
            writer.u64(cues.len() as u64);
            for (cue_id, cue) in cues {
                writer.string(cue_id);
                writer.optional_string(cue.video.as_deref());
                writer.optional_string(cue.audio.as_deref());
                writer.optional_bool(cue.mute);
                writer.optional_f64(cue.gain_db);
                match &cue.events {
                    None => writer.boolean(false),
                    Some(events) => {
                        writer.boolean(true);
                        writer.u64(events.len() as u64);
                        for event in events {
                            writer.event(event);
                        }
                    }
                }
            }
        }
        writer.finish()
    }

    #[must_use]
    pub fn program_hash(&self) -> ProgramHash {
        ProgramHash(Sha256::digest(self.canonical_bytes()).into())
    }

    /// Resolves the current player from an explicit ID or the system hostname.
    ///
    /// # Errors
    ///
    /// Returns an error if the hostname is unavailable or the selected player
    /// does not have a usable mapping for every cue.
    pub fn resolve_player(&self, explicit: Option<&str>) -> Result<ResolvedProgram> {
        let hostname = hostname::get()
            .context("cannot read system hostname")?
            .into_string()
            .map_err(|_| anyhow!("system hostname is not valid UTF-8"))?;
        self.resolve_player_with_hostname(explicit, &hostname)
    }

    /// Resolves a player using an injected hostname, primarily for deterministic callers/tests.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected identity or its cue mappings are invalid.
    pub fn resolve_player_with_hostname(
        &self,
        explicit: Option<&str>,
        hostname: &str,
    ) -> Result<ResolvedProgram> {
        let selected = match explicit {
            Some(value) => value,
            None => self.aliases.get(hostname).map_or(hostname, String::as_str),
        };
        validate_identifier("player ID", selected)?;
        let player_id = PlayerId(selected.to_owned());
        let defaults = self.players.get("default");
        let player = self.players.get(selected);
        if defaults.is_none() && player.is_none() {
            bail!("no mappings exist for player '{selected}' and no default mappings exist");
        }

        let mut cues = Vec::with_capacity(self.program.cues.len());
        for cue in &self.program.cues {
            let default_cue = defaults.and_then(|entries| entries.get(&cue.id));
            let player_cue = player.and_then(|entries| entries.get(&cue.id));
            let video = choose(
                player_cue.and_then(|value| value.video.as_ref()),
                default_cue.and_then(|value| value.video.as_ref()),
            )
            .ok_or_else(|| {
                anyhow!(
                    "player '{selected}' has no video mapping for cue '{}'",
                    cue.id
                )
            })?;
            let audio = choose(
                player_cue.and_then(|value| value.audio.as_ref()),
                default_cue.and_then(|value| value.audio.as_ref()),
            );
            let mute = choose_copy(
                player_cue.and_then(|value| value.mute),
                default_cue.and_then(|value| value.mute),
            )
            .unwrap_or(false);
            let gain_db = choose_copy(
                player_cue.and_then(|value| value.gain_db),
                default_cue.and_then(|value| value.gain_db),
            )
            .unwrap_or(0.0);
            let events = choose(
                player_cue.and_then(|value| value.events.as_ref()),
                default_cue.and_then(|value| value.events.as_ref()),
            )
            .cloned()
            .unwrap_or_default();
            cues.push(ResolvedCue {
                id: cue.id.clone(),
                video: self.resolve_path(video),
                audio: audio.map(|path| self.resolve_path(path)),
                mute,
                gain_db,
                events,
            });
        }
        Ok(ResolvedProgram {
            program_id: self.program.id.clone(),
            hash: self.program_hash(),
            player_id,
            looping: self.program.looping,
            cues,
        })
    }

    fn resolve_path(&self, value: &str) -> PathBuf {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            self.base_directory.join(path)
        }
    }

    fn validate_structure(&self) -> Result<()> {
        validate_identifier("program ID", &self.program.id)?;
        if self.program.cues.is_empty() {
            bail!("program must contain at least one cue");
        }
        if self.players.is_empty() {
            bail!("program must define at least one player or default mapping");
        }
        let mut cue_ids = BTreeSet::new();
        for cue in &self.program.cues {
            validate_identifier("cue ID", &cue.id)?;
            if !cue_ids.insert(cue.id.as_str()) {
                bail!("duplicate cue ID '{}'", cue.id);
            }
        }
        for (source, target) in &self.aliases {
            validate_identifier("alias hostname", source)?;
            validate_identifier("alias player ID", target)?;
        }
        for (player_id, cues) in &self.players {
            if player_id != "default" {
                validate_identifier("player ID", player_id)?;
            }
            for (cue_id, mapping) in cues {
                if !cue_ids.contains(cue_id.as_str()) {
                    bail!("player '{player_id}' maps unknown cue '{cue_id}'");
                }
                if mapping.gain_db.is_some_and(|value| !value.is_finite()) {
                    bail!("player '{player_id}' cue '{cue_id}' has a non-finite gain_db");
                }
                let mut event_ids = BTreeSet::new();
                if let Some(events) = &mapping.events {
                    for event in events {
                        if !event_ids.insert(event.id.clone()) {
                            bail!(
                                "duplicate event ID '{}' in player '{player_id}' cue '{cue_id}'",
                                event.id
                            );
                        }
                    }
                }
            }
        }

        let defaults = self.players.get("default");
        let mut identities: BTreeSet<&str> = self
            .players
            .keys()
            .filter(|player| player.as_str() != "default")
            .map(String::as_str)
            .collect();
        identities.extend(self.aliases.values().map(String::as_str));
        if identities.is_empty() {
            identities.insert("default");
        }
        for identity in identities {
            let specific = (identity != "default")
                .then(|| self.players.get(identity))
                .flatten();
            for cue in &self.program.cues {
                let has_video = specific
                    .and_then(|mappings| mappings.get(&cue.id))
                    .and_then(|mapping| mapping.video.as_ref())
                    .or_else(|| {
                        defaults
                            .and_then(|mappings| mappings.get(&cue.id))
                            .and_then(|mapping| mapping.video.as_ref())
                    })
                    .is_some();
                if !has_video {
                    bail!(
                        "player '{identity}' has no video mapping for cue '{}'",
                        cue.id
                    );
                }
            }
        }
        Ok(())
    }
}

impl ResolvedProgram {
    /// Rejects configuration features that are modeled but do not yet have a
    /// runtime engine.
    ///
    /// # Errors
    ///
    /// Returns an error when any cue uses a modeled feature whose runtime
    /// implementation belongs to a later milestone.
    pub fn ensure_runtime_supported(&self) -> Result<()> {
        for cue in &self.cues {
            let mut unsupported = Vec::new();
            if cue.audio.is_some() {
                unsupported.push("external audio");
            }
            if cue.mute {
                unsupported.push("cue-local mute");
            }
            if cue.gain_db != 0.0 {
                unsupported.push("cue-local gain");
            }
            if !cue.events.is_empty() {
                unsupported.push("timed events");
            }
            if !unsupported.is_empty() {
                bail!(
                    "cue '{}' configures {}, whose runtime support is not implemented yet",
                    cue.id,
                    unsupported.join(", ")
                );
            }
        }
        Ok(())
    }
}

impl EventDocument {
    fn to_event(&self) -> Result<Event> {
        validate_identifier("event ID", &self.id)?;
        let timing = match (&self.at, &self.before_end) {
            (Some(value), None) => EventTiming::At(parse_duration_ns(value)?),
            (None, Some(value)) => EventTiming::BeforeEnd(parse_duration_ns(value)?),
            (Some(_), Some(_)) => bail!("event '{}' has both at and before_end", self.id),
            (None, None) => bail!("event '{}' requires at or before_end", self.id),
        };
        let action = match self.action_type {
            EventActionType::Process => {
                if self.host.is_some()
                    || self.port.is_some()
                    || self.payload.is_some()
                    || self.payload_hex.is_some()
                {
                    bail!("process event '{}' contains UDP-only fields", self.id);
                }
                let command = self
                    .command
                    .as_ref()
                    .ok_or_else(|| anyhow!("process event '{}' requires a command", self.id))?;
                if command.is_empty() || command.iter().any(String::is_empty) {
                    bail!("process event '{}' requires a non-empty command", self.id);
                }
                EventAction::Process {
                    command: command.clone(),
                }
            }
            EventActionType::Udp => {
                if self.command.is_some() {
                    bail!("UDP event '{}' contains process-only command", self.id);
                }
                let host = self
                    .host
                    .as_ref()
                    .ok_or_else(|| anyhow!("UDP event '{}' requires a host", self.id))?;
                let port = self
                    .port
                    .ok_or_else(|| anyhow!("UDP event '{}' requires a port", self.id))?;
                if port == 0 {
                    bail!("UDP event '{}' requires a non-zero port", self.id);
                }
                let host: IpAddr = host.parse().with_context(|| {
                    format!("UDP event '{}' has invalid IP address '{host}'", self.id)
                })?;
                let payload = match (&self.payload, &self.payload_hex) {
                    (Some(text), None) => text.as_bytes().to_vec(),
                    (None, Some(hex)) => parse_hex(hex).with_context(|| {
                        format!("UDP event '{}' has invalid payload_hex", self.id)
                    })?,
                    (Some(_), Some(_)) => {
                        bail!("UDP event '{}' has both payload and payload_hex", self.id)
                    }
                    (None, None) => bail!("UDP event '{}' requires a payload", self.id),
                };
                EventAction::Udp {
                    host,
                    port,
                    payload,
                }
            }
        };
        Ok(Event {
            id: self.id.clone(),
            timing,
            action,
        })
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        EventDocument::deserialize(deserializer)?
            .to_event()
            .map_err(de::Error::custom)
    }
}

fn choose<'a, T>(specific: Option<&'a T>, default: Option<&'a T>) -> Option<&'a T> {
    specific.or(default)
}

fn choose_copy<T: Copy>(specific: Option<T>, default: Option<T>) -> Option<T> {
    specific.or(default)
}

fn validate_identifier(description: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{description} must not be empty");
    }
    if value.chars().any(char::is_control) {
        bail!("{description} must not contain control characters");
    }
    Ok(())
}

fn parse_duration_ns(value: &str) -> Result<u64> {
    if value.is_empty() {
        bail!("duration must not be empty");
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut total = 0_u128;
    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
            index += 1;
        }
        if index == number_start {
            bail!("invalid duration '{value}'");
        }
        let number = &value[number_start..index];
        let unit_ns = if value[index..].starts_with("ms") {
            index += 2;
            1_000_000_u128
        } else if value[index..].starts_with('s') {
            index += 1;
            1_000_000_000_u128
        } else if value[index..].starts_with('m') {
            index += 1;
            60_000_000_000_u128
        } else {
            bail!("duration term in '{value}' requires ms, s, or m");
        };
        let (whole, fraction, scale) = parse_decimal(number)?;
        let whole_ns = whole
            .checked_mul(unit_ns)
            .ok_or_else(|| anyhow!("duration '{value}' overflows"))?;
        let fraction_product = fraction
            .checked_mul(unit_ns)
            .ok_or_else(|| anyhow!("duration '{value}' overflows"))?;
        if fraction_product % scale != 0 {
            bail!("duration '{value}' is more precise than one nanosecond");
        }
        total = total
            .checked_add(whole_ns + fraction_product / scale)
            .ok_or_else(|| anyhow!("duration '{value}' overflows"))?;
    }
    u64::try_from(total).with_context(|| format!("duration '{value}' overflows"))
}

fn parse_decimal(value: &str) -> Result<(u128, u128, u128)> {
    let mut parts = value.split('.');
    let whole_text = parts.next().unwrap_or_default();
    let fraction_text = parts.next();
    if parts.next().is_some()
        || whole_text.is_empty()
        || !whole_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("invalid decimal duration term '{value}'");
    }
    let whole = whole_text.parse::<u128>()?;
    let Some(fraction_text) = fraction_text else {
        return Ok((whole, 0, 1));
    };
    if fraction_text.is_empty() || !fraction_text.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid decimal duration term '{value}'");
    }
    let scale = 10_u128
        .checked_pow(u32::try_from(fraction_text.len())?)
        .ok_or_else(|| anyhow!("decimal duration term '{value}' is too precise"))?;
    Ok((whole, fraction_text.parse()?, scale))
}

fn parse_hex(value: &str) -> Result<Vec<u8>> {
    let compact: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.is_empty() || compact.len() & 1 == 1 {
        bail!("hex payload must contain complete bytes");
    }
    (0..compact.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&compact[index..index + 2], 16).context("invalid hex byte"))
        .collect()
}

struct CanonicalWriter(Vec<u8>);

impl CanonicalWriter {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn finish(self) -> Vec<u8> {
        self.0
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.0.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn boolean(&mut self, value: bool) {
        self.0.push(u8::from(value));
    }

    fn optional_string(&mut self, value: Option<&str>) {
        self.boolean(value.is_some());
        if let Some(value) = value {
            self.string(value);
        }
    }

    fn optional_bool(&mut self, value: Option<bool>) {
        self.boolean(value.is_some());
        if let Some(value) = value {
            self.boolean(value);
        }
    }

    fn optional_f64(&mut self, value: Option<f64>) {
        self.boolean(value.is_some());
        if let Some(value) = value {
            let normalized = if value == 0.0 { 0.0 } else { value };
            self.0
                .extend_from_slice(&normalized.to_bits().to_be_bytes());
        }
    }

    fn string_map(&mut self, value: &BTreeMap<String, String>) {
        self.u64(value.len() as u64);
        for (key, value) in value {
            self.string(key);
            self.string(value);
        }
    }

    fn event(&mut self, event: &Event) {
        self.string(&event.id);
        match event.timing {
            EventTiming::At(value) => {
                self.0.push(0);
                self.u64(value);
            }
            EventTiming::BeforeEnd(value) => {
                self.0.push(1);
                self.u64(value);
            }
        }
        match &event.action {
            EventAction::Process { command } => {
                self.0.push(0);
                self.u64(command.len() as u64);
                for argument in command {
                    self.string(argument);
                }
            }
            EventAction::Udp {
                host,
                port,
                payload,
            } => {
                self.0.push(1);
                self.string(&host.to_string());
                self.u64(u64::from(*port));
                self.bytes(payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROGRAM: &str = r#"
version = 1
[program]
id = "demo"
loop = true
[[program.cue]]
id = "intro"
[[program.cue]]
id = "main"

[aliases]
"host-a" = "left"
"host-b" = "left"

[players.default.intro]
video = "media/intro.mp4"
mute = true

[players.default.main]
video = "media/main.mp4"

[players.left.intro]
mute = false

[players.left.main]
gain_db = 1.5
[[players.left.main.event]]
id = "lights"
before_end = "1m2.5s"
type = "udp"
host = "127.0.0.1"
port = 9000
payload_hex = "01 02 ff"
"#;

    fn parse(text: &str) -> Result<ProgramConfig> {
        ProgramConfig::parse(
            text,
            PathBuf::from("/shows/demo/show.toml"),
            PathBuf::from("/shows/demo"),
        )
    }

    #[test]
    fn resolves_aliases_defaults_overrides_paths_and_typed_events() {
        let config = parse(PROGRAM).unwrap();
        let resolved = config.resolve_player_with_hostname(None, "host-a").unwrap();
        assert_eq!(resolved.player_id.as_str(), "left");
        assert_eq!(
            resolved.cues[0].video,
            Path::new("/shows/demo/media/intro.mp4")
        );
        assert!(!resolved.cues[0].mute);
        assert_eq!(resolved.cues[1].gain_db, 1.5);
        assert_eq!(
            resolved.cues[1].events[0].timing,
            EventTiming::BeforeEnd(62_500_000_000)
        );
    }

    #[test]
    fn explicit_identity_bypasses_aliases() {
        let config = parse(PROGRAM).unwrap();
        let resolved = config
            .resolve_player_with_hostname(Some("host-a"), "ignored")
            .unwrap();
        assert_eq!(resolved.player_id.as_str(), "host-a");
        assert!(resolved.cues[0].mute);
    }

    #[test]
    fn canonical_hash_ignores_formatting_and_map_order() {
        let first = parse(PROGRAM).unwrap();
        let reordered = PROGRAM.replace(
            "\"host-a\" = \"left\"\n\"host-b\" = \"left\"",
            "# formatting and map order are irrelevant\n\"host-b\" = \"left\"\n\"host-a\"    =    \"left\"",
        );
        let second = parse(&reordered).unwrap();
        assert_eq!(first.program_hash(), second.program_hash());
        assert_eq!(first.program_hash().to_string().len(), 64);
    }

    #[test]
    fn canonical_hash_covers_other_player_events() {
        let first = parse(PROGRAM).unwrap();
        let payload_changed = parse(&PROGRAM.replace("01 02 ff", "01 03 ff")).unwrap();
        let timing_changed = parse(&PROGRAM.replace("1m2.5s", "2s")).unwrap();
        let mapping_changed = parse(&PROGRAM.replace("media/main.mp4", "media/other.mp4")).unwrap();
        assert_ne!(first.program_hash(), payload_changed.program_hash());
        assert_ne!(first.program_hash(), timing_changed.program_hash());
        assert_ne!(first.program_hash(), mapping_changed.program_hash());
    }

    #[test]
    fn program_hash_text_round_trips_and_rejects_malformed_values() {
        let hash = ProgramHash::from_bytes([0xab; 32]);
        assert_eq!(hash.to_string().parse(), Ok(hash));
        assert!("abc".parse::<ProgramHash>().is_err());
        assert!("z".repeat(64).parse::<ProgramHash>().is_err());
    }

    #[test]
    fn rejects_invalid_structure_and_event_variants() {
        assert!(parse(&PROGRAM.replace("version = 1", "version = 2")).is_err());
        assert!(parse(&PROGRAM.replace("id = \"main\"", "id = \"intro\"")).is_err());
        assert!(
            parse(&PROGRAM.replace(
                "before_end = \"1m2.5s\"",
                "at = \"1s\"\nbefore_end = \"2s\""
            ))
            .is_err()
        );
        assert!(
            parse(&PROGRAM.replace("payload_hex = \"01 02 ff\"", "payload_hex = \"xyz\"")).is_err()
        );
    }

    #[test]
    fn exact_duration_parser_accepts_supported_units_and_rejects_subnanoseconds() {
        assert_eq!(parse_duration_ns("500ms").unwrap(), 500_000_000);
        assert_eq!(parse_duration_ns("12.5s").unwrap(), 12_500_000_000);
        assert_eq!(parse_duration_ns("1m2.5s").unwrap(), 62_500_000_000);
        assert!(parse_duration_ns("0.0000000001s").is_err());
        assert!(parse_duration_ns("1.2.3s").is_err());
    }
}
