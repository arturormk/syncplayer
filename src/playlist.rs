//! Versioned playlist parsing and sequence timeline calculations.

use serde::Deserialize;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// The resolved, host-local portion of a version 1 playlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistConfig {
    pub looping: bool,
    pub videos: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistDocument {
    version: u64,
    playlist: PlaylistTable,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistTable {
    #[serde(rename = "loop")]
    looping: bool,
    videos: Vec<String>,
}

impl PlaylistConfig {
    /// Loads a playlist and resolves its entries relative to the playlist file.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable file, malformed or unsupported TOML,
    /// an empty playlist, or an invalid environment-variable expansion.
    pub fn load(path: &Path) -> Result<Self, PlaylistError> {
        let canonical = path
            .canonicalize()
            .map_err(|source| PlaylistError::Read(path.to_path_buf(), source))?;
        let text = fs::read_to_string(&canonical)
            .map_err(|source| PlaylistError::Read(canonical.clone(), source))?;
        let base = canonical.parent().unwrap_or_else(|| Path::new("."));
        parse(&text, base, |name| env::var(name).ok())
    }
}

fn parse(
    text: &str,
    base: &Path,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<PlaylistConfig, PlaylistError> {
    let document: PlaylistDocument = toml::from_str(text).map_err(PlaylistError::Toml)?;
    if document.version != 1 {
        return Err(PlaylistError::UnsupportedVersion(document.version));
    }
    if document.playlist.videos.is_empty() {
        return Err(PlaylistError::Empty);
    }

    let videos = document
        .playlist
        .videos
        .iter()
        .map(|entry| {
            let expanded = expand_environment(entry, &environment)?;
            let path = PathBuf::from(expanded);
            Ok(if path.is_absolute() {
                path
            } else {
                base.join(path)
            })
        })
        .collect::<Result<_, PlaylistError>>()?;
    Ok(PlaylistConfig {
        looping: document.playlist.looping,
        videos,
    })
}

fn expand_environment(
    input: &str,
    environment: &impl Fn(&str) -> Option<String>,
) -> Result<String, PlaylistError> {
    let chars: Vec<char> = input.chars().collect();
    let mut expanded = String::with_capacity(input.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '$' {
            expanded.push(chars[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(next) = chars.get(index).copied() else {
            return Err(PlaylistError::MalformedExpansion(input.to_owned()));
        };
        if next == '$' {
            expanded.push('$');
            index += 1;
            continue;
        }

        let name = if next == '{' {
            index += 1;
            let start = index;
            while chars.get(index).is_some_and(|character| *character != '}') {
                index += 1;
            }
            if index == start || index == chars.len() {
                return Err(PlaylistError::MalformedExpansion(input.to_owned()));
            }
            let name: String = chars[start..index].iter().collect();
            index += 1;
            name
        } else {
            let start = index;
            while chars
                .get(index)
                .is_some_and(|character| character.is_ascii_alphanumeric() || *character == '_')
            {
                index += 1;
            }
            if index == start {
                return Err(PlaylistError::MalformedExpansion(input.to_owned()));
            }
            chars[start..index].iter().collect()
        };
        if !valid_variable_name(&name) {
            return Err(PlaylistError::MalformedExpansion(input.to_owned()));
        }
        let value = environment(&name).ok_or(PlaylistError::UndefinedVariable(name))?;
        expanded.push_str(&value);
    }
    Ok(expanded)
}

fn valid_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[derive(Debug)]
pub enum PlaylistError {
    Read(PathBuf, std::io::Error),
    Toml(toml::de::Error),
    UnsupportedVersion(u64),
    Empty,
    MalformedExpansion(String),
    UndefinedVariable(String),
}

impl fmt::Display for PlaylistError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(path, error) => write!(formatter, "cannot read {}: {error}", path.display()),
            Self::Toml(error) => write!(formatter, "invalid playlist TOML: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported playlist version {version}; expected version 1"
                )
            }
            Self::Empty => formatter.write_str("playlist must contain at least one video"),
            Self::MalformedExpansion(value) => {
                write!(
                    formatter,
                    "malformed environment expansion in playlist entry {value:?}"
                )
            }
            Self::UndefinedVariable(name) => {
                write!(
                    formatter,
                    "playlist environment variable {name} is not defined"
                )
            }
        }
    }
}

impl std::error::Error for PlaylistError {}

/// A position within one item of a complete media sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ItemPosition {
    pub index: usize,
    pub local_ns: u64,
}

