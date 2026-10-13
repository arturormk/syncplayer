//! Local-player media and event validation.

use crate::program::{EventTiming, ProgramHash, ResolvedCue, ResolvedProgram};
use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;
use gst_pbutils::prelude::DiscovererStreamInfoExt;
use gstreamer as gst;
use gstreamer_pbutils as gst_pbutils;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
pub const EXTERNAL_AUDIO_DURATION_TOLERANCE_NS: u64 = 50_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaMetadata {
    pub duration_ns: u64,
    pub stream_signature: String,
    pub has_video: bool,
    pub has_audio: bool,
}

pub trait MediaProbe {
    /// Inspects one local media file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is unavailable, unreadable, unsupported,
    /// or does not expose a usable duration.
    fn probe(&self, path: &Path) -> Result<MediaMetadata>;
}

pub struct GstMediaProbe {
    discoverer: gst_pbutils::Discoverer,
}

impl GstMediaProbe {
    /// Creates a cache-disabled synchronous media probe.
    ///
    /// # Errors
    ///
    /// Returns an error when `GStreamer`'s discoverer cannot be constructed.
    pub fn new() -> Result<Self> {
        let timeout = gst::ClockTime::from_nseconds(
            u64::try_from(PROBE_TIMEOUT.as_nanos()).context("probe timeout is too large")?,
        );
        let discoverer = gst_pbutils::Discoverer::new(timeout)
            .context("failed to create GStreamer media discoverer")?;
        discoverer.set_property("use-cache", false);
        Ok(Self { discoverer })
    }
}