/// Durations and cumulative start offsets for a finite sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Timeline {
    durations_ns: Vec<u64>,
    offsets_ns: Vec<u64>,
    total_ns: u64,
    looping: bool,
}

impl Timeline {
    /// Builds a timeline from non-zero item durations.
    ///
    /// # Errors
    ///
    /// Rejects empty timelines, zero durations, and cumulative overflow.
    pub fn new(durations_ns: Vec<u64>, looping: bool) -> Result<Self, TimelineError> {
        if durations_ns.is_empty() {
            return Err(TimelineError::Empty);
        }
        let mut offsets_ns = Vec::with_capacity(durations_ns.len());
        let mut total_ns = 0_u64;
        for (index, duration_ns) in durations_ns.iter().copied().enumerate() {
            if duration_ns == 0 {
                return Err(TimelineError::ZeroDuration(index));
            }
            offsets_ns.push(total_ns);
            total_ns = total_ns
                .checked_add(duration_ns)
                .ok_or(TimelineError::DurationOverflow)?;
        }
        Ok(Self {
            durations_ns,
            offsets_ns,
            total_ns,
            looping,
        })
    }

    #[must_use]
    pub const fn total_ns(&self) -> u64 {
        self.total_ns
    }

    #[must_use]
    pub const fn is_looping(&self) -> bool {
        self.looping
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.durations_ns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.durations_ns.is_empty()
    }

    #[must_use]
    pub fn item_offset_ns(&self, index: usize) -> u64 {
        self.offsets_ns[index]
    }

    /// Maps an absolute sequence position to an item-local position.
    /// The exact final endpoint is `None` for a non-looping sequence.
    ///
    /// # Errors
    ///
    /// Returns an error when a non-looping position is beyond the endpoint.
    pub fn locate(&self, position_ns: u64) -> Result<Option<ItemPosition>, TimelineError> {
        let position_ns = if self.looping {
            position_ns % self.total_ns
        } else if position_ns == self.total_ns {
            return Ok(None);
        } else if position_ns > self.total_ns {
            return Err(TimelineError::PastEnd {
                position_ns,
                total_ns: self.total_ns,
            });
        } else {
            position_ns
        };
        let index = self
            .offsets_ns
            .partition_point(|offset| *offset <= position_ns)
            - 1;
        Ok(Some(ItemPosition {
            index,
            local_ns: position_ns - self.offsets_ns[index],
        }))
    }

    /// Advances an absolute sequence position without overflowing.
    ///
    /// # Errors
    ///
    /// Returns an error when a non-looping projection overflows or reaches
    /// the sequence endpoint.
    pub fn project(&self, position_ns: u64, lead_ns: u64) -> Result<u64, TimelineError> {
        if self.looping {
            let position = position_ns % self.total_ns;
            let lead = lead_ns % self.total_ns;
            let projected = (u128::from(position) + u128::from(lead)) % u128::from(self.total_ns);
            u64::try_from(projected).map_err(|_| TimelineError::DurationOverflow)
        } else {
            let projected = position_ns
                .checked_add(lead_ns)
                .ok_or(TimelineError::DurationOverflow)?;
            if projected >= self.total_ns {
                Err(TimelineError::PastEnd {
                    position_ns: projected,
                    total_ns: self.total_ns,
                })
            } else {
                Ok(projected)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineError {
    Empty,
    ZeroDuration(usize),
    DurationOverflow,
    PastEnd { position_ns: u64, total_ns: u64 },
}

impl fmt::Display for TimelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("media sequence is empty"),
            Self::ZeroDuration(index) => write!(formatter, "media item {index} has zero duration"),
            Self::DurationOverflow => {
                formatter.write_str("media sequence duration exceeds u64 nanoseconds")
            }
            Self::PastEnd {
                position_ns,
                total_ns,
            } => write!(
                formatter,
                "sequence position {position_ns} is at or beyond non-looping duration {total_ns}"
            ),
        }
    }
}

impl std::error::Error for TimelineError {}

/// Tracks the URI that an `about-to-finish` callback should enqueue next.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistCursor {
    item_count: usize,
    looping: bool,
    current: usize,
}

impl PlaylistCursor {
    #[must_use]
    pub const fn new(item_count: usize, looping: bool, current: usize) -> Self {
        Self {
            item_count,
            looping,
            current,
        }
    }

    pub fn reset(&mut self, current: usize) {
        self.current = current;
    }

    #[must_use]
    pub const fn current(&self) -> usize {
        self.current
    }