impl MediaProbe for GstMediaProbe {
    fn probe(&self, path: &Path) -> Result<MediaMetadata> {
        if !path.is_file() {
            bail!("file does not exist");
        }
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
        let uri = gst::glib::filename_to_uri(&canonical, None)
            .map(String::from)
            .with_context(|| format!("cannot convert {} to a file URI", canonical.display()))?;
        let info = self
            .discoverer
            .discover_uri(&uri)
            .with_context(|| format!("failed to discover {}", path.display()))?;
        if info.result() != gst_pbutils::DiscovererResult::Ok {
            bail!("discovery result is {:?}", info.result());
        }
        let duration_ns = info
            .duration()
            .map(gst::ClockTime::nseconds)
            .filter(|duration| *duration > 0)
            .ok_or_else(|| anyhow!("media has no known non-zero duration"))?;
        let video_streams = info.video_streams();
        let audio_streams = info.audio_streams();
        let mut signatures = Vec::new();
        for stream in &video_streams {
            signatures.push(format!(
                "video:{}",
                stream
                    .caps()
                    .map_or_else(|| "unknown".to_owned(), |caps| caps.to_string())
            ));
        }
        for stream in &audio_streams {
            signatures.push(format!(
                "audio:{}",
                stream
                    .caps()
                    .map_or_else(|| "unknown".to_owned(), |caps| caps.to_string())
            ));
        }
        Ok(MediaMetadata {
            duration_ns,
            stream_signature: signatures.join("|"),
            has_video: !video_streams.is_empty(),
            has_audio: !audio_streams.is_empty(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticLevel {
    Warning,
    Error,
}

impl fmt::Display for DiagnosticLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warning => formatter.write_str("WARNING"),
            Self::Error => formatter.write_str("ERROR"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub cue_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedCue {
    pub id: String,
    pub video: PathBuf,
    pub audio: Option<PathBuf>,
    pub duration_ns: Option<u64>,
    pub event_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationReport {
    pub program_id: String,
    pub program_hash: ProgramHash,
    pub player_id: String,
    pub cues: Vec<ValidatedCue>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ValidationReport {
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.level == DiagnosticLevel::Error)
    }
}

pub fn validate_program(program: &ResolvedProgram, probe: &impl MediaProbe) -> ValidationReport {
    let mut diagnostics = Vec::new();
    let mut cues = Vec::with_capacity(program.cues.len());
    let mut signatures: Vec<Option<String>> = Vec::with_capacity(program.cues.len());

    for cue in &program.cues {
        let (validated, signature) = validate_cue(cue, probe, &mut diagnostics);
        cues.push(validated);
        signatures.push(signature);
    }

    for index in 1..signatures.len() {
        warn_on_transition(
            &program.cues[index - 1].id,
            &program.cues[index].id,
            signatures[index - 1].as_deref(),
            signatures[index].as_deref(),
            &mut diagnostics,
        );
    }
    if program.looping && signatures.len() > 1 {
        let last = signatures.len() - 1;
        warn_on_transition(
            &program.cues[last].id,
            &program.cues[0].id,
            signatures[last].as_deref(),
            signatures[0].as_deref(),
            &mut diagnostics,
        );
    }

    ValidationReport {
        program_id: program.program_id.clone(),
        program_hash: program.hash,
        player_id: program.player_id.to_string(),
        cues,
        diagnostics,
    }
}

fn validate_cue(
    cue: &ResolvedCue,
    probe: &impl MediaProbe,
    diagnostics: &mut Vec<Diagnostic>,
) -> (ValidatedCue, Option<String>) {
    let video = probe_media("video", &cue.id, &cue.video, probe, diagnostics);
    let audio = cue
        .audio
        .as_ref()
        .and_then(|path| probe_media("audio", &cue.id, path, probe, diagnostics));
    if video.as_ref().is_some_and(|metadata| !metadata.has_video) {
        diagnostics.push(error_diagnostic(
            &cue.id,
            format!("video {} contains no video stream", cue.video.display()),
        ));
    }
    if audio.as_ref().is_some_and(|metadata| !metadata.has_audio) {
        diagnostics.push(error_diagnostic(
            &cue.id,
            format!(
                "external audio {} contains no audio stream",
                cue.audio.as_ref().expect("audio path exists").display()
            ),
        ));
    }
    if let (Some(video), Some(audio)) = (&video, &audio) {
        let delta = video.duration_ns.abs_diff(audio.duration_ns);
        if delta > EXTERNAL_AUDIO_DURATION_TOLERANCE_NS {
            diagnostics.push(error_diagnostic(
                &cue.id,
                format!(
                    "external audio differs from video by {} ms",
                    format_milliseconds(delta)
                ),
            ));
        }
    }
    if let Some(video) = &video {
        validate_event_times(cue, video.duration_ns, diagnostics);
    }
    warn_on_pending_runtime(cue, diagnostics);
    let signature = video
        .as_ref()
        .map(|metadata| metadata.stream_signature.clone());
    (
        ValidatedCue {
            id: cue.id.clone(),
            video: cue.video.clone(),
            audio: cue.audio.clone(),
            duration_ns: video.map(|metadata| metadata.duration_ns),
            event_count: cue.events.len(),
        },
        signature,
    )
}

fn probe_media(
    kind: &str,
    cue_id: &str,
    path: &Path,
    probe: &impl MediaProbe,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<MediaMetadata> {
    match probe.probe(path) {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            diagnostics.push(error_diagnostic(
                cue_id,
                format!("{kind} {}: {error:#}", path.display()),
            ));
            None
        }
    }
}

fn validate_event_times(cue: &ResolvedCue, duration_ns: u64, diagnostics: &mut Vec<Diagnostic>) {
    for event in &cue.events {
        match event.timing {
            EventTiming::At(time) if time >= duration_ns => diagnostics.push(error_diagnostic(
                &cue.id,
                format!(
                    "event '{}' at {} s is at or beyond cue EOF {} s",
                    event.id,
                    format_seconds(time),
                    format_seconds(duration_ns)
                ),
            )),
            EventTiming::BeforeEnd(offset) if offset > duration_ns => {
                diagnostics.push(error_diagnostic(
                    &cue.id,
                    format!(
                        "event '{}' before_end {} s exceeds cue duration {} s",
                        event.id,
                        format_seconds(offset),
                        format_seconds(duration_ns)
                    ),
                ));
            }
            _ => {}
        }
    }
}

fn warn_on_pending_runtime(cue: &ResolvedCue, diagnostics: &mut Vec<Diagnostic>) {
    let mut pending = Vec::new();
    if !cue.events.is_empty() {
        pending.push("timed events");
    }
    if !pending.is_empty() {
        diagnostics.push(warning_diagnostic(
            &cue.id,
            format!(
                "configured {} will be rejected by playback until its runtime engine is implemented",
                pending.join(", ")
            ),
        ));
    }
}

fn warn_on_transition(
    from: &str,
    to: &str,
    from_signature: Option<&str>,
    to_signature: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let (Some(from_signature), Some(to_signature)) = (from_signature, to_signature)
        && from_signature != to_signature
    {
        diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Warning,
            cue_id: None,
            message: format!(
                "transition {from} -> {to} changes stream caps and may require pipeline reconfiguration"
            ),
        });
    }
}

fn error_diagnostic(cue_id: &str, message: String) -> Diagnostic {
    Diagnostic {
        level: DiagnosticLevel::Error,
        cue_id: Some(cue_id.to_owned()),
        message,
    }
}

fn warning_diagnostic(cue_id: &str, message: String) -> Diagnostic {
    Diagnostic {
        level: DiagnosticLevel::Warning,
        cue_id: Some(cue_id.to_owned()),
        message,
    }
}

fn format_seconds(value: u64) -> String {
    format!(
        "{}.{:03}",
        value / 1_000_000_000,
        value % 1_000_000_000 / 1_000_000
    )
}

fn format_milliseconds(value: u64) -> String {
    format!("{}.{:03}", value / 1_000_000, value % 1_000_000 / 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::{Event, EventAction, PlayerId, ResolvedCue};
    use std::collections::BTreeMap;

    struct FakeProbe(BTreeMap<PathBuf, MediaMetadata>);

    impl MediaProbe for FakeProbe {
        fn probe(&self, path: &Path) -> Result<MediaMetadata> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow!("file does not exist"))
        }
    }

    #[test]
    fn aggregates_media_timing_runtime_and_transition_diagnostics() {
        let video_a = PathBuf::from("/media/a.mp4");
        let audio_a = PathBuf::from("/media/a.flac");
        let missing = PathBuf::from("/media/missing.mp4");
        let program = ResolvedProgram {
            program_id: "demo".to_owned(),
            hash: ProgramHash::from_bytes([7; 32]),
            player_id: PlayerId::new("left").unwrap(),
            looping: true,
            cues: vec![
                ResolvedCue {
                    id: "a".to_owned(),
                    video: video_a.clone(),
                    audio: Some(audio_a.clone()),
                    mute: false,
                    gain_db: 0.0,
                    events: vec![Event {
                        id: "late".to_owned(),
                        timing: EventTiming::At(11_000_000_000),
                        action: EventAction::Process {
                            command: vec!["true".to_owned()],
                        },
                    }],
                },
                ResolvedCue {
                    id: "b".to_owned(),
                    video: missing,
                    audio: None,
                    mute: false,
                    gain_db: 0.0,
                    events: Vec::new(),
                },
            ],
        };
        let probe = FakeProbe(BTreeMap::from([
            (
                video_a,
                MediaMetadata {
                    duration_ns: 10_000_000_000,
                    stream_signature: "video:a".to_owned(),
                    has_video: true,
                    has_audio: true,
                },
            ),
            (
                audio_a,
                MediaMetadata {
                    duration_ns: 9_900_000_000,
                    stream_signature: "audio:a".to_owned(),
                    has_video: false,
                    has_audio: true,
                },
            ),
        ]));

        let report = validate_program(&program, &probe);
        assert!(report.has_errors());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.message.contains("differs"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.message.contains("beyond cue EOF"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.message.contains("runtime engine"))
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.message.contains("file does not exist"))
        );
    }

    #[test]
    fn clean_program_has_no_diagnostics() {
        let path = PathBuf::from("/media/a.mp4");
        let program = ResolvedProgram {
            program_id: "demo".to_owned(),
            hash: ProgramHash::from_bytes([0; 32]),
            player_id: PlayerId::new("left").unwrap(),
            looping: false,
            cues: vec![ResolvedCue {
                id: "a".to_owned(),
                video: path.clone(),
                audio: None,
                mute: false,
                gain_db: 0.0,
                events: Vec::new(),
            }],
        };
        let probe = FakeProbe(BTreeMap::from([(
            path,
            MediaMetadata {
                duration_ns: 1,
                stream_signature: "video:a".to_owned(),
                has_video: true,
                has_audio: false,
            },
        )]));
        let report = validate_program(&program, &probe);
        assert!(!report.has_errors());
        assert_eq!(report.diagnostics, []);
    }
}