    pub fn queue_next(&mut self) -> Option<usize> {
        let next = self.current.checked_add(1)?;
        if next < self.item_count {
            self.current = next;
            Some(next)
        } else if self.looping && self.item_count > 0 {
            self.current = 0;
            Some(0)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1
[playlist]
loop = true
videos = ["a.mp4", "${HOST}/b.mp4", "price-$$5.mp4", "a.mp4"]
"#;

    fn parse_test(text: &str) -> Result<PlaylistConfig, PlaylistError> {
        parse(text, Path::new("/shows/demo"), |name| {
            (name == "HOST").then(|| "alpha".to_owned())
        })
    }

    #[test]
    fn parses_resolves_expands_and_preserves_duplicates() {
        let playlist = parse_test(VALID).unwrap();
        assert!(playlist.looping);
        assert_eq!(
            playlist.videos,
            [
                "/shows/demo/a.mp4",
                "/shows/demo/alpha/b.mp4",
                "/shows/demo/price-$5.mp4",
                "/shows/demo/a.mp4",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn keeps_absolute_paths_absolute_and_supports_unbraced_names() {
        let playlist =
            parse_test("version=1\n[playlist]\nloop=false\nvideos=['/media/$HOST.mp4']").unwrap();
        assert_eq!(playlist.videos, [PathBuf::from("/media/alpha.mp4")]);
    }

    #[test]
    fn rejects_unknown_fields_versions_and_empty_lists() {
        assert!(parse_test("version=1\nextra=2\n[playlist]\nloop=false\nvideos=['a']").is_err());
        assert!(matches!(
            parse_test("version=2\n[playlist]\nloop=false\nvideos=['a']"),
            Err(PlaylistError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            parse_test("version=1\n[playlist]\nloop=false\nvideos=[]"),
            Err(PlaylistError::Empty)
        ));
    }

    #[test]
    fn rejects_missing_and_malformed_variables() {
        for entry in ["$MISSING", "$", "${}", "${HOST", "$9BAD", "${BAD-NAME}"] {
            let text = format!("version=1\n[playlist]\nloop=false\nvideos=[{entry:?}]");
            assert!(parse_test(&text).is_err(), "accepted {entry}");
        }
    }

    #[test]
    fn timeline_maps_boundaries_and_final_endpoint() {
        let timeline = Timeline::new(vec![10, 20, 30], false).unwrap();
        assert_eq!(
            timeline.locate(9).unwrap(),
            Some(ItemPosition {
                index: 0,
                local_ns: 9
            })
        );
        assert_eq!(
            timeline.locate(10).unwrap(),
            Some(ItemPosition {
                index: 1,
                local_ns: 0
            })
        );
        assert_eq!(
            timeline.locate(30).unwrap(),
            Some(ItemPosition {
                index: 2,
                local_ns: 0
            })
        );
        assert_eq!(timeline.locate(60).unwrap(), None);
        assert!(matches!(
            timeline.locate(61),
            Err(TimelineError::PastEnd { .. })
        ));
    }

    #[test]
    fn looping_timeline_wraps_and_projects_large_leads() {
        let timeline = Timeline::new(vec![10, 20], true).unwrap();
        assert_eq!(
            timeline.locate(30).unwrap(),
            Some(ItemPosition {
                index: 0,
                local_ns: 0
            })
        );
        assert_eq!(
            timeline.locate(41).unwrap(),
            Some(ItemPosition {
                index: 1,
                local_ns: 1
            })
        );
        assert_eq!(timeline.project(29, u64::MAX).unwrap(), 14);
    }

    #[test]
    fn timeline_rejects_invalid_durations_and_ended_projection() {
        assert_eq!(
            Timeline::new(vec![1, 0], false),
            Err(TimelineError::ZeroDuration(1))
        );
        assert_eq!(
            Timeline::new(vec![u64::MAX, 1], false),
            Err(TimelineError::DurationOverflow)
        );
        let timeline = Timeline::new(vec![10], false).unwrap();
        assert!(matches!(
            timeline.project(8, 2),
            Err(TimelineError::PastEnd { .. })
        ));
    }

    #[test]
    fn cursor_sequences_non_looping_and_looping_items() {
        let mut cursor = PlaylistCursor::new(3, false, 0);
        assert_eq!(cursor.queue_next(), Some(1));
        assert_eq!(cursor.queue_next(), Some(2));
        assert_eq!(cursor.queue_next(), None);

        let mut cursor = PlaylistCursor::new(3, true, 2);
        assert_eq!(cursor.queue_next(), Some(0));
        assert_eq!(cursor.current(), 0);
        let mut one = PlaylistCursor::new(1, true, 0);
        assert_eq!(one.queue_next(), Some(0));
    }
}
