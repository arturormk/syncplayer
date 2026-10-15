use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gst::prelude::*;
use gst_gl::prelude::ContextGLExt;
use gstreamer as gst;
use gstreamer_gl as gst_gl;
use gstreamer_net as gst_net;
use gstreamer_pbutils as gst_pbutils;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use syncplayer::console::{ConsoleCommand, HELP as CONSOLE_HELP, OsdCommand, parse_command};
use syncplayer::diagnostics::DiagnosticsHub;
use syncplayer::events::{EventCue, EventObservation, EventRuntime};
use syncplayer::playlist::{ItemPosition, PlaylistCursor, Timeline};
use syncplayer::program::{ProgramConfig, ProgramHash, ResolvedProgram};
use syncplayer::protocol::{
    Capabilities, DebugLoopState, DiagnosticTelemetry, MAX_STATE_PACKET_BYTES,
    MAX_SUBSCRIPTION_CUES, MasterSession, Message, Operation, ReleasePlan, StateDatagram,
    StateSnapshot, Subscription, TransportState, decode_state_datagram, encode_state_datagram,
    read_message, write_message,
};
use syncplayer::subscriber::{ParticipantLease, ParticipantRegistry, Readiness, StateEndpoint};
use syncplayer::time::{ClockNs, MediaPositionNs};
use syncplayer::transport::{EosDecision, RuntimeState, TransportController};
use syncplayer::validation::{
    DiagnosticLevel, EXTERNAL_AUDIO_DURATION_TOLERANCE_NS, GstMediaProbe, validate_program,
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

const PREROLL_TIMEOUT: Duration = Duration::from_secs(30);
const CLOCK_SYNC_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MASTER_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const REJOIN_PREPARATION_TIMEOUT: Duration = Duration::from_secs(95);
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(1);
const DRIFT_WARN_THRESHOLD_NS: u128 = 100_000_000;
const DRIFT_REPAIR_THRESHOLD_NS: u128 = 250_000_000;
const DRIFT_REQUIRED_SAMPLES: u32 = 3;
const DRIFT_RECOVERY_COOLDOWN: Duration = Duration::from_secs(10);
const DRIFT_SLEW_SETTLE_THRESHOLD_NS: u128 = 40_000_000;
const DRIFT_SLEW_CORRECTION_WINDOW_SECS: f64 = 10.0;
const DRIFT_SLEW_MAX_ADJUSTMENT: f64 = 0.02;
const DRIFT_SLEW_RATE_UPDATE_THRESHOLD: f64 = 0.001;
const LOCAL_RECOVERY_LEAD: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(version, about = "Syncplayer two-host shared-clock experiment")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Play normally, export the pipeline clock, and welcome Slaves.
    Master(MasterArgs),
    /// Discipline a local pipeline to a Master's clock.
    Slave(SlaveArgs),
    /// Validate a shared program for the selected local player.
    Validate(ValidateArgs),
}

#[derive(Debug, Args)]
struct MasterArgs {
    #[command(flatten)]
    playback: PlaybackArgs,

    /// TCP address for the experimental control connection.
    #[arg(long, default_value = "0.0.0.0:46000")]
    bind: SocketAddr,

    /// UDP address used to send independent state snapshots.
    #[arg(long, default_value = "0.0.0.0:0")]
    state_bind: SocketAddr,

    /// Complete state snapshots sent per second (1-100).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..=100))]
    state_rate_hz: u16,

    /// Drop a participant after this many seconds without a control keepalive.
    #[arg(long, default_value_t = 8.0, value_parser = positive_seconds)]
    keepalive_timeout: f64,

    /// UDP port used by `GstNetTimeProvider`.
    #[arg(long, default_value_t = 46_001)]
    clock_port: u16,

    /// Seconds between sending a release plan and its clock deadline.
    #[arg(long, default_value_t = 5.0, value_parser = positive_seconds)]
    release_delay: f64,

    /// Best-effort startup grace period in seconds; should exceed `--release-delay`.
    #[arg(long, default_value_t = 0.0, value_parser = non_negative_seconds)]
    startup_delay: f64,

    /// Maximum allowed Master/Slave cue-duration difference in milliseconds.
    #[arg(long, default_value_t = 100)]
    duration_tolerance_ms: u64,

    /// Action when a cue exceeds the duration tolerance.
    #[arg(long, value_enum, default_value_t = DurationMismatchPolicy::Reject)]
    duration_mismatch: DurationMismatchPolicy,

    /// Append centralized Master/Slave playback observations to this CSV file.
    #[arg(long, value_name = "PATH")]
    diagnostics_log: Option<PathBuf>,

    /// Enable the installation console on this explicit TCP address.
    #[arg(long, value_name = "IP:PORT")]
    console_bind: Option<SocketAddr>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum DurationMismatchPolicy {
    Warn,
    Reject,
}

#[derive(Debug, Args)]
struct SlaveArgs {
    #[command(flatten)]
    playback: PlaybackArgs,

    /// Master's experimental TCP control address.
    #[arg(long)]
    master: SocketAddr,

    /// UDP address on which to receive independent state snapshots.
    #[arg(long, default_value = "0.0.0.0:0")]
    state_bind: SocketAddr,

    /// Seconds between reliable control-channel keepalives.
    #[arg(long, default_value_t = 2.0, value_parser = positive_seconds)]
    keepalive_interval: f64,

    /// Seconds to wait for a keepalive acknowledgement before reconnecting.
    #[arg(long, default_value_t = 8.0, value_parser = positive_seconds)]
    keepalive_timeout: f64,

    /// Gently vary local playback speed to correct sustained 100-250 ms drift.
    #[arg(long)]
    drift_slew: bool,
}

#[derive(Debug, Args)]
struct PlaybackArgs {
    /// Versioned shared program definition.
    #[arg(value_name = "PROGRAM")]
    program: PathBuf,

    /// Logical player identity. Defaults to the hostname, with alias lookup.
    #[arg(long)]
    player_id: Option<String>,

    /// Silence the selected audio stream.
    #[arg(long)]
    mute: bool,

    /// Add this many decibels to each cue's configured gain.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true, value_parser = finite_f64)]
    gain_db: f64,

    /// Ask the selected video sink to occupy its display.
    #[arg(long)]
    fullscreen: bool,

    /// Render directly through EGL/GBM and composite the OSD on the GPU.
    #[arg(long, conflicts_with_all = ["video_sink", "display"])]
    kms_gl: bool,

    /// Width of the video output canvas in pixels.
    #[arg(long, requires = "height", value_parser = clap::value_parser!(u32).range(1..))]
    width: Option<u32>,

    /// Height of the video output canvas in pixels.
    #[arg(long, requires = "width", value_parser = clap::value_parser!(u32).range(1..))]
    height: Option<u32>,

    /// Platform display/output identifier (for example WAYLAND display name or DRM connector ID).
    #[arg(long)]
    display: Option<String>,

    /// Video scaling policy.
    #[arg(long, value_enum, default_value_t = FitMode::Contain)]
    fit: FitMode,

    /// Master playback clock policy. Slaves always use the Master's network clock.
    #[arg(long, value_enum, default_value_t = ClockPolicy::Auto)]
    clock: ClockPolicy,

    /// Hardware decoder policy.
    #[arg(long, value_enum, default_value_t = HardwareDecoding::Auto)]
    hwdec: HardwareDecoding,

    /// Override `GStreamer`'s automatically selected video sink.
    #[arg(long)]
    video_sink: Option<String>,

    /// Override `GStreamer`'s automatically selected audio sink.
    #[arg(long)]
    audio_sink: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum FitMode {
    Contain,
    Cover,
    Stretch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ClockPolicy {
    Auto,
    Audio,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum HardwareDecoding {
    Auto,
    Off,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// Versioned shared program definition.
    #[arg(value_name = "PROGRAM")]
    program: PathBuf,

    /// Logical player identity. Defaults to the hostname, with alias lookup.
    #[arg(long)]
    player_id: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    gst::init().context("failed to initialize GStreamer")?;

    match Cli::parse().command {
        Command::Master(args) => run_master(&args),
        Command::Slave(args) => run_slave(&args),
        Command::Validate(args) => run_validate(&args),
    }
}

#[allow(clippy::too_many_lines)]
fn run_master(args: &MasterArgs) -> Result<()> {
    validate_playback_args(&args.playback)?;
    // Let the pipeline choose its natural clock. With a real audio sink this
    // is normally the device-backed audio clock, so the Master does not have
    // to slave its own output to an unrelated system clock.
    let program = load_runtime_program(&args.playback)?;
    let sequence = PlaybackSequence::prepare(&program)?;
    let compatibility = ProgramCompatibility {
        program_id: program.program_id.clone(),
        program_hash: program.hash,
        master_player_id: program.player_id.to_string(),
        cue_durations_ns: sequence.cue_durations_ns.clone(),
        duration_tolerance_ns: args
            .duration_tolerance_ms
            .checked_mul(1_000_000)
            .ok_or_else(|| anyhow!("duration tolerance is too large"))?,
        mismatch_policy: args.duration_mismatch,
    };
    let player = Arc::new(Player::new(&args.playback, sequence, None, false)?);
    let clock = player.clock()?;
    let bind_ip = match args.bind.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => ip.to_string(),
    };
    let provider =
        gst_net::NetTimeProvider::new(&clock, Some(&bind_ip), i32::from(args.clock_port))
            .context("failed to expose the Master clock")?;
    if !provider.is_active() {
        bail!("GStreamer network time provider did not become active");
    }

    let listener = TcpListener::bind(args.bind)
        .with_context(|| format!("failed to bind control socket {}", args.bind))?;
    let state_socket = Arc::new(
        UdpSocket::bind(args.state_bind)
            .with_context(|| format!("failed to bind state socket {}", args.state_bind))?,
    );
    let clock_port = provider
        .port()
        .try_into()
        .context("invalid provider port")?;
    let startup_delay = Duration::from_secs_f64(args.startup_delay);
    let release_delay = Duration::from_secs_f64(args.release_delay);
    let state_interval = Duration::from_secs_f64(1.0 / f64::from(args.state_rate_hz));
    let keepalive_timeout = Duration::from_secs_f64(args.keepalive_timeout);
    let session = new_master_session()?;
    let diagnostics = Arc::new(
        DiagnosticsHub::new(args.diagnostics_log.as_deref())
            .context("failed to open diagnostics log")?,
    );
    let initial_plan = player.schedule_start(startup_delay)?;
    let console_listener = args
        .console_bind
        .map(TcpListener::bind)
        .transpose()
        .with_context(|| {
            format!(
                "failed to bind installation console {}",
                args.console_bind.expect("console address exists")
            )
        })?;
    let mut transport = TransportController::new(
        session,
        program.program_id.clone(),
        program.hash,
        program.cues.iter().map(|cue| cue.id.clone()).collect(),
        compatibility.cue_durations_ns.clone(),
        program.looping,
        ClockNs::new(clock.time().nseconds()),
    )
    .context("failed to create authoritative transport")?;
    transport.start(initial_plan.release_at);
    let generation = transport.generation();
    if startup_delay.is_zero() {
        info!(
            control = %listener.local_addr()?,
            state = %state_socket.local_addr()?,
            state_rate_hz = args.state_rate_hz,
            clock_port,
            master_session = %session,
            transport_generation = generation,
            "Master playback started; ready to welcome Slaves"
        );
    } else {
        info!(
            control = %listener.local_addr()?,
            state = %state_socket.local_addr()?,
            state_rate_hz = args.state_rate_hz,
            clock_port,
            master_session = %session,
            transport_generation = generation,
            release_at_ns = initial_plan.release_at.get(),
            startup_delay_s = args.startup_delay,
            "Master prerolled on the first frame; synchronized startup scheduled"
        );
    }

    let server = Arc::new(SlaveServer {
        player: player.clone(),
        clock,
        clock_port,
        release_delay,
        operation_plan: Mutex::new(initial_plan),
        transport: Arc::new(Mutex::new(transport)),
        compatibility,
        registry: ParticipantRegistry::new(),
        state_socket,
        state_interval,
        keepalive_timeout,
        diagnostics: diagnostics.clone(),
        operator_lock: Mutex::new(()),
    });
    broadcast_state(server.clone());
    serve_slaves(listener, server.clone());
    if let Some(console_listener) = console_listener {
        info!(console = %console_listener.local_addr()?, "installation console enabled");
        serve_console(console_listener, server.clone());
    }
    player.monitor(
        initial_plan,
        Some(&server.transport),
        Some((diagnostics.as_ref(), program.player_id.as_str())),
    )
}

fn run_slave(args: &SlaveArgs) -> Result<()> {
    validate_playback_args(&args.playback)?;
    let program = load_runtime_program(&args.playback)?;
    let sequence = PlaybackSequence::prepare(&program)?;
    let state_socket = UdpSocket::bind(args.state_bind)
        .with_context(|| format!("failed to bind state receive socket {}", args.state_bind))?;
    let state_port = state_socket.local_addr()?.port();
    if state_port == 0 {
        bail!("state receive socket did not obtain a usable UDP port");
    }
    let subscription = create_subscription(&program, &sequence, state_port)?;
    let (mut stream, clock, authority) = wait_for_master(args.master, &subscription);
    let player = Arc::new(Player::new(
        &args.playback,
        sequence,
        Some(&clock),
        args.drift_slew,
    )?);
    let plan = receive_release(&mut stream, &clock, authority)?;
    player.apply_control_plan(&clock, plan)?;
    report_prepared(&mut stream, authority)?;
    info!(
        release_at_ns = plan.release_at.get(),
        drift_slew = args.drift_slew,
        "accepted shared-clock release"
    );

    monitor_master(
        stream,
        state_socket,
        args.master,
        authority,
        player.clone(),
        subscription,
        KeepaliveConfig {
            interval: Duration::from_secs_f64(args.keepalive_interval),
            timeout: Duration::from_secs_f64(args.keepalive_timeout),
        },
    );
    player.monitor(plan, None, None)
}

fn create_subscription(
    program: &ResolvedProgram,
    sequence: &PlaybackSequence,
    state_port: u16,
) -> Result<Subscription> {
    if sequence.cue_durations_ns.len() > MAX_SUBSCRIPTION_CUES {
        bail!(
            "program has {} cues; subscription supports at most {MAX_SUBSCRIPTION_CUES}",
            sequence.cue_durations_ns.len()
        );
    }
    Ok(Subscription {
        player_id: program.player_id.to_string(),
        program_id: program.program_id.clone(),
        program_hash: program.hash,
        software_version: env!("CARGO_PKG_VERSION").to_owned(),
        capabilities: Capabilities::current(),
        state_port,
        cue_durations_ns: sequence.cue_durations_ns.clone(),
    })
}

fn load_runtime_program(args: &PlaybackArgs) -> Result<ResolvedProgram> {
    let config = ProgramConfig::load(&args.program)
        .with_context(|| format!("failed to load program {}", args.program.display()))?;
    let program = config.resolve_player(args.player_id.as_deref())?;
    info!(
        program = %program.program_id,
        program_hash = %program.hash,
        player = %program.player_id,
        cue_count = program.cues.len(),
        "shared program resolved"
    );
    Ok(program)
}

fn run_validate(args: &ValidateArgs) -> Result<()> {
    let config = ProgramConfig::load(&args.program)
        .with_context(|| format!("failed to load program {}", args.program.display()))?;
    let program = config.resolve_player(args.player_id.as_deref())?;
    let probe = GstMediaProbe::new()?;
    let report = validate_program(&program, &probe);

    println!("Program: {}", report.program_id);
    println!("Hash:    {}", report.program_hash);
    println!("Player:  {}", report.player_id);
    println!();
    println!("Cues:");
    for cue in &report.cues {
        let duration = cue.duration_ns.map_or_else(
            || "unavailable".to_owned(),
            |value| {
                format!(
                    "{}.{:03} s",
                    value / 1_000_000_000,
                    value % 1_000_000_000 / 1_000_000
                )
            },
        );
        println!("  {:<20} {:<12} {}", cue.id, duration, cue.video.display());
        if let Some(audio) = &cue.audio {
            println!("  {:<20} {:<12} {}", "external audio", "", audio.display());
        }
        if cue.event_count > 0 {
            println!("  {:<20} {} event(s)", "events", cue.event_count);
        }
    }
    if !report.diagnostics.is_empty() {
        println!();
        println!("Diagnostics:");
        for diagnostic in &report.diagnostics {
            let cue = diagnostic
                .cue_id
                .as_deref()
                .map_or_else(String::new, |cue| format!(" [{cue}]"));
            println!("  {}{cue}: {}", diagnostic.level, diagnostic.message);
        }
    }
    if report.has_errors() {
        bail!("program validation failed");
    }
    let warnings = report
        .diagnostics
        .iter()
        .filter(|item| item.level == DiagnosticLevel::Warning)
        .count();
    println!();
    println!("Validation succeeded with {warnings} warning(s).");
    Ok(())
}

fn wait_for_master(
    master: SocketAddr,
    subscription: &Subscription,
) -> (TcpStream, gst::Clock, Authority) {
    loop {
        match connect_to_master(master, subscription) {
            Ok(connection) => return connection,
            Err(error) => {
                warn!(
                    %master,
                    %error,
                    retry_in_s = MASTER_RETRY_INTERVAL.as_secs(),
                    "Master connection attempt failed; retrying"
                );
                thread::sleep(MASTER_RETRY_INTERVAL);
            }
        }
    }
}

fn serve_slaves(listener: TcpListener, server: Arc<SlaveServer>) {
    thread::spawn(move || {
        for accepted in listener.incoming() {
            let stream = match accepted {
                Ok(connection) => connection,
                Err(error) => {
                    error!(%error, "failed to accept Slave");
                    continue;
                }
            };
            let peer = match stream.peer_addr() {
                Ok(peer) => peer,
                Err(error) => {
                    warn!(%error, "could not identify connected Slave");
                    continue;
                }
            };
            info!(%peer, "Slave connected; sending synchronization welcome");
            let server = server.clone();
            thread::spawn(move || {
                if let Err(error) = welcome_slave(stream, peer, &server) {
                    warn!(%peer, %error, "Slave synchronization session ended");
                }
            });
        }
    });
}

fn serve_console(listener: TcpListener, server: Arc<SlaveServer>) {
    thread::spawn(move || {
        for accepted in listener.incoming() {
            let stream = match accepted {
                Ok(stream) => stream,
                Err(error) => {
                    warn!(%error, "failed to accept console connection");
                    continue;
                }
            };
            let peer = stream.peer_addr().ok();
            let server = server.clone();
            thread::spawn(move || {
                if let Err(error) = run_console_session(stream, &server) {
                    warn!(?peer, %error, "console session ended with an error");
                }
            });
        }
    });
}

fn run_console_session(mut stream: TcpStream, server: &SlaveServer) -> Result<()> {
    writeln!(stream, "Syncplayer installation console")?;
    writeln!(stream, "{CONSOLE_HELP}")?;
    write!(stream, "> ")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let response = if line.len() > 4096 {
            "error: command exceeds 4096 bytes".to_owned()
        } else {
            match parse_command(&line) {
                Ok(command) => execute_console_command(server, command)
                    .unwrap_or_else(|error| format!("error: {error:#}")),
                Err(error) => format!("error: {error}"),
            }
        };
        writeln!(stream, "{response}")?;
        write!(stream, "> ")?;
        stream.flush()?;
    }
}

#[allow(clippy::too_many_lines)]
fn execute_console_command(server: &SlaveServer, command: ConsoleCommand) -> Result<String> {
    let _operator = if matches!(
        &command,
        ConsoleCommand::Help | ConsoleCommand::Status | ConsoleCommand::Diagnostics
    ) {
        None
    } else {
        Some(
            server
                .operator_lock
                .lock()
                .map_err(|_| anyhow!("operator lock poisoned"))?,
        )
    };
    match command {
        ConsoleCommand::Help => Ok(CONSOLE_HELP.to_owned()),
        ConsoleCommand::Status => console_status(server),
        ConsoleCommand::Diagnostics => console_diagnostics(server),
        ConsoleCommand::Pause => {
            let release_at = console_release_at(server, console_operation_delay(server)?)?;
            let mut transport = server
                .transport
                .lock()
                .map_err(|_| anyhow!("transport lock poisoned"))?;
            transport.pause(release_at)?;
            let runtime = transport.snapshot(release_at);
            drop(transport);
            let plan = plan_from_runtime(server, runtime, Operation::Pause, release_at)?;
            publish_operation_plan(server, plan)?;
            server.player.schedule_pause(plan)?;
            Ok(format!(
                "pause scheduled at shared clock {}",
                release_at.get()
            ))
        }
        ConsoleCommand::Play { delay } => {
            let minimum_delay = console_operation_delay(server)?;
            let delay = delay.unwrap_or(minimum_delay);
            if delay < minimum_delay {
                bail!(
                    "play delay must be at least {:.3} seconds so participants retain a preparation window",
                    minimum_delay.as_secs_f64()
                );
            }
            let release_at = console_release_at(server, delay)?;
            let mut transport = server
                .transport
                .lock()
                .map_err(|_| anyhow!("transport lock poisoned"))?;
            transport.release(release_at)?;
            let runtime = transport.snapshot(release_at);
            drop(transport);
            let plan = plan_from_runtime(server, runtime, Operation::Resume, release_at)?;
            publish_operation_plan(server, plan)?;
            server
                .player
                .resynchronize(&server.clock, plan, "operator-play")?;
            Ok(format!(
                "play scheduled at shared clock {}",
                release_at.get()
            ))
        }
        ConsoleCommand::Stop => {
            let now = ClockNs::new(server.clock.time().nseconds());
            let release_at = console_release_at(server, console_operation_delay(server)?)?;
            let mut transport = server
                .transport
                .lock()
                .map_err(|_| anyhow!("transport lock poisoned"))?;
            transport.stop(now)?;
            let runtime = transport.snapshot(now);
            drop(transport);
            let plan = plan_from_runtime(server, runtime, Operation::Stop, release_at)?;
            publish_operation_plan(server, plan)?;
            server
                .player
                .prepare_paused(&server.clock, plan, "operator-stop")?;
            Ok("stopped at the first cue".to_owned())
        }
        ConsoleCommand::Goto { cue, time } => {
            let cue_index = match cue {
                Some(cue) => server
                    .player
                    .sequence
                    .cue_ids
                    .iter()
                    .position(|candidate| candidate == &cue)
                    .ok_or_else(|| anyhow!("unknown cue '{cue}'"))?,
                None => {
                    server
                        .transport
                        .lock()
                        .map_err(|_| anyhow!("transport lock poisoned"))?
                        .snapshot(ClockNs::new(server.clock.time().nseconds()))
                        .cue_index
                }
            };
            let cue_position_ns =
                u64::try_from(time.as_nanos()).context("goto time is too large")?;
            let release_at = console_release_at(server, console_operation_delay(server)?)?;
            let mut transport = server
                .transport
                .lock()
                .map_err(|_| anyhow!("transport lock poisoned"))?;
            transport.goto(cue_index, cue_position_ns, release_at)?;
            let runtime = transport.snapshot(release_at);
            let playing = runtime.transport_state != TransportState::Paused;
            drop(transport);
            let plan = plan_from_runtime(server, runtime, Operation::Goto, release_at)?;
            publish_operation_plan(server, plan)?;
            if playing {
                server
                    .player
                    .resynchronize(&server.clock, plan, "operator-goto")?;
            } else {
                server
                    .player
                    .prepare_paused(&server.clock, plan, "operator-goto")?;
            }
            Ok(format!(
                "goto scheduled for cue {}",
                server.player.sequence.cue_ids[cue_index]
            ))
        }
        ConsoleCommand::Loop { duration } => {
            let now = ClockNs::new(server.clock.time().nseconds());
            let duration_ns =
                u64::try_from(duration.as_nanos()).context("loop duration is too large")?;
            if duration_ns == 0 {
                bail!("loop duration must be positive");
            }
            let release_at = console_release_at(server, console_operation_delay(server)?)?;
            let mut transport = server
                .transport
                .lock()
                .map_err(|_| anyhow!("transport lock poisoned"))?;
            let current = transport.snapshot(now);
            let end_ns = current
                .anchor_cue_position
                .get()
                .checked_add(duration_ns)
                .ok_or_else(|| anyhow!("loop endpoint overflow"))?;
            transport.set_debug_loop(current.anchor_cue_position.get(), end_ns, false, now)?;
            let runtime = transport.snapshot(release_at);
            let playing = runtime.transport_state != TransportState::Paused;
            drop(transport);
            let plan = plan_from_runtime(server, runtime, Operation::Loop, release_at)?;
            publish_operation_plan(server, plan)?;
            if playing {
                server
                    .player
                    .resynchronize(&server.clock, plan, "operator-loop")?;
            } else {
                server
                    .player
                    .prepare_paused(&server.clock, plan, "operator-loop")?;
            }
            Ok("diagnostic loop enabled; timed events are suppressed".to_owned())
        }
        ConsoleCommand::Unloop => {
            let now = ClockNs::new(server.clock.time().nseconds());
            let release_at = console_release_at(server, console_operation_delay(server)?)?;
            let mut transport = server
                .transport
                .lock()
                .map_err(|_| anyhow!("transport lock poisoned"))?;
            if transport.snapshot(now).debug_loop.is_none() {
                return Ok("diagnostic loop is not active".to_owned());
            }
            transport.clear_debug_loop(now)?;
            let runtime = transport.snapshot(release_at);
            let playing = runtime.transport_state != TransportState::Paused;
            drop(transport);
            let plan = plan_from_runtime(server, runtime, Operation::Unloop, release_at)?;
            publish_operation_plan(server, plan)?;
            if playing {
                server
                    .player
                    .resynchronize(&server.clock, plan, "operator-unloop")?;
            } else {
                server
                    .player
                    .prepare_paused(&server.clock, plan, "operator-unloop")?;
            }
            Ok("diagnostic loop disabled".to_owned())
        }
        ConsoleCommand::Osd(command) => {
            let enabled = match command {
                OsdCommand::On => true,
                OsdCommand::Off => false,
                OsdCommand::Toggle => !server.player.osd_enabled(),
            };
            server.player.set_osd_enabled(enabled)?;
            Ok(format!(
                "OSD {} on all players",
                if enabled { "enabled" } else { "disabled" }
            ))
        }
    }
}

fn console_release_at(server: &SlaveServer, delay: Duration) -> Result<ClockNs> {
    ClockNs::new(server.clock.time().nseconds())
        .checked_add(delay)
        .ok_or_else(|| anyhow!("operation deadline overflow"))
}

fn console_operation_delay(server: &SlaveServer) -> Result<Duration> {
    server
        .release_delay
        .checked_mul(2)
        .ok_or_else(|| anyhow!("operation preparation delay overflow"))
}

fn plan_from_runtime(
    server: &SlaveServer,
    runtime: RuntimeState,
    operation: Operation,
    release_at: ClockNs,
) -> Result<ReleasePlan> {
    let generation = runtime.generation;
    let snapshot = server.player.authoritative_snapshot(runtime)?;
    Ok(ReleasePlan {
        generation,
        operation,
        transport_state: snapshot.transport_state,
        media_position: snapshot.media_position,
        release_at,
    })
}

fn publish_operation_plan(server: &SlaveServer, plan: ReleasePlan) -> Result<()> {
    *server
        .operation_plan
        .lock()
        .map_err(|_| anyhow!("operation plan lock poisoned"))? = plan;
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn console_status(server: &SlaveServer) -> Result<String> {
    let now = ClockNs::new(server.clock.time().nseconds());
    let runtime = server
        .transport
        .lock()
        .map_err(|_| anyhow!("transport lock poisoned"))?
        .snapshot(now);
    let participants = server
        .registry
        .snapshot(Instant::now(), server.keepalive_timeout)?;
    let mut lines = vec![format!(
        "master session={} generation={} state={:?} cue={} time={:.3}s loop={} osd={}",
        runtime.session,
        runtime.generation,
        runtime.transport_state,
        runtime.cue_id,
        runtime.anchor_cue_position.get() as f64 / 1_000_000_000.0,
        runtime.debug_loop.is_some(),
        if server.player.osd_enabled() {
            "on"
        } else {
            "off"
        }
    )];
    if participants.is_empty() {
        lines.push("slaves: none".to_owned());
    } else {
        for participant in participants {
            lines.push(format!(
                "slave {} remote={} readiness={:?} clock={} last_seen={}ms",
                participant.player_id,
                participant.remote_address,
                participant.readiness,
                if participant.clock_synchronized {
                    "ok"
                } else {
                    "syncing"
                },
                participant.last_seen_age.as_millis()
            ));
        }
    }
    Ok(lines.join("\n"))
}

#[allow(clippy::cast_precision_loss)]
fn console_diagnostics(server: &SlaveServer) -> Result<String> {
    let summaries = server.diagnostics.summaries()?;
    if summaries.is_empty() {
        return Ok("no diagnostic samples collected yet".to_owned());
    }
    Ok(summaries
        .into_iter()
        .map(|summary| {
            let rate = summary.rate_error_ppm.map_or_else(
                || "warming-up".to_owned(),
                |ppm| format!("{ppm:+.1}ppm ({:+.4}%)", ppm / 10_000.0),
            );
            format!(
                "{} samples={}/{} valid error={:+.3}ms min={:+.3}ms max={:+.3}ms rms={:.3}ms rate={} disconnects={} recoveries={} playing={} transition={} clock={}",
                summary.player_id,
                summary.valid_samples,
                summary.samples,
                summary.latest_error_ns as f64 / 1_000_000.0,
                summary.minimum_error_ns as f64 / 1_000_000.0,
                summary.maximum_error_ns as f64 / 1_000_000.0,
                summary.rms_error_ns / 1_000_000.0,
                rate,
                summary.disconnect_count,
                summary.recovery_count,
                summary.pipeline_playing,
                summary.transition_in_progress,
                if summary.clock_healthy { "ok" } else { "lost" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

struct SlaveServer {
    player: Arc<Player>,
    clock: gst::Clock,
    clock_port: u16,
    release_delay: Duration,
    operation_plan: Mutex<ReleasePlan>,
    transport: Arc<Mutex<TransportController>>,
    compatibility: ProgramCompatibility,
    registry: Arc<ParticipantRegistry>,
    state_socket: Arc<UdpSocket>,
    state_interval: Duration,
    keepalive_timeout: Duration,
    diagnostics: Arc<DiagnosticsHub>,
    operator_lock: Mutex<()>,
}

impl SlaveServer {
    fn authority(&self) -> Result<Authority> {
        let transport = self
            .transport
            .lock()
            .map_err(|_| anyhow!("transport lock poisoned"))?;
        Ok(Authority {
            session: transport.session(),
            generation: transport.generation(),
        })
    }
}

#[derive(Clone, Debug)]
struct ProgramCompatibility {
    program_id: String,
    program_hash: ProgramHash,
    master_player_id: String,
    cue_durations_ns: Vec<u64>,
    duration_tolerance_ns: u64,
    mismatch_policy: DurationMismatchPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Authority {
    session: MasterSession,
    generation: u64,
}

#[derive(Debug)]
struct GenerationChanged {
    received: u64,
    current: u64,
}

impl std::fmt::Display for GenerationChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "authoritative generation changed from {} to {}",
            self.current, self.received
        )
    }
}

impl std::error::Error for GenerationChanged {}

#[derive(Clone, Copy, Debug)]
struct KeepaliveConfig {
    interval: Duration,
    timeout: Duration,
}

fn welcome_slave(mut stream: TcpStream, peer: SocketAddr, server: &SlaveServer) -> Result<()> {
    configure_stream(&stream)?;
    let authority = server.authority()?;
    let (subscription, lease) = accept_subscription(&mut stream, peer, server, authority)?;
    match read_message(&mut stream).context("failed to read Slave readiness")? {
        Message::Ready => {
            lease.mark_clock_synchronized()?;
            info!(player = %subscription.player_id, %peer, "Slave clock is synchronized and ready to rejoin");
            log_registry(&server.registry, server.keepalive_timeout)?;
        }
        Message::Error(message) => bail!("Slave rejected preparation: {message}"),
        other => bail!("expected READY from Slave, received {other:?}"),
    }

    let plan = plan_slave_release(peer, server, authority)?;
    write_message(&mut stream, &Message::Release(plan))
        .context("failed to send synchronization state")?;
    info!(
        %peer,
        position_ns = plan.media_position.get(),
        release_at_ns = plan.release_at.get(),
        "Slave welcomed with current playback state"
    );

    stream.set_read_timeout(Some(REJOIN_PREPARATION_TIMEOUT))?;
    match read_message(&mut stream).context("failed to read Slave preparation result")? {
        Message::Prepared {
            session,
            generation,
        } if session == authority.session && generation == authority.generation => {
            lease.mark_ready()?;
            server
                .diagnostics
                .record_event(
                    &subscription.player_id,
                    server.clock.time().nseconds(),
                    "connected",
                    "ready",
                )
                .context("failed to record Slave connection event")?;
            info!(
                player = %subscription.player_id,
                %peer,
                master_session = %session,
                transport_generation = generation,
                "Slave prepared release; starting liveness monitoring"
            );
            log_registry(&server.registry, server.keepalive_timeout)?;
        }
        Message::Prepared {
            session,
            generation,
        } => bail!(
            "invalid preparation result: session {session}, generation {generation}; expected session {}, generation {}",
            authority.session,
            authority.generation
        ),
        Message::Error(message) => bail!("Slave failed to prepare release: {message}"),
        other => bail!("expected PREPARED from Slave, received {other:?}"),
    }

    let result = run_master_keepalives(&mut stream, peer, server, authority, &lease);
    let disconnect_kind = if server.authority()?.generation == authority.generation {
        "disconnected"
    } else {
        "transport-rejoin"
    };
    let detail = result
        .as_ref()
        .err()
        .map_or_else(|| "session-ended".to_owned(), |error| format!("{error:#}"));
    server
        .diagnostics
        .record_event(
            lease.player_id(),
            server.clock.time().nseconds(),
            disconnect_kind,
            &detail,
        )
        .context("failed to record Slave disconnect event")?;
    info!(player = %lease.player_id(), %peer, "Slave participant disconnected");
    result
}

fn accept_subscription(
    stream: &mut TcpStream,
    peer: SocketAddr,
    server: &SlaveServer,
    authority: Authority,
) -> Result<(Subscription, ParticipantLease)> {
    let subscription = match read_message(stream) {
        Ok(Message::Subscribe(subscription)) => subscription,
        Ok(other) => {
            let error = anyhow!("expected SUBSCRIBE from Slave, received {other:?}");
            reject_connection(stream, &error);
            return Err(error);
        }
        Err(error) => {
            let error = anyhow!(error).context("failed to read Slave subscription");
            reject_connection(stream, &error);
            return Err(error);
        }
    };
    let duration_differences = match validate_subscription(&subscription, &server.compatibility) {
        Ok(differences) => differences,
        Err(error) => {
            reject_connection(stream, &error);
            return Err(error);
        }
    };
    let lease = match server.registry.register(
        &subscription,
        peer,
        Instant::now(),
        server.keepalive_timeout,
    ) {
        Ok(lease) => lease,
        Err(error) => {
            let error = anyhow!(error);
            reject_connection(stream, &error);
            return Err(error);
        }
    };
    if subscription.software_version != env!("CARGO_PKG_VERSION") {
        warn!(
            player = %subscription.player_id,
            slave_software = %subscription.software_version,
            master_software = env!("CARGO_PKG_VERSION"),
            "Slave software version differs from Master; protocol remains compatible"
        );
    }
    for difference in duration_differences {
        warn!(
            player = %subscription.player_id,
            cue_index = difference.cue_index,
            master_duration_ns = difference.master_duration_ns,
            slave_duration_ns = difference.slave_duration_ns,
            delta_ns = difference.delta_ns,
            policy = ?server.compatibility.mismatch_policy,
            "Slave cue duration exceeds configured tolerance"
        );
    }
    info!(
        player = %subscription.player_id,
        %peer,
        program = %subscription.program_id,
        program_hash = %subscription.program_hash,
        software = %subscription.software_version,
        capabilities = format_args!("{:016x}", subscription.capabilities.bits()),
        state_port = subscription.state_port,
        "Slave subscription accepted"
    );
    log_registry(&server.registry, server.keepalive_timeout)?;
    write_message(
        stream,
        &Message::Subscribed {
            session: authority.session,
            generation: authority.generation,
            clock_port: server.clock_port,
            state_interval_ms: u32::try_from(server.state_interval.as_millis())
                .context("state interval does not fit the protocol")?,
            master_cue_durations_ns: server.compatibility.cue_durations_ns.clone(),
        },
    )?;
    Ok((subscription, lease))
}

fn plan_slave_release(
    peer: SocketAddr,
    server: &SlaveServer,
    authority: Authority,
) -> Result<ReleasePlan> {
    if server.authority()? != authority {
        bail!("transport changed while the Slave was preparing; retry the subscription");
    }
    let now = ClockNs::new(server.clock.time().nseconds());
    let minimum_lead_ns = u64::try_from(server.release_delay.as_nanos())
        .context("release delay is too large for the clock")?;
    let current_operation = *server
        .operation_plan
        .lock()
        .map_err(|_| anyhow!("operation plan lock poisoned"))?;
    let plan = if current_operation.generation == authority.generation
        && can_join_startup(current_operation.release_at, now, minimum_lead_ns)
    {
        info!(%peer, "Slave is ready for the shared operation deadline");
        current_operation
    } else {
        let release_at = now
            .checked_add(server.release_delay)
            .ok_or_else(|| anyhow!("release timestamp overflow"))?;
        let runtime = server
            .transport
            .lock()
            .map_err(|_| anyhow!("transport lock poisoned"))?
            .snapshot(now);
        if runtime.transport_state == TransportState::Paused {
            let snapshot = server.player.authoritative_snapshot(runtime)?;
            ReleasePlan {
                generation: authority.generation,
                operation: Operation::Goto,
                transport_state: TransportState::Paused,
                media_position: snapshot.media_position,
                release_at,
            }
        } else {
            server.player.join_plan(authority.generation, release_at)?
        }
    };
    Ok(plan)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DurationDifference {
    cue_index: usize,
    master_duration_ns: u64,
    slave_duration_ns: u64,
    delta_ns: u64,
}

fn validate_subscription(
    subscription: &Subscription,
    compatibility: &ProgramCompatibility,
) -> Result<Vec<DurationDifference>> {
    if subscription.player_id == compatibility.master_player_id {
        bail!(
            "player ID '{}' is already used by the Master",
            subscription.player_id
        );
    }
    if subscription.program_id != compatibility.program_id {
        bail!(
            "program ID mismatch: Master '{}', Slave '{}'",
            compatibility.program_id,
            subscription.program_id
        );
    }
    if subscription.program_hash != compatibility.program_hash {
        bail!(
            "program hash mismatch: Master {}, Slave {}",
            compatibility.program_hash,
            subscription.program_hash
        );
    }
    if !subscription.capabilities.contains(Capabilities::REQUIRED) {
        bail!(
            "Slave capabilities {:016x} do not include required capabilities {:016x}",
            subscription.capabilities.bits(),
            Capabilities::REQUIRED
        );
    }
    if subscription.state_port == 0 {
        bail!("Slave must provide a non-zero UDP state receive port");
    }
    if subscription.cue_durations_ns.len() != compatibility.cue_durations_ns.len() {
        bail!(
            "cue-duration count mismatch: Master {}, Slave {}",
            compatibility.cue_durations_ns.len(),
            subscription.cue_durations_ns.len()
        );
    }
    let differences = compatibility
        .cue_durations_ns
        .iter()
        .zip(&subscription.cue_durations_ns)
        .enumerate()
        .filter_map(|(cue_index, (master, slave))| {
            let delta_ns = master.abs_diff(*slave);
            (delta_ns > compatibility.duration_tolerance_ns).then_some(DurationDifference {
                cue_index,
                master_duration_ns: *master,
                slave_duration_ns: *slave,
                delta_ns,
            })
        })
        .collect::<Vec<_>>();
    if compatibility.mismatch_policy == DurationMismatchPolicy::Reject
        && let Some(difference) = differences.first()
    {
        bail!(
            "cue {} duration mismatch: Master {} ns, Slave {} ns, delta {} ns exceeds tolerance {} ns",
            difference.cue_index,
            difference.master_duration_ns,
            difference.slave_duration_ns,
            difference.delta_ns,
            compatibility.duration_tolerance_ns
        );
    }
    Ok(differences)
}

fn reject_connection(stream: &mut TcpStream, error: &anyhow::Error) {
    let _ = write_message(stream, &Message::Error(format!("{error:#}")));
}

fn log_registry(registry: &ParticipantRegistry, stale_after: Duration) -> Result<()> {
    for participant in registry.snapshot(Instant::now(), stale_after)? {
        let state_endpoint = match participant.state_endpoint {
            StateEndpoint::ControlConnection => "control".to_owned(),
            StateEndpoint::Udp(endpoint) => endpoint.to_string(),
        };
        debug!(
            player = %participant.player_id,
            remote = %participant.remote_address,
            readiness = ?participant.readiness,
            clock_synchronized = participant.clock_synchronized,
            last_seen_ms = participant.last_seen_age.as_millis(),
            state_endpoint,
            software = %participant.software_version,
            capabilities = format_args!("{:016x}", participant.capabilities.bits()),
            cue_count = participant.cue_durations_ns.len(),
            "participant registry status"
        );
    }
    Ok(())
}

fn broadcast_state(server: Arc<SlaveServer>) {
    thread::spawn(move || {
        let mut generation = None;
        let mut sequence = 0_u64;
        loop {
            let started = Instant::now();
            let result = (|| -> Result<()> {
                let master_clock = ClockNs::new(server.clock.time().nseconds());
                let runtime = server
                    .transport
                    .lock()
                    .map_err(|_| anyhow!("transport lock poisoned"))?
                    .snapshot(master_clock);
                if generation != Some(runtime.generation) {
                    generation = Some(runtime.generation);
                    sequence = 0;
                }
                sequence = sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("state sequence overflow"))?;
                let packet = StateDatagram {
                    session: server.authority()?.session,
                    sequence,
                    master_clock,
                    state: server.player.authoritative_snapshot(runtime)?,
                };
                let bytes =
                    encode_state_datagram(&packet).context("failed to encode state snapshot")?;
                for participant in server
                    .registry
                    .snapshot(Instant::now(), server.keepalive_timeout)?
                {
                    if participant.readiness != Readiness::Ready {
                        continue;
                    }
                    let StateEndpoint::Udp(endpoint) = participant.state_endpoint else {
                        continue;
                    };
                    if let Err(error) = server.state_socket.send_to(&bytes, endpoint) {
                        warn!(player = %participant.player_id, %endpoint, %error, "failed to send state snapshot");
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                error!(%error, "state broadcaster iteration failed");
            }
            thread::sleep(server.state_interval.saturating_sub(started.elapsed()));
        }
    });
}

fn run_master_keepalives(
    stream: &mut TcpStream,
    peer: SocketAddr,
    server: &SlaveServer,
    authority: Authority,
    lease: &ParticipantLease,
) -> Result<()> {
    stream.set_write_timeout(Some(server.keepalive_timeout))?;
    let mut last_sequence = None;
    let mut last_keepalive = Instant::now();
    loop {
        let remaining = server
            .keepalive_timeout
            .saturating_sub(last_keepalive.elapsed());
        if remaining.is_zero() {
            bail!("Slave keepalive deadline expired");
        }
        stream.set_read_timeout(Some(remaining))?;
        match read_message(stream).context("failed to read Slave control message")? {
            Message::Keepalive { session, sequence }
                if session == authority.session
                    && last_sequence.is_none_or(|old| sequence > old) =>
            {
                lease.mark_seen()?;
                last_keepalive = Instant::now();
                write_message(stream, &Message::KeepaliveAck { session, sequence })
                    .context("failed to acknowledge Slave keepalive")?;
                last_sequence = Some(sequence);
                debug!(%peer, master_session = %session, keepalive_sequence = sequence,
                    "Slave keepalive acknowledged");
            }
            Message::Keepalive { session, sequence } => {
                bail!("invalid keepalive: session {session}, sequence {sequence}")
            }
            Message::Telemetry { session, sample } if session == authority.session => {
                server
                    .diagnostics
                    .record(lease.player_id(), sample)
                    .context("failed to record Slave diagnostics")?;
            }
            Message::Telemetry { session, .. } => {
                bail!("invalid telemetry session {session}")
            }
            Message::Error(message) => bail!("Slave rejected control session: {message}"),
            other => bail!("expected KEEPALIVE from Slave, received {other:?}"),
        }
    }
}

fn monitor_master_session(
    stream: &mut TcpStream,
    state_socket: &UdpSocket,
    master: SocketAddr,
    authority: Authority,
    player: &Player,
    keepalive_interval: Duration,
    keepalive_timeout: Duration,
) -> Result<()> {
    stream.set_read_timeout(Some(keepalive_timeout))?;
    stream.set_write_timeout(Some(keepalive_timeout))?;
    let mut keepalive_sequence = 0_u64;
    let mut next_keepalive = Instant::now();
    let mut next_diagnostic = Instant::now();
    let mut last_state_sequence = None;
    let mut bytes = [0_u8; MAX_STATE_PACKET_BYTES];
    loop {
        if Instant::now() >= next_keepalive {
            keepalive_sequence = keepalive_sequence
                .checked_add(1)
                .ok_or_else(|| anyhow!("keepalive sequence overflow"))?;
            let sent_at = Instant::now();
            write_message(
                stream,
                &Message::Keepalive {
                    session: authority.session,
                    sequence: keepalive_sequence,
                },
            )
            .context("failed to send control keepalive")?;
            match read_message(stream).context("keepalive acknowledgement unavailable")? {
                Message::KeepaliveAck { session, sequence }
                    if session == authority.session && sequence == keepalive_sequence => {}
                Message::Error(message) => bail!("Master rejected keepalive: {message}"),
                other => bail!("invalid keepalive acknowledgement: {other:?}"),
            }
            debug!(%master, keepalive_sequence, round_trip_ms = sent_at.elapsed().as_millis(),
                "Master control keepalive acknowledged");
            next_keepalive = Instant::now() + keepalive_interval;
        }

        if Instant::now() >= next_diagnostic {
            if let Some(sample) = player.diagnostic_telemetry()? {
                write_message(
                    stream,
                    &Message::Telemetry {
                        session: authority.session,
                        sample,
                    },
                )
                .context("failed to send diagnostic telemetry")?;
            }
            next_diagnostic = Instant::now() + TELEMETRY_INTERVAL;
        }

        let wait = next_keepalive
            .saturating_duration_since(Instant::now())
            .min(next_diagnostic.saturating_duration_since(Instant::now()))
            .min(Duration::from_millis(250))
            .max(Duration::from_millis(1));
        state_socket.set_read_timeout(Some(wait))?;
        match state_socket.recv_from(&mut bytes) {
            Ok((length, source)) if source.ip() == master.ip() => {
                match decode_state_datagram(&bytes[..length]) {
                    Ok(packet) => {
                        if validate_state_datagram(&packet, authority, last_state_sequence)? {
                            player.accept_authoritative_snapshot(&packet.state, authority)?;
                            last_state_sequence = Some(packet.sequence);
                            debug!(%master, state_sequence = packet.sequence,
                            master_clock_ns = packet.master_clock.get(),
                            snapshot_position_ns = packet.state.media_position.get(),
                            "authoritative state snapshot accepted");
                        }
                    }
                    Err(error) => warn!(%source, %error, "discarded malformed state datagram"),
                }
            }
            Ok((_length, source)) => debug!(%source, "ignored state datagram from unexpected host"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error).context("failed to receive state datagram"),
        }
    }
}

fn validate_state_datagram(
    packet: &StateDatagram,
    authority: Authority,
    last_sequence: Option<u64>,
) -> Result<bool> {
    if packet.session != authority.session {
        return Ok(false);
    }
    if packet.state.generation < authority.generation {
        return Ok(false);
    }
    if packet.state.generation > authority.generation {
        return Err(GenerationChanged {
            received: packet.state.generation,
            current: authority.generation,
        }
        .into());
    }
    if last_sequence.is_some_and(|last| packet.sequence <= last) {
        return Ok(false);
    }
    if packet.state.transport_state == TransportState::Playing
        && packet.state.anchor_clock > packet.master_clock
    {
        bail!("playing snapshot anchor is later than its packet clock");
    }
    Ok(true)
}

fn connect_to_master(
    master: SocketAddr,
    subscription: &Subscription,
) -> Result<(TcpStream, gst::Clock, Authority)> {
    let mut stream = TcpStream::connect_timeout(&master, IO_TIMEOUT)
        .with_context(|| format!("failed to connect to Master {master}"))?;
    configure_stream(&stream)?;
    write_message(&mut stream, &Message::Subscribe(subscription.clone()))
        .context("failed to send subscription")?;
    let (clock_port, authority, master_cue_durations_ns) =
        match read_message(&mut stream).context("failed to read subscription result")? {
            Message::Subscribed {
                session,
                generation,
                clock_port,
                state_interval_ms,
                master_cue_durations_ns,
            } => (
                clock_port,
                Authority {
                    session,
                    generation,
                },
                {
                    debug!(state_interval_ms, "Master state snapshot cadence accepted");
                    master_cue_durations_ns
                },
            ),
            Message::Error(message) => bail!("Master rejected subscription: {message}"),
            other => bail!("expected SUBSCRIBED from Master, received {other:?}"),
        };
    if master_cue_durations_ns.len() != subscription.cue_durations_ns.len() {
        bail!(
            "Master returned {} cue durations for a {}-cue subscription",
            master_cue_durations_ns.len(),
            subscription.cue_durations_ns.len()
        );
    }
    for (cue_index, (master_duration, local_duration)) in master_cue_durations_ns
        .iter()
        .zip(&subscription.cue_durations_ns)
        .enumerate()
    {
        debug!(
            cue_index,
            master_duration_ns = master_duration,
            local_duration_ns = local_duration,
            delta_ns = master_duration.abs_diff(*local_duration),
            "Master/Slave cue duration comparison"
        );
    }
    let master_ip = master.ip().to_string();
    let clock = gst_net::NetClientClock::new(
        Some("syncplayer-lab-clock"),
        &master_ip,
        i32::from(clock_port),
        Some(gst::ClockTime::ZERO),
    );
    info!(
        master = %master_ip,
        clock_port,
        master_session = %authority.session,
        transport_generation = authority.generation,
        "calibrating new network clock"
    );
    let calibration_started = Instant::now();
    clock
        .wait_for_sync(Some(to_clock_time(CLOCK_SYNC_TIMEOUT)?))
        .context("network clock did not synchronize before timeout")?;
    info!(
        clock_time_ns = clock.time().nseconds(),
        calibration_ms = calibration_started.elapsed().as_millis(),
        "network clock synchronized"
    );
    Ok((stream, clock.upcast(), authority))
}

fn receive_release(
    stream: &mut TcpStream,
    clock: &gst::Clock,
    authority: Authority,
) -> Result<ReleasePlan> {
    write_message(stream, &Message::Ready).context("failed to report readiness")?;
    let plan = match read_message(stream).context("failed to read release plan")? {
        Message::Release(plan) => plan,
        Message::Error(message) => bail!("Master cancelled release: {message}"),
        other => bail!("expected RELEASE from Master, received {other:?}"),
    };
    let plan = plan
        .validate(ClockNs::new(clock.time().nseconds()), None)
        .context("Master supplied an unusable release plan")?;
    if plan.generation != authority.generation {
        bail!(
            "release generation {} does not match handshake generation {}",
            plan.generation,
            authority.generation
        );
    }
    Ok(plan)
}

fn report_prepared(stream: &mut TcpStream, authority: Authority) -> Result<()> {
    write_message(
        stream,
        &Message::Prepared {
            session: authority.session,
            generation: authority.generation,
        },
    )
    .context("failed to report prepared release")
}

fn monitor_master(
    mut stream: TcpStream,
    state_socket: UdpSocket,
    master: SocketAddr,
    mut authority: Authority,
    player: Arc<Player>,
    subscription: Subscription,
    keepalive: KeepaliveConfig,
) {
    thread::spawn(move || {
        loop {
            let error = monitor_master_session(
                &mut stream,
                &state_socket,
                master,
                authority,
                &player,
                keepalive.interval,
                keepalive.timeout,
            )
            .err()
            .unwrap_or_else(|| anyhow!("Master protocol monitor stopped"));
            let planned_rejoin = error.downcast_ref::<GenerationChanged>().is_some();
            if !planned_rejoin {
                player.set_clock_health(false);
                let _ = write_message(
                    &mut stream,
                    &Message::Error(format!("Slave control monitor failed: {error:#}")),
                );
            }
            if let Err(shutdown_error) = stream.shutdown(Shutdown::Both) {
                debug!(%master, %shutdown_error, "old Master connection was already closed");
            }
            if planned_rejoin {
                info!(
                    %master,
                    master_session = %authority.session,
                    transport_generation = authority.generation,
                    %error,
                    "authoritative operation requires a prepared rejoin"
                );
            } else {
                warn!(
                    %master,
                    master_session = %authority.session,
                    transport_generation = authority.generation,
                    recovery_reason = "master-liveness",
                    %error,
                    "Master liveness lost; entering free-running mode"
                );
                player.disconnect_count.fetch_add(1, Ordering::AcqRel);
            }

            loop {
                match connect_to_master(master, &subscription).and_then(
                    |(mut new_stream, clock, new_authority)| {
                        let plan = receive_release(&mut new_stream, &clock, new_authority)?;
                        player.apply_control_plan(&clock, plan)?;
                        report_prepared(&mut new_stream, new_authority)?;
                        player.set_clock_health(true);
                        Ok((new_stream, plan, new_authority))
                    },
                ) {
                    Ok((new_stream, plan, new_authority)) => {
                        stream = new_stream;
                        authority = new_authority;
                        info!(
                            %master,
                            master_session = %authority.session,
                            transport_generation = authority.generation,
                            release_at_ns = plan.release_at.get(),
                            "Master reconnected; leaving free-running mode after prepared rejoin"
                        );
                        break;
                    }
                    Err(error) => {
                        warn!(%master, %error, "Master reconnect attempt failed; still free-running");
                        thread::sleep(MASTER_RETRY_INTERVAL);
                    }
                }
            }
        }
    });
}

#[derive(Clone, Debug)]
struct MediaItem {
    path: PathBuf,
    uri: String,
    audio_uri: String,
    external_audio: bool,
    has_audio: bool,
    mute: bool,
    gain_db: f64,
}

#[derive(Debug)]
struct PlaybackSequence {
    program_id: String,
    program_hash: ProgramHash,
    player_id: String,
    cue_ids: Vec<String>,
    event_cues: Vec<EventCue>,
    items: Vec<MediaItem>,
    cue_durations_ns: Vec<u64>,
    timeline: Option<Timeline>,
    looping: bool,
    from_playlist: bool,
}

impl PlaybackSequence {
    #[allow(clippy::too_many_lines)]
    fn prepare(program: &ResolvedProgram) -> Result<Self> {
        let discoverer = gst_pbutils::Discoverer::new(to_clock_time(PREROLL_TIMEOUT)?)
            .context("failed to create GStreamer media discoverer")?;
        // Startup discovery must reflect the files as they exist now. Do not
        // let GstDiscoverer reuse persistent results from an earlier run.
        discoverer.set_property("use-cache", false);

        let mut items = Vec::with_capacity(program.cues.len());
        let mut durations = Vec::with_capacity(program.cues.len());
        for (index, cue) in program.cues.iter().enumerate() {
            let path = cue.video.clone();
            let uri =
                media_uri(&path).with_context(|| format!("cue '{}' is unavailable", cue.id))?;
            let info = discoverer.discover_uri(&uri).with_context(|| {
                format!("failed to discover cue '{}' at {}", cue.id, path.display())
            })?;
            if info.result() != gst_pbutils::DiscovererResult::Ok {
                bail!(
                    "cue '{}' could not be discovered (result {:?}): {}",
                    cue.id,
                    info.result(),
                    path.display()
                );
            }
            let duration_ns = info
                .duration()
                .map(gst::ClockTime::nseconds)
                .filter(|duration| *duration > 0)
                .ok_or_else(|| {
                    anyhow!(
                        "cue '{}' has no known non-zero duration: {}",
                        cue.id,
                        path.display()
                    )
                })?;
            if info.video_streams().is_empty() {
                bail!(
                    "cue '{}' contains no video stream: {}",
                    cue.id,
                    path.display()
                );
            }

            let (audio_uri, audio_duration_ns, has_audio) = if let Some(audio_path) = &cue.audio {
                let audio_uri = media_uri(audio_path)
                    .with_context(|| format!("cue '{}' external audio is unavailable", cue.id))?;
                let audio_info = discoverer.discover_uri(&audio_uri).with_context(|| {
                    format!(
                        "failed to discover cue '{}' external audio at {}",
                        cue.id,
                        audio_path.display()
                    )
                })?;
                if audio_info.result() != gst_pbutils::DiscovererResult::Ok {
                    bail!(
                        "cue '{}' external audio could not be discovered (result {:?}): {}",
                        cue.id,
                        audio_info.result(),
                        audio_path.display()
                    );
                }
                if audio_info.audio_streams().is_empty() {
                    bail!(
                        "cue '{}' external audio contains no audio stream: {}",
                        cue.id,
                        audio_path.display()
                    );
                }
                let audio_duration_ns = audio_info
                    .duration()
                    .map(gst::ClockTime::nseconds)
                    .filter(|duration| *duration > 0)
                    .ok_or_else(|| {
                        anyhow!(
                            "cue '{}' external audio has no known non-zero duration: {}",
                            cue.id,
                            audio_path.display()
                        )
                    })?;
                let delta = duration_ns.abs_diff(audio_duration_ns);
                if delta > EXTERNAL_AUDIO_DURATION_TOLERANCE_NS {
                    bail!(
                        "cue '{}' external audio duration differs from video by {} ms (maximum {} ms)",
                        cue.id,
                        format_ns_as_milliseconds(delta),
                        format_ns_as_milliseconds(EXTERNAL_AUDIO_DURATION_TOLERANCE_NS)
                    );
                }
                (audio_uri, Some(audio_duration_ns), true)
            } else {
                (uri.clone(), None, !info.audio_streams().is_empty())
            };
            gain_db_to_linear(cue.gain_db)
                .with_context(|| format!("cue '{}' has an unsupported gain_db value", cue.id))?;
            info!(
                item_index = index,
                cue = %cue.id,
                media = %path.display(),
                external_audio = cue.audio.as_ref().map(|value| value.display().to_string()),
                duration_ns,
                audio_duration_ns,
                has_audio,
                mute = cue.mute,
                gain_db = cue.gain_db,
                "program cue discovered"
            );
            items.push(MediaItem {
                path,
                uri,
                audio_uri,
                external_audio: cue.audio.is_some(),
                has_audio,
                mute: cue.mute,
                gain_db: cue.gain_db,
            });
            durations.push(duration_ns);
        }
        let timeline = Timeline::new(durations.clone(), program.looping)
            .context("invalid program sequence duration")?;
        let from_playlist = items.len() > 1;
        let event_cues = program
            .cues
            .iter()
            .zip(&durations)
            .map(|(cue, duration_ns)| EventCue {
                id: cue.id.clone(),
                duration_ns: *duration_ns,
                events: cue.events.clone(),
            })
            .collect();
        Ok(Self {
            program_id: program.program_id.clone(),
            program_hash: program.hash,
            player_id: program.player_id.to_string(),
            cue_ids: program.cues.iter().map(|cue| cue.id.clone()).collect(),
            event_cues,
            items,
            cue_durations_ns: durations,
            timeline: Some(timeline),
            looping: program.looping,
            from_playlist,
        })
    }

    fn locate(&self, position_ns: u64) -> Result<ItemPosition> {
        match &self.timeline {
            Some(timeline) => timeline
                .locate(position_ns)
                .context("invalid sequence position")?
                .ok_or_else(|| {
                    anyhow!("sequence position is at the end of a non-looping playlist")
                }),
            None => Ok(ItemPosition {
                index: 0,
                local_ns: position_ns,
            }),
        }
    }

    fn project(&self, position_ns: u64, lead_ns: u64) -> Result<u64> {
        match &self.timeline {
            Some(timeline) => timeline
                .project_absolute(position_ns, lead_ns)
                .context("cannot schedule a join beyond the end of the media sequence"),
            None => Ok(position_ns.saturating_add(lead_ns)),
        }
    }

    #[cfg(test)]
    fn cue_identity(&self, absolute_position_ns: u64) -> Result<(ItemPosition, u64)> {
        let item = self.locate(absolute_position_ns)?;
        let iteration = self.timeline.as_ref().map_or(0, |timeline| {
            timeline.playlist_iteration(absolute_position_ns)
        });
        Ok((item, iteration))
    }

    fn position_for_cue(
        &self,
        item: ItemPosition,
        playlist_iteration: u64,
    ) -> Result<MediaPositionNs> {
        match &self.timeline {
            Some(timeline) => timeline
                .absolute_position(item, playlist_iteration)
                .map(MediaPositionNs::new)
                .context("invalid authoritative cue position"),
            None if item.index == 0 && playlist_iteration == 0 => {
                Ok(MediaPositionNs::new(item.local_ns))
            }
            None => bail!(
                "single-media playback cannot map item {} in iteration {playlist_iteration}",
                item.index
            ),
        }
    }

    fn position_for_snapshot(
        &self,
        item: ItemPosition,
        playlist_iteration: u64,
    ) -> Result<MediaPositionNs> {
        if !self.looping
            && item.index + 1 == self.items.len()
            && item.local_ns == self.cue_durations_ns[item.index]
        {
            return self
                .timeline
                .as_ref()
                .map(|timeline| MediaPositionNs::new(timeline.total_ns()))
                .ok_or_else(|| anyhow!("completed sequence has no timeline"));
        }
        self.position_for_cue(item, playlist_iteration)
    }

    fn sequence_position(&self, position: ItemPosition) -> Result<MediaPositionNs> {
        match &self.timeline {
            Some(timeline) => timeline
                .sequence_position(position)
                .map(MediaPositionNs::new)
                .context("invalid item-local position"),
            None if position.index == 0 => Ok(MediaPositionNs::new(position.local_ns)),
            None => bail!("single-media playback reported item {}", position.index),
        }
    }

    fn sequence_position_near(
        &self,
        position: ItemPosition,
        expected: MediaPositionNs,
    ) -> Result<MediaPositionNs> {
        let position_in_iteration = self.sequence_position(position)?;
        let Some(timeline) = &self.timeline else {
            return Ok(position_in_iteration);
        };
        if !timeline.is_looping() {
            return Ok(position_in_iteration);
        }
        let expected_iteration = timeline.playlist_iteration(expected.get());
        let first_iteration = expected_iteration.saturating_sub(1);
        let last_iteration = expected_iteration.saturating_add(1);
        (first_iteration..=last_iteration)
            .filter_map(|iteration| {
                timeline
                    .absolute_position(position, iteration)
                    .ok()
                    .map(MediaPositionNs::new)
            })
            .min_by_key(|candidate| (*candidate - expected).unsigned_abs())
            .ok_or_else(|| anyhow!("could not place item near the expected playlist occurrence"))
    }

    fn position_for_remote_snapshot(
        &self,
        item: ItemPosition,
        playlist_iteration: u64,
    ) -> Result<MediaPositionNs> {
        let duration_ns = self
            .cue_durations_ns
            .get(item.index)
            .copied()
            .ok_or_else(|| anyhow!("snapshot cue index {} does not exist", item.index))?;
        let local_item = ItemPosition {
            index: item.index,
            // A permitted duration mismatch can leave the Master briefly inside
            // a cue after this rendition has reached its endpoint. Preserve the
            // authoritative cue identity and hold the last representable instant.
            local_ns: item.local_ns.min(duration_ns.saturating_sub(1)),
        };
        self.position_for_snapshot(local_item, playlist_iteration)
    }

    fn position_error(&self, actual: MediaPositionNs, expected: MediaPositionNs) -> i128 {
        match &self.timeline {
            Some(timeline) if timeline.is_looping() => {
                actual.circular_error(expected, timeline.total_ns())
            }
            _ => actual - expected,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlayerRole {
    Master,
    Slave,
}

impl fmt::Display for PlayerRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Master => "MASTER",
            Self::Slave => "SLAVE",
        })
    }
}

#[derive(Clone, Debug)]
struct OsdMetadata {
    cue_index: usize,
    cue_id: String,
    transport_state: TransportState,
}

impl PlayerRole {
    const fn from_external_clock(externally_driven: bool) -> Self {
        if externally_driven {
            Self::Slave
        } else {
            Self::Master
        }
    }
}

#[derive(Clone, Debug)]
struct PositionSample {
    recovery_attempt: u64,
    clock_time: ClockNs,
    expected_sequence_position: MediaPositionNs,
    expected_item: ItemPosition,
    expected_playlist_iteration: u64,
    actual_sequence_position: MediaPositionNs,
    actual_item: ItemPosition,
    actual_uri: String,
    error_ns: i128,
    pipeline_state: gst::State,
    transition_in_progress: bool,
}

#[derive(Clone, Copy, Debug)]
struct EventProjection {
    session: MasterSession,
    generation: u64,
    anchor_clock: ClockNs,
    anchor_media_position: MediaPositionNs,
    anchor_item: ItemPosition,
    cue_occurrence: u64,
    playlist_iteration: u64,
    transport_state: TransportState,
    debug_loop: Option<DebugLoopState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriftAction {
    None,
    Warn { consecutive: u32 },
    Repair { consecutive: u32 },
    Cooldown { remaining: Duration },
}

#[derive(Debug, Default)]
struct DriftPolicy {
    warning_samples: u32,
    repair_samples: u32,
    warning_emitted: bool,
    cooldown_until: Option<Instant>,
}

impl DriftPolicy {
    fn observe(&mut self, now: Instant, error_ns: i128) -> DriftAction {
        if let Some(cooldown_until) = self.cooldown_until {
            if let Some(remaining) = cooldown_until.checked_duration_since(now) {
                self.clear_counts();
                return DriftAction::Cooldown { remaining };
            }
            self.cooldown_until = None;
        }

        let magnitude = error_ns.unsigned_abs();
        if magnitude <= DRIFT_WARN_THRESHOLD_NS {
            self.clear_counts();
            return DriftAction::None;
        }

        self.warning_samples = self.warning_samples.saturating_add(1);
        if magnitude > DRIFT_REPAIR_THRESHOLD_NS {
            self.repair_samples = self.repair_samples.saturating_add(1);
        } else {
            self.repair_samples = 0;
        }

        if self.repair_samples >= DRIFT_REQUIRED_SAMPLES {
            let consecutive = self.repair_samples;
            self.clear_counts();
            self.cooldown_until = Some(now + DRIFT_RECOVERY_COOLDOWN);
            return DriftAction::Repair { consecutive };
        }
        if self.warning_samples >= DRIFT_REQUIRED_SAMPLES && !self.warning_emitted {
            self.warning_emitted = true;
            return DriftAction::Warn {
                consecutive: self.warning_samples,
            };
        }
        DriftAction::None
    }

    fn suppress(&mut self) {
        self.clear_counts();
    }

    fn clear_counts(&mut self) {
        self.warning_samples = 0;
        self.repair_samples = 0;
        self.warning_emitted = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DriftSlewAction {
    None,
    Apply(f64),
    Reset,
}

#[derive(Debug)]
struct DriftSlewController {
    enabled: bool,
    active: bool,
    reset_pending: bool,
    direction: i8,
    qualifying_samples: u32,
    applied_rate: f64,
}

impl DriftSlewController {
    const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            active: false,
            reset_pending: false,
            direction: 0,
            qualifying_samples: 0,
            applied_rate: 1.0,
        }
    }

    fn observe(&mut self, error_ns: i128) -> DriftSlewAction {
        if !self.enabled {
            return DriftSlewAction::None;
        }
        if self.reset_pending {
            self.reset_pending = false;
            return DriftSlewAction::Reset;
        }

        let magnitude = error_ns.unsigned_abs();
        let direction = error_ns.signum() as i8;
        if self.active {
            if magnitude <= DRIFT_SLEW_SETTLE_THRESHOLD_NS
                || magnitude > DRIFT_REPAIR_THRESHOLD_NS
                || direction != self.direction
            {
                self.clear();
                return DriftSlewAction::Reset;
            }
            let rate = drift_slew_rate(error_ns);
            if (rate - self.applied_rate).abs() < DRIFT_SLEW_RATE_UPDATE_THRESHOLD {
                return DriftSlewAction::None;
            }
            self.applied_rate = rate;
            return DriftSlewAction::Apply(rate);
        }

        if magnitude <= DRIFT_WARN_THRESHOLD_NS || magnitude > DRIFT_REPAIR_THRESHOLD_NS {
            self.direction = 0;
            self.qualifying_samples = 0;
            return DriftSlewAction::None;
        }
        if self.direction == direction {
            self.qualifying_samples = self.qualifying_samples.saturating_add(1);
        } else {
            self.direction = direction;
            self.qualifying_samples = 1;
        }
        if self.qualifying_samples < DRIFT_REQUIRED_SAMPLES {
            return DriftSlewAction::None;
        }

        self.active = true;
        let rate = drift_slew_rate(error_ns);
        self.applied_rate = rate;
        DriftSlewAction::Apply(rate)
    }

    fn suppress(&mut self) -> bool {
        let was_active = self.active;
        self.reset_pending |= was_active;
        self.clear_tracking();
        was_active
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.clear();
    }

    fn clear(&mut self) {
        self.reset_pending = false;
        self.clear_tracking();
    }

    fn clear_tracking(&mut self) {
        self.active = false;
        self.direction = 0;
        self.qualifying_samples = 0;
        self.applied_rate = 1.0;
    }
}

fn drift_slew_rate(error_ns: i128) -> f64 {
    let magnitude_ns = u64::try_from(error_ns.unsigned_abs()).unwrap_or(u64::MAX);
    let magnitude = Duration::from_nanos(magnitude_ns).as_secs_f64();
    let signed_error = if error_ns.is_negative() {
        -magnitude
    } else {
        magnitude
    };
    let adjustment = (signed_error / DRIFT_SLEW_CORRECTION_WINDOW_SECS)
        .clamp(-DRIFT_SLEW_MAX_ADJUSTMENT, DRIFT_SLEW_MAX_ADJUSTMENT);
    1.0 - adjustment
}

struct Player {
    pipeline: gst::Pipeline,
    video_source: gst::Element,
    audio_source: gst::Element,
    audio_volume: gst::Element,
    global_mute: bool,
    global_gain_db: f64,
    sequence: PlaybackSequence,
    events: Option<Mutex<EventRuntime>>,
    event_projection: Mutex<Option<EventProjection>>,
    events_suppressed_until: Mutex<Option<ClockNs>>,
    cursor: Arc<Mutex<PlaylistCursor>>,
    audio_cursor: Arc<Mutex<PlaylistCursor>>,
    role: PlayerRole,
    osd_overlay: gst::Element,
    osd_enabled: AtomicBool,
    osd_metadata: Mutex<Option<OsdMetadata>>,
    clock_healthy: AtomicBool,
    clock: Mutex<gst::Clock>,
    plan: Mutex<Option<ReleasePlan>>,
    transport_state: Mutex<TransportState>,
    recovery_lock: Mutex<()>,
    recovery_in_progress: AtomicBool,
    recovery_attempt: Arc<AtomicU64>,
    disconnect_count: AtomicU64,
    drift_policy: Mutex<DriftPolicy>,
    drift_slew: Mutex<DriftSlewController>,
    rate_control_targets: Vec<gst::Element>,
    handoff_in_progress: Arc<AtomicBool>,
    separate_audio_handoff: bool,
    boundary_mismatch: Mutex<Option<(usize, usize, u64)>>,
    scheduled_release: Arc<Mutex<Option<ScheduledRelease>>>,
    release_error: Arc<Mutex<Option<String>>>,
}

struct ScheduledRelease {
    release_id: gst::SingleShotClockId,
}

impl ScheduledRelease {
    fn unschedule(self) {
        self.release_id.unschedule();
    }
}

struct ReleaseWorker {
    pipeline: gst::Pipeline,
    clock: gst::Clock,
    plan: ReleasePlan,
    release_id: gst::SingleShotClockId,
    recovery_attempt: u64,
    active_recovery_attempt: Arc<AtomicU64>,
    scheduled_release: Arc<Mutex<Option<ScheduledRelease>>>,
    release_error: Arc<Mutex<Option<String>>>,
    target_state: gst::State,
}

impl ReleaseWorker {
    fn run(self) {
        if let Err(error) = self.run_inner() {
            self.release_id.unschedule();
            record_release_error(&self.release_error, error.to_string());
        }
    }

    fn run_inner(&self) -> Result<()> {
        if !wait_for_clock_id(&self.release_id, "shared-clock release")? {
            return Ok(());
        }

        if !recovery_attempt_is_active(&self.active_recovery_attempt, self.recovery_attempt) {
            debug!(
                recovery_attempt = self.recovery_attempt,
                "obsolete scheduled release ignored"
            );
            return Ok(());
        }

        // Hold this lock through set_state so cancellation cannot pause and
        // then lose a race to an obsolete release worker.
        let mut scheduled = self
            .scheduled_release
            .lock()
            .map_err(|_| anyhow!("scheduled release lock poisoned"))?;
        if !release_is_active(scheduled.as_ref(), &self.release_id) {
            return Ok(());
        }
        if !recovery_attempt_is_active(&self.active_recovery_attempt, self.recovery_attempt) {
            scheduled.take();
            return Ok(());
        }
        scheduled.take();

        let fired_at_ns = self.clock.time().nseconds();
        debug!(
            release_at_ns = self.plan.release_at.get(),
            fired_at_ns,
            lateness_ns = i128::from(fired_at_ns) - i128::from(self.plan.release_at.get()),
            "shared-clock release fired"
        );
        self.pipeline
            .set_state(self.target_state)
            .with_context(|| format!("failed to request {:?} at release", self.target_state))?;
        Ok(())
    }
}

fn release_is_active(
    scheduled: Option<&ScheduledRelease>,
    release_id: &gst::SingleShotClockId,
) -> bool {
    scheduled.is_some_and(|scheduled| scheduled.release_id == *release_id)
}

fn recovery_attempt_is_active(active: &AtomicU64, expected: u64) -> bool {
    active.load(Ordering::Acquire) == expected
}

fn wait_for_clock_id(id: &gst::SingleShotClockId, description: &str) -> Result<bool> {
    let (result, _) = id.wait();
    match result {
        Ok(_) | Err(gst::ClockError::Early) => Ok(true),
        Err(gst::ClockError::Unscheduled) => Ok(false),
        Err(error) => {
            Err(anyhow!(error)).with_context(|| format!("failed to wait for {description}"))
        }
    }
}

fn make_event_runtime(sequence: &PlaybackSequence) -> Result<Option<Mutex<EventRuntime>>> {
    if sequence.event_cues.iter().all(|cue| cue.events.is_empty()) {
        return Ok(None);
    }
    Ok(Some(Mutex::new(EventRuntime::new(
        sequence.player_id.clone(),
        sequence.event_cues.clone(),
    )?)))
}

impl Player {
    #[allow(clippy::too_many_lines)]
    fn new(
        args: &PlaybackArgs,
        sequence: PlaybackSequence,
        forced_clock: Option<&gst::Clock>,
        drift_slew: bool,
    ) -> Result<Self> {
        configure_decoder_policy(args.hwdec);
        for item in &sequence.items {
            let effective_gain_db = args.gain_db + item.gain_db;
            if effective_gain_db > 0.0 {
                warn!(
                    media = %item.path.display(),
                    gain_db = effective_gain_db,
                    "positive audio gain may clip"
                );
            }
            combined_gain(args.gain_db, item.gain_db)
                .with_context(|| format!("invalid effective gain for {}", item.path.display()))?;
        }
        // Constructing the runtime here prepares all UDP sockets before playback starts.
        if sequence.items.iter().all(|item| !item.external_audio) {
            return Self::new_playbin(args, sequence, forced_clock, drift_slew);
        }
        let first = &sequence.items[0];
        let pipeline = gst::Pipeline::new();
        let video_source = gst::ElementFactory::make("uridecodebin3")
            .name("video-source")
            .property("uri", &first.uri)
            .build()
            .context("uridecodebin3 is unavailable for video")?;
        let audio_source = gst::ElementFactory::make("uridecodebin3")
            .name("audio-source")
            .property("uri", &first.audio_uri)
            .build()
            .context("uridecodebin3 is unavailable for audio")?;

        let video_queue = make_named_element("queue", "video-queue", "video queue")?;
        let mut video_chain = vec![video_queue.clone()];
        if !args.kms_gl {
            let video_convert =
                make_named_element("videoconvert", "video-convert", "video converter")?;
            let video_scale = gst::ElementFactory::make("videoscale")
                .name("video-scale")
                .property("add-borders", args.fit == FitMode::Contain)
                .build()
                .context("videoscale is unavailable")?;
            video_chain.extend([video_convert, video_scale]);
        }
        if !args.kms_gl && args.fit == FitMode::Cover {
            let (width, height) = output_size(args)?;
            let width = i32::try_from(width).context("output width exceeds GStreamer limits")?;
            let height = i32::try_from(height).context("output height exceeds GStreamer limits")?;
            let crop = gst::ElementFactory::make("aspectratiocrop")
                .name("video-cover-crop")
                .property("aspect-ratio", gst::Fraction::new(width, height))
                .build()
                .context("cover mode requires the aspectratiocrop GStreamer element")?;
            video_chain.push(crop);
        }
        if !args.kms_gl
            && let Some((width, height)) = optional_output_size(args)
        {
            let width = i32::try_from(width).context("output width exceeds GStreamer limits")?;
            let height = i32::try_from(height).context("output height exceeds GStreamer limits")?;
            let caps = gst::Caps::builder("video/x-raw")
                .field("width", width)
                .field("height", height)
                .build();
            let caps_filter = gst::ElementFactory::make("capsfilter")
                .name("video-output-size")
                .property("caps", &caps)
                .build()
                .context("capsfilter is unavailable")?;
            video_chain.push(caps_filter);
        }
        let osd_overlay = make_osd_overlay()?;
        video_chain.push(osd_overlay.clone());
        let video_sink = select_video_sink(args)?;
        force_clock_sync(&video_sink, "video sink")?;
        configure_video_sink(&video_sink, args)?;
        let video_rate_control = video_sink.clone();
        video_chain.push(video_sink);

        let audio_queue = make_named_element("queue", "audio-queue", "audio queue")?;
        let audio_convert = make_named_element("audioconvert", "audio-convert", "audio converter")?;
        let audio_resample =
            make_named_element("audioresample", "audio-resample", "audio resampler")?;
        let audio_volume = gst::ElementFactory::make("volume")
            .name("audio-volume")
            .property("mute", args.mute || first.mute)
            .property("volume", combined_gain(args.gain_db, first.gain_db)?)
            .build()
            .context("volume is unavailable")?;
        let audio_sink = if let Some(name) = &args.audio_sink {
            make_element(name, "audio sink")?
        } else {
            make_element("autoaudiosink", "automatic audio sink")?
        };
        force_clock_sync(&audio_sink, "audio sink")?;
        let audio_rate_control = audio_sink.clone();
        // A cue without an audio stream must still be able to preroll its
        // video. The sink remains clock-synchronised once buffers arrive.
        if audio_sink.find_property("async").is_some() {
            audio_sink.set_property("async", false);
        }
        let audio_chain = [
            audio_queue.clone(),
            audio_convert,
            audio_resample,
            audio_volume.clone(),
            audio_sink,
        ];

        pipeline.add(&video_source)?;
        pipeline.add_many(&video_chain)?;
        gst::Element::link_many(&video_chain)?;
        pipeline.add(&audio_source)?;
        pipeline.add_many(&audio_chain)?;
        gst::Element::link_many(&audio_chain)?;
        select_decoded_streams(&video_source, gst::StreamType::VIDEO);
        select_decoded_streams(&audio_source, gst::StreamType::AUDIO);
        connect_decoded_pad(&video_source, &video_queue, "video_", "video");
        connect_decoded_pad(&audio_source, &audio_queue, "audio_", "audio");

        configure_kms_gl_context(&pipeline, args)?;

        let cursor = Arc::new(Mutex::new(PlaylistCursor::new(
            sequence.items.len(),
            sequence.looping,
            0,
        )));
        let audio_cursor = Arc::new(Mutex::new(PlaylistCursor::new(
            sequence.items.len(),
            sequence.looping,
            0,
        )));
        let handoff_in_progress = Arc::new(AtomicBool::new(false));
        let weak_video_source = video_source.downgrade();
        let weak_audio_source = audio_source.downgrade();
        let weak_volume = audio_volume.downgrade();
        let callback_cursor = cursor.clone();
        let video_handoff_in_progress = handoff_in_progress.clone();
        let video_uris: Vec<String> = sequence.items.iter().map(|item| item.uri.clone()).collect();
        video_source.connect("about-to-finish", false, move |_| {
            let next = callback_cursor
                .lock()
                .ok()
                .and_then(|mut cursor| cursor.queue_next());
            if let (Some(index), Some(source)) = (next, weak_video_source.upgrade()) {
                video_handoff_in_progress.store(true, Ordering::Release);
                debug!(next_item_index = index, next_uri = %video_uris[index], "queueing prepared video item");
                source.set_property("uri", &video_uris[index]);
            }
            None
        });
        let callback_audio_cursor = audio_cursor.clone();
        let audio_handoff_in_progress = handoff_in_progress.clone();
        let audio_items: Vec<(String, bool, f64)> = sequence
            .items
            .iter()
            .map(|item| {
                Ok((
                    item.audio_uri.clone(),
                    args.mute || item.mute,
                    combined_gain(args.gain_db, item.gain_db)?,
                ))
            })
            .collect::<Result<_>>()?;
        audio_source.connect("about-to-finish", false, move |_| {
            let next = callback_audio_cursor
                .lock()
                .ok()
                .and_then(|mut cursor| cursor.queue_next());
            if let Some(index) = next {
                audio_handoff_in_progress.store(true, Ordering::Release);
                if let Some(source) = weak_audio_source.upgrade() {
                    source.set_property("uri", &audio_items[index].0);
                }
                if let Some(volume) = weak_volume.upgrade() {
                    volume.set_property("mute", audio_items[index].1);
                    volume.set_property("volume", audio_items[index].2);
                }
                debug!(next_item_index = index, next_uri = %audio_items[index].0, "queueing prepared audio item");
            }
            None
        });
        if let Some(clock) = forced_clock {
            pipeline.use_clock(Some(clock));
        } else if args.clock == ClockPolicy::System {
            pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        }
        pipeline
            .set_state(gst::State::Paused)
            .with_context(|| preroll_request_error(args))?;
        wait_for_state(
            &pipeline,
            PREROLL_TIMEOUT,
            "media did not preroll before timeout",
        )
        .with_context(|| preroll_wait_error(args))?;
        if forced_clock.is_some() {
            configure_external_clock_audio_slaving(&pipeline)?;
        }
        log_selected_pipeline_elements(&pipeline);
        log_and_validate_video_negotiation(&osd_overlay, args)?;
        let clock = select_playback_clock(&pipeline, forced_clock, args.clock, first.has_audio)?;
        // Pin the clock selected during preroll so the one exported by the
        // Master is exactly the one used once the pipeline starts playing.
        pipeline.use_clock(Some(&clock));
        info!(
            clock = %clock.name(),
            clock_type = %clock.type_().name(),
            externally_driven = forced_clock.is_some(),
            "playback clock selected"
        );
        let duration = pipeline.query_duration::<gst::ClockTime>();
        info!(
            media = %sequence.items[0].path.display(),
            item_count = sequence.items.len(),
            duration_ns = duration.map(gst::ClockTime::nseconds),
            mute = args.mute || first.mute,
            gain_db = args.gain_db + first.gain_db,
            fit = ?args.fit,
            fullscreen = args.fullscreen,
            output_width = args.width,
            output_height = args.height,
            display = args.display.as_deref(),
            kms_gl = args.kms_gl,
            hwdec = ?args.hwdec,
            looped = sequence.looping,
            playlist = sequence.from_playlist,
            "media sequence prerolled"
        );
        let events = make_event_runtime(&sequence)?;
        Ok(Self {
            pipeline,
            video_source,
            audio_source,
            audio_volume,
            global_mute: args.mute,
            global_gain_db: args.gain_db,
            sequence,
            events,
            event_projection: Mutex::new(None),
            events_suppressed_until: Mutex::new(None),
            cursor,
            audio_cursor,
            role: PlayerRole::from_external_clock(forced_clock.is_some()),
            osd_overlay,
            osd_enabled: AtomicBool::new(false),
            osd_metadata: Mutex::new(None),
            clock_healthy: AtomicBool::new(true),
            clock: Mutex::new(clock),
            plan: Mutex::new(None),
            transport_state: Mutex::new(TransportState::Waiting),
            recovery_lock: Mutex::new(()),
            recovery_in_progress: AtomicBool::new(false),
            recovery_attempt: Arc::new(AtomicU64::new(0)),
            disconnect_count: AtomicU64::new(0),
            drift_policy: Mutex::new(DriftPolicy::default()),
            drift_slew: Mutex::new(DriftSlewController::new(drift_slew)),
            rate_control_targets: vec![video_rate_control, audio_rate_control],
            handoff_in_progress,
            separate_audio_handoff: true,
            boundary_mismatch: Mutex::new(None),
            scheduled_release: Arc::new(Mutex::new(None)),
            release_error: Arc::new(Mutex::new(None)),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn new_playbin(
        args: &PlaybackArgs,
        sequence: PlaybackSequence,
        forced_clock: Option<&gst::Clock>,
        drift_slew: bool,
    ) -> Result<Self> {
        let first = &sequence.items[0];
        let element = gst::ElementFactory::make("playbin3")
            .name("embedded-media-player")
            .property("uri", &first.uri)
            .property("mute", args.mute || first.mute)
            .property("volume", combined_gain(args.gain_db, first.gain_db)?)
            .property("force-aspect-ratio", args.fit == FitMode::Contain)
            .build()
            .context("playbin3 is unavailable")?;
        let (video_sink, osd_overlay) = build_playbin_video_sink(args)?;
        let video_rate_control = video_sink.clone();
        element.set_property("video-sink", &video_sink);
        let audio_sink = if let Some(name) = &args.audio_sink {
            make_element(name, "audio sink")?
        } else {
            make_element("autoaudiosink", "automatic audio sink")?
        };
        force_clock_sync(&audio_sink, "audio sink")?;
        element.set_property("audio-sink", &audio_sink);

        let cursor = Arc::new(Mutex::new(PlaylistCursor::new(
            sequence.items.len(),
            sequence.looping,
            0,
        )));
        let audio_cursor = Arc::new(Mutex::new(PlaylistCursor::new(
            sequence.items.len(),
            sequence.looping,
            0,
        )));
        let handoff_in_progress = Arc::new(AtomicBool::new(false));
        let callback_cursor = cursor.clone();
        let callback_handoff_in_progress = handoff_in_progress.clone();
        let weak_element = element.downgrade();
        let items: Vec<(String, bool, f64)> = sequence
            .items
            .iter()
            .map(|item| {
                Ok((
                    item.uri.clone(),
                    args.mute || item.mute,
                    combined_gain(args.gain_db, item.gain_db)?,
                ))
            })
            .collect::<Result<_>>()?;
        element.connect("about-to-finish", false, move |_| {
            let next = callback_cursor
                .lock()
                .ok()
                .and_then(|mut cursor| cursor.queue_next());
            if let (Some(index), Some(element)) = (next, weak_element.upgrade()) {
                callback_handoff_in_progress.store(true, Ordering::Release);
                element.set_property("mute", items[index].1);
                element.set_property("volume", items[index].2);
                element.set_property("uri", &items[index].0);
                debug!(next_item_index = index, next_uri = %items[index].0, "queueing prepared embedded-media item");
            }
            None
        });

        let pipeline = element
            .clone()
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("playbin3 did not create a GstPipeline"))?;
        configure_kms_gl_context(&pipeline, args)?;
        if let Some(clock) = forced_clock {
            pipeline.use_clock(Some(clock));
        } else if args.clock == ClockPolicy::System {
            pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        }
        pipeline
            .set_state(gst::State::Paused)
            .with_context(|| preroll_request_error(args))?;
        wait_for_state(
            &pipeline,
            PREROLL_TIMEOUT,
            "media did not preroll before timeout",
        )
        .with_context(|| preroll_wait_error(args))?;
        if forced_clock.is_some() {
            configure_external_clock_audio_slaving(&pipeline)?;
        }
        log_selected_pipeline_elements(&pipeline);
        log_and_validate_video_negotiation(&osd_overlay, args)?;
        let clock = select_playback_clock(&pipeline, forced_clock, args.clock, first.has_audio)?;
        pipeline.use_clock(Some(&clock));
        info!(
            clock = %clock.name(),
            clock_type = %clock.type_().name(),
            externally_driven = forced_clock.is_some(),
            "playback clock selected"
        );
        let duration = pipeline.query_duration::<gst::ClockTime>();
        info!(
            engine = "playbin3",
            media = %first.path.display(),
            item_count = sequence.items.len(),
            duration_ns = duration.map(gst::ClockTime::nseconds),
            mute = args.mute || first.mute,
            gain_db = args.gain_db + first.gain_db,
            fit = ?args.fit,
            fullscreen = args.fullscreen,
            output_width = args.width,
            output_height = args.height,
            display = args.display.as_deref(),
            kms_gl = args.kms_gl,
            hwdec = ?args.hwdec,
            looped = sequence.looping,
            playlist = sequence.from_playlist,
            "embedded-media sequence prerolled"
        );
        let events = make_event_runtime(&sequence)?;
        Ok(Self {
            pipeline,
            video_source: element.clone(),
            audio_source: element.clone(),
            audio_volume: element,
            global_mute: args.mute,
            global_gain_db: args.gain_db,
            sequence,
            events,
            event_projection: Mutex::new(None),
            events_suppressed_until: Mutex::new(None),
            cursor,
            audio_cursor,
            role: PlayerRole::from_external_clock(forced_clock.is_some()),
            osd_overlay,
            osd_enabled: AtomicBool::new(false),
            osd_metadata: Mutex::new(None),
            clock_healthy: AtomicBool::new(true),
            clock: Mutex::new(clock),
            plan: Mutex::new(None),
            transport_state: Mutex::new(TransportState::Waiting),
            recovery_lock: Mutex::new(()),
            recovery_in_progress: AtomicBool::new(false),
            recovery_attempt: Arc::new(AtomicU64::new(0)),
            disconnect_count: AtomicU64::new(0),
            drift_policy: Mutex::new(DriftPolicy::default()),
            drift_slew: Mutex::new(DriftSlewController::new(drift_slew)),
            // A seek sent through one playbin sink reaches the shared
            // demuxer, which emits the rate change to all of its streams.
            rate_control_targets: vec![video_rate_control],
            handoff_in_progress,
            separate_audio_handoff: false,
            boundary_mismatch: Mutex::new(None),
            scheduled_release: Arc::new(Mutex::new(None)),
            release_error: Arc::new(Mutex::new(None)),
        })
    }

    fn schedule_start(&self, delay: Duration) -> Result<ReleasePlan> {
        let now = ClockNs::new(self.clock()?.time().nseconds());
        let release_at = now
            .checked_add(delay)
            .ok_or_else(|| anyhow!("startup timestamp overflow"))?;
        let plan = ReleasePlan {
            generation: 1,
            operation: Operation::Start,
            transport_state: TransportState::Playing,
            media_position: MediaPositionNs::new(0),
            release_at,
        };
        let recovery_attempt = self.next_recovery_attempt();
        self.schedule_playing(plan, recovery_attempt, true)?;
        Ok(plan)
    }

    fn resynchronize(
        &self,
        clock: &gst::Clock,
        plan: ReleasePlan,
        reason: &'static str,
    ) -> Result<()> {
        let _recovery = self
            .recovery_lock
            .lock()
            .map_err(|_| anyhow!("recovery lock poisoned"))?;
        let recovery_attempt = self.next_recovery_attempt();
        self.recovery_in_progress.store(true, Ordering::Release);
        info!(
            recovery_attempt,
            recovery_reason = reason,
            target_position_ns = plan.media_position.get(),
            release_at_ns = plan.release_at.get(),
            "preparing pipeline recovery"
        );
        let result = self.resynchronize_inner(clock, plan, recovery_attempt, true);
        self.recovery_in_progress.store(false, Ordering::Release);
        if result.is_ok() {
            info!(
                recovery_attempt,
                recovery_reason = reason,
                "pipeline recovery prepared"
            );
        }
        result
    }

    fn apply_control_plan(&self, clock: &gst::Clock, plan: ReleasePlan) -> Result<()> {
        match plan.operation {
            Operation::Pause if plan.transport_state == TransportState::Paused => {
                self.schedule_pause(plan)
            }
            _ if plan.transport_state == TransportState::Paused => {
                self.prepare_paused(clock, plan, "operator-control")
            }
            _ => self.resynchronize(clock, plan, "operator-control"),
        }
    }

    fn prepare_paused(
        &self,
        clock: &gst::Clock,
        plan: ReleasePlan,
        reason: &'static str,
    ) -> Result<()> {
        self.resynchronize(clock, plan, reason)?;
        self.cancel_scheduled_release()?;
        *self
            .transport_state
            .lock()
            .map_err(|_| anyhow!("transport state lock poisoned"))? = TransportState::Paused;
        Ok(())
    }

    fn resynchronize_inner(
        &self,
        clock: &gst::Clock,
        plan: ReleasePlan,
        recovery_attempt: u64,
        update_authority: bool,
    ) -> Result<()> {
        if let Some(events) = &self.events {
            events
                .lock()
                .map_err(|_| anyhow!("event runtime lock poisoned"))?
                .mark_discontinuity();
            *self
                .events_suppressed_until
                .lock()
                .map_err(|_| anyhow!("event suppression lock poisoned"))? = Some(plan.release_at);
        }
        self.cancel_scheduled_release()?;
        let item_position = self.sequence.locate(plan.media_position.get())?;
        debug!(
            item_index = item_position.index,
            item_local_ns = item_position.local_ns,
            "resetting pipeline for synchronization"
        );
        self.pipeline
            .set_state(gst::State::Null)
            .context("failed to reset media for synchronization")?;
        self.drift_slew
            .lock()
            .map_err(|_| anyhow!("drift slew lock poisoned"))?
            .clear();
        wait_for_state(
            &self.pipeline,
            PREROLL_TIMEOUT,
            "media did not stop before synchronization timeout",
        )?;
        debug!(
            item_index = item_position.index,
            "pipeline stopped for synchronization item"
        );
        self.cursor
            .lock()
            .map_err(|_| anyhow!("playlist cursor lock poisoned"))?
            .reset(item_position.index);
        self.audio_cursor
            .lock()
            .map_err(|_| anyhow!("audio playlist cursor lock poisoned"))?
            .reset(item_position.index);
        let item = &self.sequence.items[item_position.index];
        self.video_source.set_property("uri", &item.uri);
        self.audio_source.set_property("uri", &item.audio_uri);
        self.audio_volume
            .set_property("mute", self.global_mute || item.mute);
        self.audio_volume
            .set_property("volume", combined_gain(self.global_gain_db, item.gain_db)?);
        self.pipeline.use_clock(Some(clock));
        *self
            .clock
            .lock()
            .map_err(|_| anyhow!("clock lock poisoned"))? = clock.clone();
        self.pipeline
            .set_state(gst::State::Paused)
            .context("failed to preroll synchronization item")?;
        wait_for_state(
            &self.pipeline,
            PREROLL_TIMEOUT,
            "synchronization item did not preroll before timeout",
        )?;
        if item_position.local_ns > 0 {
            self.pipeline
                .seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::from_nseconds(item_position.local_ns),
                )
                .context("failed to seek to synchronization position")?;
            wait_for_state(
                &self.pipeline,
                PREROLL_TIMEOUT,
                "synchronization seek did not preroll before timeout",
            )?;
        }
        self.schedule_playing(plan, recovery_attempt, update_authority)
    }

    fn schedule_playing(
        &self,
        plan: ReleasePlan,
        recovery_attempt: u64,
        update_authority: bool,
    ) -> Result<()> {
        self.cancel_scheduled_release()?;
        *self
            .release_error
            .lock()
            .map_err(|_| anyhow!("release error lock poisoned"))? = None;
        if update_authority {
            *self
                .plan
                .lock()
                .map_err(|_| anyhow!("plan lock poisoned"))? = Some(plan);
            *self
                .transport_state
                .lock()
                .map_err(|_| anyhow!("transport state lock poisoned"))? = TransportState::Playing;
        }

        let clock = self.clock()?;
        let now = ClockNs::new(clock.time().nseconds());
        if plan.release_at <= now {
            return self.begin_playing();
        }

        let release_id =
            clock.new_single_shot_id(gst::ClockTime::from_nseconds(plan.release_at.get()));
        *self
            .scheduled_release
            .lock()
            .map_err(|_| anyhow!("scheduled release lock poisoned"))? = Some(ScheduledRelease {
            release_id: release_id.clone(),
        });

        let release_worker = ReleaseWorker {
            pipeline: self.pipeline.clone(),
            clock,
            plan,
            release_id,
            recovery_attempt,
            active_recovery_attempt: self.recovery_attempt.clone(),
            scheduled_release: self.scheduled_release.clone(),
            release_error: self.release_error.clone(),
            target_state: gst::State::Playing,
        };
        let worker = thread::Builder::new()
            .name("syncplayer-release".to_owned())
            .spawn(move || release_worker.run());
        if let Err(error) = worker {
            self.cancel_scheduled_release()?;
            return Err(error).context("failed to spawn release worker");
        }

        Ok(())
    }

    fn schedule_pause(&self, plan: ReleasePlan) -> Result<()> {
        self.cancel_scheduled_release()?;
        *self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))? = Some(plan);
        *self
            .transport_state
            .lock()
            .map_err(|_| anyhow!("transport state lock poisoned"))? = TransportState::Paused;
        let clock = self.clock()?;
        let now = ClockNs::new(clock.time().nseconds());
        if plan.release_at <= now {
            self.pipeline
                .set_state(gst::State::Paused)
                .context("failed to pause pipeline")?;
            return Ok(());
        }
        let recovery_attempt = self.next_recovery_attempt();
        let release_id =
            clock.new_single_shot_id(gst::ClockTime::from_nseconds(plan.release_at.get()));
        *self
            .scheduled_release
            .lock()
            .map_err(|_| anyhow!("scheduled release lock poisoned"))? = Some(ScheduledRelease {
            release_id: release_id.clone(),
        });
        let worker = ReleaseWorker {
            pipeline: self.pipeline.clone(),
            clock,
            plan,
            release_id,
            recovery_attempt,
            active_recovery_attempt: self.recovery_attempt.clone(),
            scheduled_release: self.scheduled_release.clone(),
            release_error: self.release_error.clone(),
            target_state: gst::State::Paused,
        };
        thread::Builder::new()
            .name("syncplayer-pause".to_owned())
            .spawn(move || worker.run())
            .context("failed to spawn pause worker")?;
        Ok(())
    }

    fn next_recovery_attempt(&self) -> u64 {
        self.recovery_attempt.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn begin_playing(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("failed to request PLAYING")?;
        Ok(())
    }

    fn cancel_scheduled_release(&self) -> Result<()> {
        let scheduled = self
            .scheduled_release
            .lock()
            .map_err(|_| anyhow!("scheduled release lock poisoned"))?
            .take();
        if let Some(scheduled) = scheduled {
            scheduled.unschedule();
        }
        Ok(())
    }

    fn join_plan(&self, generation: u64, release_at: ClockNs) -> Result<ReleasePlan> {
        let _recovery = self
            .recovery_lock
            .lock()
            .map_err(|_| anyhow!("recovery lock poisoned"))?;
        self.plan_for_release(generation, release_at)
    }

    fn plan_for_release(&self, generation: u64, release_at: ClockNs) -> Result<ReleasePlan> {
        let current_plan = self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))?
            .ok_or_else(|| anyhow!("Master playback has no authoritative plan"))?;
        let elapsed = release_at
            .get()
            .saturating_sub(current_plan.release_at.get());
        Ok(ReleasePlan {
            generation,
            operation: Operation::Goto,
            transport_state: TransportState::Playing,
            media_position: MediaPositionNs::new(
                self.sequence
                    .project(current_plan.media_position.get(), elapsed)?,
            ),
            release_at,
        })
    }

    fn authoritative_snapshot(&self, runtime: RuntimeState) -> Result<StateSnapshot> {
        let item = ItemPosition {
            index: runtime.cue_index,
            local_ns: runtime.anchor_cue_position.get(),
        };
        self.update_osd_metadata(OsdMetadata {
            cue_index: runtime.cue_index,
            cue_id: runtime.cue_id.clone(),
            transport_state: runtime.transport_state,
        })?;
        let media_position = self
            .sequence
            .position_for_snapshot(item, runtime.playlist_iteration)?;
        let debug_loop = runtime.debug_loop.map(|debug_loop| DebugLoopState {
            start: MediaPositionNs::new(debug_loop.start_ns),
            end: MediaPositionNs::new(debug_loop.end_ns),
            events_enabled: debug_loop.events_enabled,
        });
        self.update_event_projection(EventProjection {
            session: runtime.session,
            generation: runtime.generation,
            anchor_clock: runtime.anchor_clock,
            anchor_media_position: media_position,
            anchor_item: item,
            cue_occurrence: runtime.cue_occurrence,
            playlist_iteration: runtime.playlist_iteration,
            transport_state: runtime.transport_state,
            debug_loop,
        })?;
        Ok(StateSnapshot {
            generation: runtime.generation,
            program_id: runtime.program_id,
            program_hash: runtime.program_hash,
            transport_state: runtime.transport_state,
            operation: runtime.operation,
            media_position,
            anchor_clock: runtime.anchor_clock,
            item_index: u64::try_from(item.index).context("item index exceeds u64")?,
            cue_id: runtime.cue_id,
            cue_occurrence: runtime.cue_occurrence,
            item_position: MediaPositionNs::new(item.local_ns),
            cue_duration: runtime.cue_duration,
            playlist_iteration: runtime.playlist_iteration,
            debug_loop,
            osd_enabled: self.osd_enabled(),
        })
    }

    fn accept_authoritative_snapshot(
        &self,
        snapshot: &StateSnapshot,
        authority: Authority,
    ) -> Result<()> {
        if !validate_snapshot_generation(snapshot, authority)? {
            debug!(
                snapshot_generation = snapshot.generation,
                transport_generation = authority.generation,
                "stale authoritative snapshot ignored"
            );
            return Ok(());
        }
        let item_index = usize::try_from(snapshot.item_index)
            .context("snapshot item index does not fit this host")?;
        if snapshot.program_id != self.sequence.program_id
            || snapshot.program_hash != self.sequence.program_hash
        {
            bail!("authoritative snapshot identifies a different program");
        }
        let local_cue_id = self
            .sequence
            .cue_ids
            .get(item_index)
            .ok_or_else(|| anyhow!("snapshot cue index {item_index} does not exist"))?;
        if &snapshot.cue_id != local_cue_id {
            bail!(
                "snapshot cue identity mismatch: index {item_index} is '{}' locally, Master announced '{}'",
                local_cue_id,
                snapshot.cue_id
            );
        }
        let master_duration_ns = snapshot.cue_duration.get();
        if master_duration_ns == 0 {
            bail!("snapshot announced a zero cue duration");
        }
        if snapshot.item_position.get() > master_duration_ns {
            bail!("snapshot cue position exceeds the authoritative cue duration");
        }
        let local_duration_ns = self.sequence.cue_durations_ns[item_index];
        if snapshot.item_position.get() >= local_duration_ns {
            debug!(
                item_index,
                authoritative_item_position_ns = snapshot.item_position.get(),
                local_duration_ns,
                mismatch_ns = snapshot.item_position.get() - local_duration_ns,
                "holding shorter local rendition at its last valid cue position"
            );
        }
        let local_media_position = self.sequence.position_for_remote_snapshot(
            ItemPosition {
                index: item_index,
                local_ns: snapshot.item_position.get(),
            },
            snapshot.playlist_iteration,
        )?;
        self.set_osd_enabled(snapshot.osd_enabled)?;
        self.update_osd_metadata(OsdMetadata {
            cue_index: item_index,
            cue_id: snapshot.cue_id.clone(),
            transport_state: snapshot.transport_state,
        })?;
        self.update_event_projection(EventProjection {
            session: authority.session,
            generation: snapshot.generation,
            anchor_clock: snapshot.anchor_clock,
            anchor_media_position: local_media_position,
            anchor_item: ItemPosition {
                index: item_index,
                local_ns: snapshot.item_position.get(),
            },
            cue_occurrence: snapshot.cue_occurrence,
            playlist_iteration: snapshot.playlist_iteration,
            transport_state: snapshot.transport_state,
            debug_loop: snapshot.debug_loop,
        })?;

        let refreshed = ReleasePlan {
            generation: snapshot.generation,
            operation: snapshot.operation,
            transport_state: snapshot.transport_state,
            media_position: local_media_position,
            release_at: snapshot.anchor_clock,
        };
        let mut plan = self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))?;
        if plan.as_ref() != Some(&refreshed) {
            debug!(
                transport_generation = snapshot.generation,
                anchor_clock_ns = snapshot.anchor_clock.get(),
                authoritative_position_ns = snapshot.media_position.get(),
                local_anchor_position_ns = local_media_position.get(),
                item_index,
                item_position_ns = snapshot.item_position.get(),
                playlist_iteration = snapshot.playlist_iteration,
                "authoritative state snapshot refreshed without seeking"
            );
            *plan = Some(refreshed);
        }
        *self
            .transport_state
            .lock()
            .map_err(|_| anyhow!("transport state lock poisoned"))? = snapshot.transport_state;
        Ok(())
    }

    fn monitor(
        &self,
        initial_plan: ReleasePlan,
        master_transport: Option<&Arc<Mutex<TransportController>>>,
        diagnostics: Option<(&DiagnosticsHub, &str)>,
    ) -> Result<()> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| anyhow!("pipeline has no bus"))?;
        let mut next_telemetry = Instant::now();
        loop {
            if let Some(message) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(100),
                &[
                    gst::MessageType::Error,
                    gst::MessageType::Eos,
                    gst::MessageType::ClockLost,
                ],
            ) {
                match message.view() {
                    gst::MessageView::Error(error_message) => {
                        error!(source = ?error_message.src().map(GstObjectExt::path_string), error = %error_message.error(), debug = ?error_message.debug(), "GStreamer error");
                        // A failed streaming pipeline can block indefinitely while being driven
                        // to NULL. Return the fatal error immediately so the process supervisor
                        // can restart us instead of leaving a blank but apparently live player.
                        return Err(anyhow!(
                            "playback pipeline failed: {}",
                            error_message.error()
                        ));
                    }
                    gst::MessageView::Eos(..) => {
                        if self.handle_eos(master_transport)? {
                            info!("end of stream");
                            break;
                        }
                        continue;
                    }
                    gst::MessageView::ClockLost(..) => {
                        self.set_clock_health(false);
                        match self.role {
                            PlayerRole::Slave => warn!(
                                recovery_reason = "clock-lost",
                                "Slave pipeline reported CLOCK_LOST; shared clock marked unhealthy and local drift repair disabled"
                            ),
                            PlayerRole::Master => {
                                warn!(
                                    recovery_reason = "clock-lost",
                                    "Master pipeline reported CLOCK_LOST; rebuilding clock selection"
                                );
                                self.recover_master_clock(master_transport)?;
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }
            if Instant::now() >= next_telemetry {
                self.process_telemetry(initial_plan)?;
                if let Some((hub, player_id)) = &diagnostics
                    && let Some(sample) = self.diagnostic_telemetry()?
                {
                    hub.record(player_id, sample)
                        .context("failed to record Master diagnostics")?;
                }
                next_telemetry = Instant::now() + TELEMETRY_INTERVAL;
            }
            self.tick_events()?;
            if let Some(error) = self
                .release_error
                .lock()
                .map_err(|_| anyhow!("release error lock poisoned"))?
                .take()
            {
                return Err(anyhow!(error));
            }
        }
        self.pipeline
            .set_state(gst::State::Null)
            .context("failed to stop pipeline")?;
        Ok(())
    }

    fn update_event_projection(&self, projection: EventProjection) -> Result<()> {
        if self.events.is_none() {
            return Ok(());
        }
        *self
            .event_projection
            .lock()
            .map_err(|_| anyhow!("event projection lock poisoned"))? = Some(projection);
        self.tick_events()
    }

    #[allow(clippy::too_many_lines)]
    fn tick_events(&self) -> Result<()> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        let Some(projection) = *self
            .event_projection
            .lock()
            .map_err(|_| anyhow!("event projection lock poisoned"))?
        else {
            return Ok(());
        };
        let now = ClockNs::new(self.clock()?.time().nseconds());
        let mut suppressed_until = self
            .events_suppressed_until
            .lock()
            .map_err(|_| anyhow!("event suppression lock poisoned"))?;
        if suppressed_until.is_some_and(|deadline| now < deadline) {
            events
                .lock()
                .map_err(|_| anyhow!("event runtime lock poisoned"))?
                .mark_discontinuity();
            return Ok(());
        }
        *suppressed_until = None;
        drop(suppressed_until);
        let elapsed_ns = if projection.transport_state == TransportState::Playing {
            now.get().saturating_sub(projection.anchor_clock.get())
        } else {
            0
        };
        let (item, playlist_iteration, cue_occurrence) =
            if let Some(debug_loop) = projection.debug_loop {
                let loop_length = debug_loop.end.get().saturating_sub(debug_loop.start.get());
                if loop_length == 0 {
                    return Ok(());
                }
                let from_start = projection
                    .anchor_item
                    .local_ns
                    .saturating_sub(debug_loop.start.get());
                let offset =
                    (u128::from(from_start) + u128::from(elapsed_ns)) % u128::from(loop_length);
                (
                    ItemPosition {
                        index: projection.anchor_item.index,
                        local_ns: debug_loop.start.get()
                            + u64::try_from(offset).expect("modulo-u64 result fits u64"),
                    },
                    projection.playlist_iteration,
                    projection.cue_occurrence,
                )
            } else {
                let Ok(position) = self
                    .sequence
                    .project(projection.anchor_media_position.get(), elapsed_ns)
                else {
                    return Ok(());
                };
                let Ok(item) = self.sequence.locate(position) else {
                    return Ok(());
                };
                let playlist_iteration = self
                    .sequence
                    .timeline
                    .as_ref()
                    .map_or(0, |timeline| timeline.playlist_iteration(position));
                let cue_count = u64::try_from(self.sequence.cue_ids.len())
                    .context("cue count does not fit u64")?;
                let anchor_index = u64::try_from(projection.anchor_item.index)
                    .context("anchor cue index does not fit u64")?;
                let item_index = u64::try_from(item.index).context("cue index does not fit u64")?;
                let anchor_ordinal = projection
                    .playlist_iteration
                    .saturating_mul(cue_count)
                    .saturating_add(anchor_index);
                let ordinal = playlist_iteration
                    .saturating_mul(cue_count)
                    .saturating_add(item_index);
                (
                    item,
                    playlist_iteration,
                    projection
                        .cue_occurrence
                        .saturating_add(ordinal.saturating_sub(anchor_ordinal)),
                )
            };
        events
            .lock()
            .map_err(|_| anyhow!("event runtime lock poisoned"))?
            .observe(EventObservation {
                session: projection.session,
                generation: projection.generation,
                cue_index: item.index,
                cue_occurrence,
                playlist_iteration,
                cue_position_ns: item.local_ns,
                transport_state: projection.transport_state,
                debug_loop_events_enabled: projection
                    .debug_loop
                    .map(|debug_loop| debug_loop.events_enabled),
            });
        Ok(())
    }

    /// Returns true only for the authoritative end of a non-looping program.
    fn handle_eos(
        &self,
        master_transport: Option<&Arc<Mutex<TransportController>>>,
    ) -> Result<bool> {
        if self.role == PlayerRole::Slave {
            let current_item = self
                .cursor
                .lock()
                .map_err(|_| anyhow!("playlist cursor lock poisoned"))?
                .current();
            let current_media = self
                .sequence
                .items
                .get(current_item)
                .map(|item| item.path.display().to_string());
            warn!(
                current_item,
                ?current_media,
                "Slave reached local EOS; holding the current logical cue until authoritative state advances"
            );
            self.pipeline
                .set_state(gst::State::Paused)
                .context("failed to hold Slave pipeline after local EOS")?;
            return Ok(false);
        }

        let transport = master_transport
            .ok_or_else(|| anyhow!("Master player has no authoritative transport"))?;
        let now = ClockNs::new(self.clock()?.time().nseconds());
        let decision = transport
            .lock()
            .map_err(|_| anyhow!("transport lock poisoned"))?
            .master_eos(now);
        if let EosDecision::HeldEarly { remaining_ns } = decision {
            warn!(
                remaining_ns,
                "Master reached EOS before its authoritative cue boundary; holding the cue"
            );
            self.pipeline
                .set_state(gst::State::Paused)
                .context("failed to hold early Master EOS")?;
            return Ok(false);
        }
        if decision == EosDecision::Advanced {
            debug!("Master EOS confirmed an authoritative cue transition");
        }
        if self.sequence.looping {
            bail!(
                "Master looping pipeline reached terminal EOS instead of performing its prepared handoff"
            );
        }
        let current = self
            .cursor
            .lock()
            .map_err(|_| anyhow!("playlist cursor lock poisoned"))?
            .current();
        if current + 1 < self.sequence.items.len() {
            bail!("media sequence reached EOS before its final item (cursor item {current})");
        }
        Ok(true)
    }

    fn recover_master_clock(
        &self,
        master_transport: Option<&Arc<Mutex<TransportController>>>,
    ) -> Result<()> {
        let _recovery = self
            .recovery_lock
            .lock()
            .map_err(|_| anyhow!("recovery lock poisoned"))?;
        let clock = self.clock()?;
        self.pipeline
            .set_state(gst::State::Paused)
            .context("failed to pause Master during clock recovery")?;
        wait_for_state(
            &self.pipeline,
            PREROLL_TIMEOUT,
            "Master did not pause during clock recovery",
        )?;
        // The exported clock remains the installation time authority. Pin it
        // again before PLAYING so a disappearing sink clock cannot silently
        // replace the clock domain seen by Slaves.
        self.pipeline.use_clock(Some(&clock));
        self.pipeline
            .set_state(gst::State::Playing)
            .context("failed to resume Master after clock recovery")?;
        let now = ClockNs::new(clock.time().nseconds());
        master_transport
            .ok_or_else(|| anyhow!("Master player has no authoritative transport"))?
            .lock()
            .map_err(|_| anyhow!("transport lock poisoned"))?
            .reanchor(now);
        self.set_clock_health(true);
        info!(
            recovery_reason = "clock-lost",
            clock = %clock.name(),
            "Master clock recovery completed"
        );
        Ok(())
    }

    fn process_telemetry(&self, fallback_plan: ReleasePlan) -> Result<()> {
        let Some(sample) = self.position_sample(fallback_plan)? else {
            self.drift_policy
                .lock()
                .map_err(|_| anyhow!("drift policy lock poisoned"))?
                .suppress();
            self.suppress_drift_slew()?;
            debug!("position query did not produce a reliable drift sample");
            return Ok(());
        };
        debug!(
            recovery_attempt = sample.recovery_attempt,
            clock_ns = sample.clock_time.get(),
            expected_sequence_position_ns = sample.expected_sequence_position.get(),
            expected_item_index = sample.expected_item.index,
            expected_item_local_ns = sample.expected_item.local_ns,
            expected_playlist_iteration = sample.expected_playlist_iteration,
            actual_sequence_position_ns = sample.actual_sequence_position.get(),
            actual_item_index = sample.actual_item.index,
            actual_item_local_ns = sample.actual_item.local_ns,
            actual_uri = sample.actual_uri,
            error_ns = sample.error_ns,
            pipeline_state = ?sample.pipeline_state,
            transition_in_progress = sample.transition_in_progress,
            "position telemetry"
        );
        self.diagnose_boundary_mismatch(&sample)?;
        self.finish_safe_handoff(&sample)?;

        if sample.transition_in_progress || !self.clock_healthy.load(Ordering::Acquire) {
            self.drift_policy
                .lock()
                .map_err(|_| anyhow!("drift policy lock poisoned"))?
                .suppress();
            self.suppress_drift_slew()?;
            debug!(
                transition_in_progress = sample.transition_in_progress,
                clock_healthy = self.clock_healthy.load(Ordering::Acquire),
                "drift sample suppressed"
            );
            return Ok(());
        }

        self.process_drift_slew(&sample)?;

        let action = self
            .drift_policy
            .lock()
            .map_err(|_| anyhow!("drift policy lock poisoned"))?
            .observe(Instant::now(), sample.error_ns);
        match action {
            DriftAction::None => {}
            DriftAction::Warn { consecutive } => warn!(
                error_ns = sample.error_ns,
                consecutive_drift_samples = consecutive,
                "sustained playback drift detected"
            ),
            DriftAction::Repair { consecutive } => {
                warn!(
                    error_ns = sample.error_ns,
                    consecutive_drift_samples = consecutive,
                    recovery_reason = "local-drift",
                    "material playback drift persisted; repairing local pipeline"
                );
                self.repair_local_drift(&sample)?;
            }
            DriftAction::Cooldown { remaining } => debug!(
                error_ns = sample.error_ns,
                cooldown_remaining_ms = remaining.as_millis(),
                "drift recovery suppressed during cooldown"
            ),
        }
        Ok(())
    }

    fn finish_safe_handoff(&self, sample: &PositionSample) -> Result<()> {
        if !self.handoff_in_progress.load(Ordering::Acquire) {
            return Ok(());
        }
        let video_item = self
            .cursor
            .lock()
            .map_err(|_| anyhow!("playlist cursor lock poisoned"))?
            .current();
        let audio_settled = !self.separate_audio_handoff
            || self
                .audio_cursor
                .lock()
                .map_err(|_| anyhow!("audio playlist cursor lock poisoned"))?
                .current()
                == sample.expected_item.index;
        let settled = !self.recovery_in_progress.load(Ordering::Acquire)
            && self
                .scheduled_release
                .lock()
                .map_err(|_| anyhow!("scheduled release lock poisoned"))?
                .is_none()
            && sample.pipeline_state == gst::State::Playing
            && self.pipeline.pending_state() == gst::State::VoidPending
            && sample.actual_item.index == sample.expected_item.index
            && video_item == sample.expected_item.index
            && audio_settled;
        if settled {
            self.handoff_in_progress.store(false, Ordering::Release);
            debug!(
                item_index = sample.expected_item.index,
                "playlist handoff settled; drift slew may resume after this sample"
            );
        }
        Ok(())
    }

    fn process_drift_slew(&self, sample: &PositionSample) -> Result<()> {
        let current_item = self
            .cursor
            .lock()
            .map_err(|_| anyhow!("playlist cursor lock poisoned"))?
            .current();
        let audio_current = !self.separate_audio_handoff
            || self
                .audio_cursor
                .lock()
                .map_err(|_| anyhow!("audio playlist cursor lock poisoned"))?
                .current()
                == sample.expected_item.index;
        if self.handoff_in_progress.load(Ordering::Acquire)
            || current_item != sample.expected_item.index
            || !audio_current
            || self.pipeline.current_state() != gst::State::Playing
            || self.pipeline.pending_state() != gst::State::VoidPending
        {
            self.drift_slew
                .lock()
                .map_err(|_| anyhow!("drift slew lock poisoned"))?
                .suppress();
            debug!("drift slew rate update deferred across playlist handoff");
            return Ok(());
        }
        let action = self
            .drift_slew
            .lock()
            .map_err(|_| anyhow!("drift slew lock poisoned"))?
            .observe(sample.error_ns);
        let (rate, active) = match action {
            DriftSlewAction::None => return Ok(()),
            DriftSlewAction::Apply(rate) => (rate, true),
            DriftSlewAction::Reset => (1.0, false),
        };
        if let Err(error) = self.set_instant_playback_rate(rate) {
            self.drift_slew
                .lock()
                .map_err(|_| anyhow!("drift slew lock poisoned"))?
                .disable();
            warn!(
                %error,
                requested_rate = rate,
                "drift slew disabled because the pipeline rejected an instant rate change"
            );
            if active && let Err(reset_error) = self.set_instant_playback_rate(1.0) {
                warn!(
                    %reset_error,
                    "failed to restore normal playback rate after a partial drift slew"
                );
            }
            return Ok(());
        }
        info!(
            playback_rate = rate,
            error_ns = sample.error_ns,
            drift_slew_active = active,
            "Slave drift slew rate updated"
        );
        Ok(())
    }

    fn suppress_drift_slew(&self) -> Result<()> {
        let was_active = self
            .drift_slew
            .lock()
            .map_err(|_| anyhow!("drift slew lock poisoned"))?
            .suppress();
        if was_active {
            debug!(
                "drift slew restoration deferred until the pipeline is safely outside a transition"
            );
        }
        Ok(())
    }

    fn set_instant_playback_rate(&self, rate: f64) -> Result<()> {
        for target in &self.rate_control_targets {
            target
                .seek(
                    rate,
                    gst::SeekFlags::INSTANT_RATE_CHANGE,
                    gst::SeekType::None,
                    gst::ClockTime::NONE,
                    gst::SeekType::None,
                    gst::ClockTime::NONE,
                )
                .with_context(|| {
                    format!(
                        "rate-control target '{}' rejected rate {rate:.5}",
                        target.name()
                    )
                })?;
        }
        Ok(())
    }

    fn diagnose_boundary_mismatch(&self, sample: &PositionSample) -> Result<()> {
        let mut previous = self
            .boundary_mismatch
            .lock()
            .map_err(|_| anyhow!("boundary mismatch lock poisoned"))?;
        if sample.expected_item.index == sample.actual_item.index {
            *previous = None;
            return Ok(());
        }
        let mismatch = (
            sample.expected_item.index,
            sample.actual_item.index,
            sample.expected_playlist_iteration,
        );
        if previous.as_ref() != Some(&mismatch) {
            warn!(
                authoritative_cue_index = sample.expected_item.index,
                local_cue_index = sample.actual_item.index,
                playlist_iteration = sample.expected_playlist_iteration,
                "local cue boundary differs from the Master; treating the local transition as diagnostic only"
            );
            *previous = Some(mismatch);
        }
        Ok(())
    }

    fn position_sample(&self, fallback_plan: ReleasePlan) -> Result<Option<PositionSample>> {
        let _recovery = match self.recovery_lock.try_lock() {
            Ok(recovery) => recovery,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                bail!("recovery lock poisoned");
            }
        };
        let plan = self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))?
            .as_ref()
            .copied()
            .unwrap_or(fallback_plan);
        let clock = self.clock()?;
        let clock_now = clock.time().nseconds();
        let transport_state = *self
            .transport_state
            .lock()
            .map_err(|_| anyhow!("transport state lock poisoned"))?;
        let elapsed_ns = if transport_state == TransportState::Playing {
            clock_now.saturating_sub(plan.release_at.get())
        } else {
            0
        };
        let expected_sequence_position = MediaPositionNs::new(
            self.sequence
                .project(plan.media_position.get(), elapsed_ns)?,
        );
        let expected_item = self.sequence.locate(expected_sequence_position.get())?;
        let expected_playlist_iteration = self.sequence.timeline.as_ref().map_or(0, |timeline| {
            timeline.playlist_iteration(expected_sequence_position.get())
        });
        let Some(actual_local) = self.pipeline.query_position::<gst::ClockTime>() else {
            return Ok(None);
        };
        // `uridecodebin3` mutates its `current-uri` storage on a streaming
        // thread during gapless handoff. Reading that GObject string from the
        // monitor thread can race with the mutation (and has produced invalid
        // UTF-8 panics in glib). The playlist cursor and URI list are owned by
        // Rust, so use them as the safe transition identity instead.
        let actual_item_index = self
            .cursor
            .lock()
            .map_err(|_| anyhow!("playlist cursor lock poisoned"))?
            .current();
        let actual_uri = self.sequence.items[actual_item_index].uri.clone();
        let actual_item = ItemPosition {
            index: actual_item_index,
            local_ns: actual_local.nseconds(),
        };
        let actual_sequence_position = match self
            .sequence
            .sequence_position_near(actual_item, expected_sequence_position)
        {
            Ok(position) => position,
            Err(error) => {
                debug!(%error, "item-local position is ambiguous at a cue boundary");
                return Ok(None);
            }
        };
        let pipeline_state = self.pipeline.current_state();
        let transition_in_progress = self.recovery_in_progress.load(Ordering::Acquire)
            || self.handoff_in_progress.load(Ordering::Acquire)
            || self
                .scheduled_release
                .lock()
                .map_err(|_| anyhow!("scheduled release lock poisoned"))?
                .is_some()
            || pipeline_state != gst::State::Playing
            || self.pipeline.pending_state() != gst::State::VoidPending
            || actual_item_index != expected_item.index;
        Ok(Some(PositionSample {
            recovery_attempt: self.recovery_attempt.load(Ordering::Acquire),
            clock_time: ClockNs::new(clock_now),
            expected_sequence_position,
            expected_item,
            expected_playlist_iteration,
            actual_sequence_position,
            actual_item,
            actual_uri,
            error_ns: self
                .sequence
                .position_error(actual_sequence_position, expected_sequence_position),
            pipeline_state,
            transition_in_progress,
        }))
    }

    fn diagnostic_telemetry(&self) -> Result<Option<DiagnosticTelemetry>> {
        let Some(plan) = *self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))?
        else {
            return Ok(None);
        };
        let Some(sample) = self.position_sample(plan)? else {
            return Ok(None);
        };
        let error_ns = i64::try_from(sample.error_ns).unwrap_or_else(|_| {
            if sample.error_ns.is_negative() {
                i64::MIN
            } else {
                i64::MAX
            }
        });
        Ok(Some(DiagnosticTelemetry {
            clock_time: sample.clock_time,
            generation: plan.generation,
            expected_position: sample.expected_sequence_position,
            actual_position: sample.actual_sequence_position,
            error_ns,
            cue_index: u64::try_from(sample.actual_item.index)
                .context("cue index does not fit diagnostic protocol")?,
            cue_position: MediaPositionNs::new(sample.actual_item.local_ns),
            pipeline_playing: sample.pipeline_state == gst::State::Playing,
            transition_in_progress: sample.transition_in_progress,
            clock_healthy: self.clock_healthy.load(Ordering::Acquire),
            disconnect_count: self.disconnect_count.load(Ordering::Acquire),
            recovery_count: self.recovery_attempt.load(Ordering::Acquire),
        }))
    }

    fn repair_local_drift(&self, sample: &PositionSample) -> Result<()> {
        let _recovery = self
            .recovery_lock
            .lock()
            .map_err(|_| anyhow!("recovery lock poisoned"))?;
        let active_attempt = self.recovery_attempt.load(Ordering::Acquire);
        if active_attempt != sample.recovery_attempt {
            debug!(
                sample_recovery_attempt = sample.recovery_attempt,
                active_recovery_attempt = active_attempt,
                "local drift repair cancelled because a newer recovery completed"
            );
            return Ok(());
        }
        if !self.clock_healthy.load(Ordering::Acquire) {
            debug!("local drift repair cancelled because the playback clock is unhealthy");
            return Ok(());
        }

        let clock = self.clock()?;
        let release_at = ClockNs::new(clock.time().nseconds())
            .checked_add(LOCAL_RECOVERY_LEAD)
            .ok_or_else(|| anyhow!("local recovery timestamp overflow"))?;
        let generation = self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))?
            .as_ref()
            .map_or(1, |plan| plan.generation);
        let plan = match self.plan_for_release(generation, release_at) {
            Ok(plan) => plan,
            Err(error) => {
                warn!(%error, "local drift repair skipped because no future release can be planned");
                return Ok(());
            }
        };
        let recovery_attempt = self.next_recovery_attempt();
        self.recovery_in_progress.store(true, Ordering::Release);
        info!(
            recovery_attempt,
            recovery_reason = "local-drift",
            position_before_ns = sample.actual_sequence_position.get(),
            target_position_ns = plan.media_position.get(),
            release_at_ns = plan.release_at.get(),
            error_before_ns = sample.error_ns,
            "preparing local drift recovery"
        );
        let result = self.resynchronize_inner(&clock, plan, recovery_attempt, false);
        self.recovery_in_progress.store(false, Ordering::Release);
        result
    }

    fn osd_enabled(&self) -> bool {
        self.osd_enabled.load(Ordering::Acquire)
    }

    fn set_osd_enabled(&self, enabled: bool) -> Result<()> {
        let changed = self.osd_enabled.swap(enabled, Ordering::AcqRel) != enabled;
        if changed {
            self.osd_overlay.set_property("silent", !enabled);
        }
        if enabled {
            self.refresh_osd_text()?;
        }
        Ok(())
    }

    fn update_osd_metadata(&self, metadata: OsdMetadata) -> Result<()> {
        *self
            .osd_metadata
            .lock()
            .map_err(|_| anyhow!("OSD metadata lock poisoned"))? = Some(metadata);
        if self.osd_enabled() {
            self.refresh_osd_text()?;
        }
        Ok(())
    }

    fn refresh_osd_text(&self) -> Result<()> {
        let metadata = self
            .osd_metadata
            .lock()
            .map_err(|_| anyhow!("OSD metadata lock poisoned"))?
            .clone();
        let Some(metadata) = metadata else {
            return Ok(());
        };
        let link = match self.role {
            PlayerRole::Master => "AUTHORITY",
            PlayerRole::Slave if self.clock_healthy.load(Ordering::Acquire) => "CONNECTED",
            PlayerRole::Slave => "FREE-RUNNING",
        };
        let text = format!(
            "PLAYER {}   ROLE {}\nPROGRAM {}   CUE {:02}/{} {}\nSTATE {:?}   LINK {}\nTIME ",
            self.sequence.player_id,
            self.role,
            self.sequence.program_id,
            metadata.cue_index + 1,
            self.sequence.cue_ids.len(),
            metadata.cue_id,
            metadata.transport_state,
            link
        );
        self.osd_overlay.set_property("text", text);
        Ok(())
    }

    fn set_clock_health(&self, healthy: bool) {
        self.clock_healthy.store(healthy, Ordering::Release);
        if self.osd_enabled()
            && let Err(error) = self.refresh_osd_text()
        {
            warn!(%error, "failed to refresh OSD connection state");
        }
    }

    fn clock(&self) -> Result<gst::Clock> {
        self.clock
            .lock()
            .map(|clock| clock.clone())
            .map_err(|_| anyhow!("clock lock poisoned"))
    }
}

fn validate_snapshot_generation(snapshot: &StateSnapshot, authority: Authority) -> Result<bool> {
    if snapshot.generation < authority.generation {
        return Ok(false);
    }
    if snapshot.generation > authority.generation {
        bail!(
            "snapshot introduced generation {}; prepared rejoin required from generation {}",
            snapshot.generation,
            authority.generation
        );
    }
    Ok(true)
}

fn record_release_error(release_error: &Mutex<Option<String>>, message: String) {
    error!(error = %message, "scheduled release failed");
    if let Ok(mut release_error) = release_error.lock() {
        *release_error = Some(message);
    }
}

fn can_join_startup(release_at: ClockNs, now: ClockNs, minimum_lead_ns: u64) -> bool {
    release_at.get().saturating_sub(now.get()) >= minimum_lead_ns
}

impl Drop for Player {
    fn drop(&mut self) {
        let scheduled = self
            .scheduled_release
            .lock()
            .ok()
            .and_then(|mut scheduled| scheduled.take());
        if let Some(scheduled) = scheduled {
            scheduled.unschedule();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn media_uri(path: &Path) -> Result<String> {
    if !path.is_file() {
        bail!("media file does not exist: {}", path.display());
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    gst::glib::filename_to_uri(&canonical, None)
        .map(String::from)
        .with_context(|| format!("cannot convert {} to a file URI", canonical.display()))
}

fn make_element(factory: &str, description: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("{description} factory '{factory}' is unavailable"))
}

fn make_named_element(factory: &str, name: &str, description: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("{description} factory '{factory}' is unavailable"))
}

fn connect_decoded_pad(
    source: &gst::Element,
    queue: &gst::Element,
    pad_prefix: &'static str,
    description: &'static str,
) {
    let weak_queue = queue.downgrade();
    source.connect_pad_added(move |_, pad| {
        if !pad.name().starts_with(pad_prefix) {
            return;
        }
        let Some(queue) = weak_queue.upgrade() else {
            return;
        };
        let Some(sink_pad) = queue.static_pad("sink") else {
            error!(stream = description, "decode queue has no sink pad");
            return;
        };
        if sink_pad.is_linked() {
            debug!(stream = description, pad = %pad.name(), "ignoring additional decoded stream");
            return;
        }
        match pad.link(&sink_pad) {
            Ok(_) => debug!(stream = description, pad = %pad.name(), "decoded stream linked"),
            Err(error) => error!(stream = description, pad = %pad.name(), ?error, "failed to link decoded stream"),
        }
    });
}

fn select_decoded_streams(source: &gst::Element, selected_type: gst::StreamType) {
    source.connect("select-stream", false, move |values| {
        let selected = values
            .get(2)
            .and_then(|value| value.get::<gst::Stream>().ok())
            .is_some_and(|stream| stream.stream_type().intersects(selected_type));
        Some(i32::from(selected).to_value())
    });
}

fn optional_output_size(args: &PlaybackArgs) -> Option<(u32, u32)> {
    args.width.zip(args.height)
}

fn validate_playback_args(args: &PlaybackArgs) -> Result<()> {
    if !args.kms_gl {
        return Ok(());
    }
    if args.fit == FitMode::Cover {
        bail!("--kms-gl does not yet support --fit cover; use contain or stretch");
    }
    if args.width.is_some() || args.height.is_some() {
        bail!("--kms-gl does not yet support --width/--height canvas sizing");
    }
    if args.display.is_some() {
        bail!(
            "--kms-gl cannot be combined with --display; use GST_GL_GBM_DRM_DEVICE and GST_GL_GBM_DRM_CONNECTOR"
        );
    }
    if args.video_sink.is_some() {
        bail!("--kms-gl cannot be combined with --video-sink");
    }
    Ok(())
}

fn preroll_request_error(args: &PlaybackArgs) -> &'static str {
    if args.kms_gl {
        "--kms-gl could not initialize EGL/GBM output; verify the GStreamer GL plugins, Mesa EGL/GBM drivers, and DRM permissions"
    } else {
        "failed to request PAUSED"
    }
}

fn preroll_wait_error(args: &PlaybackArgs) -> &'static str {
    if args.kms_gl {
        "--kms-gl EGL/GBM output failed during preroll; verify the DRM device/connector and GPU driver"
    } else {
        "playback preroll failed"
    }
}

fn output_size(args: &PlaybackArgs) -> Result<(u32, u32)> {
    optional_output_size(args).ok_or_else(|| {
        anyhow!("--fit cover requires both --width and --height to define the crop aspect ratio")
    })
}

fn select_video_sink(args: &PlaybackArgs) -> Result<gst::Element> {
    if args.kms_gl {
        return make_kms_gl_sink("glimagesink");
    }
    if let Some(factory) = &args.video_sink {
        return make_element(factory, "video sink");
    }
    if let Some(display) = &args.display {
        if display.parse::<u32>().is_ok() && gst::ElementFactory::find("kmssink").is_some() {
            return make_element("kmssink", "DRM/KMS video sink");
        }
        if display.starts_with(':') && gst::ElementFactory::find("ximagesink").is_some() {
            return make_element("ximagesink", "X11 video sink");
        }
        if gst::ElementFactory::find("waylandsink").is_some() {
            return make_element("waylandsink", "Wayland video sink");
        }
    }
    if args.fullscreen
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
        && gst::ElementFactory::find("waylandsink").is_some()
    {
        return make_element("waylandsink", "Wayland video sink");
    }
    make_element("autovideosink", "automatic video sink")
}

fn make_kms_gl_sink(factory: &str) -> Result<gst::Element> {
    make_element(
        factory,
        "EGL/GBM GL video sink (install the GStreamer GL runtime plugins)",
    )
}

fn make_osd_overlay() -> Result<gst::Element> {
    gst::ElementFactory::make("timeoverlay")
        .name("diagnostic-osd")
        .property("silent", true)
        .property("auto-resize", false)
        .property("font-desc", "Sans Bold 32")
        .property("shaded-background", true)
        .property("shading-value", 120_u32)
        .property("draw-outline", true)
        .property("draw-shadow", true)
        .property_from_str("halignment", "center")
        .property_from_str("valignment", "center")
        .property_from_str("line-alignment", "center")
        .property_from_str("time-mode", "stream-time")
        .build()
        .context("diagnostic OSD requires the GStreamer timeoverlay element")
}

fn build_playbin_video_sink(args: &PlaybackArgs) -> Result<(gst::Element, gst::Element)> {
    let mut video_chain = Vec::new();
    if !args.kms_gl {
        let video_convert =
            make_named_element("videoconvert", "embedded-video-convert", "video converter")?;
        let video_scale = gst::ElementFactory::make("videoscale")
            .name("embedded-video-scale")
            .property("add-borders", args.fit == FitMode::Contain)
            .build()
            .context("videoscale is unavailable")?;
        video_chain.extend([video_convert, video_scale]);
    }
    if !args.kms_gl && args.fit == FitMode::Cover {
        let (width, height) = output_size(args)?;
        let width = i32::try_from(width).context("output width exceeds GStreamer limits")?;
        let height = i32::try_from(height).context("output height exceeds GStreamer limits")?;
        let crop = gst::ElementFactory::make("aspectratiocrop")
            .name("embedded-video-cover-crop")
            .property("aspect-ratio", gst::Fraction::new(width, height))
            .build()
            .context("cover mode requires the aspectratiocrop GStreamer element")?;
        video_chain.push(crop);
    }
    if !args.kms_gl
        && let Some((width, height)) = optional_output_size(args)
    {
        let width = i32::try_from(width).context("output width exceeds GStreamer limits")?;
        let height = i32::try_from(height).context("output height exceeds GStreamer limits")?;
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", width)
            .field("height", height)
            .build();
        let caps_filter = gst::ElementFactory::make("capsfilter")
            .name("embedded-video-output-size")
            .property("caps", &caps)
            .build()
            .context("capsfilter is unavailable")?;
        video_chain.push(caps_filter);
    }
    let osd_overlay = make_osd_overlay()?;
    video_chain.push(osd_overlay.clone());
    let video_sink = select_video_sink(args)?;
    force_clock_sync(&video_sink, "video sink")?;
    configure_video_sink(&video_sink, args)?;
    video_chain.push(video_sink);

    let bin = gst::Bin::with_name("embedded-video-output");
    bin.add_many(&video_chain)?;
    gst::Element::link_many(&video_chain)?;
    let sink_pad = video_chain[0]
        .static_pad("sink")
        .or_else(|| video_chain[0].static_pad("video_sink"))
        .context("video output bin's first element has no video sink pad")?;
    let ghost_pad = gst::GhostPad::builder_with_target(&sink_pad)?
        .name("sink")
        .build();
    bin.add_pad(&ghost_pad)?;
    Ok((bin.upcast(), osd_overlay))
}

fn configure_video_sink(sink: &gst::Element, args: &PlaybackArgs) -> Result<()> {
    if sink.find_property("force-aspect-ratio").is_some() {
        sink.set_property("force-aspect-ratio", args.fit == FitMode::Contain);
    }
    if args.fullscreen && !args.kms_gl {
        if sink.find_property("fullscreen").is_none() {
            bail!(
                "video sink '{}' cannot be made fullscreen directly; select a fullscreen-capable sink such as waylandsink",
                sink.name()
            );
        }
        sink.set_property("fullscreen", true);
    }
    if let Some(display) = &args.display {
        if sink.find_property("display").is_some() {
            sink.set_property("display", display);
        } else if sink.find_property("connector-id").is_some() {
            let connector = display.parse::<u32>().with_context(|| {
                format!("DRM/KMS display identifier '{display}' is not a connector number")
            })?;
            sink.set_property("connector-id", connector);
        } else {
            bail!(
                "video sink '{}' does not expose a supported display-selection property",
                sink.name()
            );
        }
    }
    Ok(())
}

fn configure_kms_gl_context(pipeline: &gst::Pipeline, args: &PlaybackArgs) -> Result<()> {
    if !args.kms_gl {
        return Ok(());
    }
    let display = gst_gl::GLDisplay::with_type(gst_gl::GLDisplayType::GBM).context(
        "--kms-gl requires GStreamer GL built with Mesa GBM support and an accessible DRM device",
    )?;
    let mut context = gst::Context::new(gst_gl::GL_DISPLAY_CONTEXT_TYPE, true);
    context
        .get_mut()
        .expect("a newly-created GStreamer context is writable")
        .set_gl_display(Some(&display));
    pipeline.set_context(&context);
    info!(
        compositor = "gpu-egl-gbm",
        sink = "glimagesink",
        fullscreen = true,
        "selected video compositor"
    );
    Ok(())
}

fn log_and_validate_video_negotiation(
    osd_overlay: &gst::Element,
    args: &PlaybackArgs,
) -> Result<()> {
    let caps = osd_overlay
        .static_pad("src")
        .and_then(|pad| pad.current_caps());
    let features: Vec<String> = caps
        .as_ref()
        .map(|caps| {
            caps.iter_with_features()
                .flat_map(|(_, features)| features.iter().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default();
    info!(
        compositor = if args.kms_gl { "gpu-egl-gbm" } else { "inline" },
        caps = caps.as_ref().map(ToString::to_string),
        memory_features = ?features,
        "negotiated video output"
    );
    if !args.kms_gl {
        return Ok(());
    }
    let has_dmabuf = features.iter().any(|feature| feature == "memory:DMABuf");
    let has_overlay = features
        .iter()
        .any(|feature| feature == "meta:GstVideoOverlayComposition");
    if !has_dmabuf || !has_overlay {
        bail!(
            "--kms-gl requires negotiated DMA-BUF video with overlay-composition metadata; negotiated caps were {}",
            caps.as_ref()
                .map_or_else(|| "unavailable".to_owned(), ToString::to_string)
        );
    }
    Ok(())
}

fn configure_decoder_policy(policy: HardwareDecoding) {
    if policy == HardwareDecoding::Auto {
        info!(
            hwdec = "auto",
            "GStreamer decoder ranking will select hardware acceleration when available"
        );
        return;
    }
    let decoder_type = gst::ElementFactoryType::DECODER | gst::ElementFactoryType::MEDIA_VIDEO;
    let mut disabled = Vec::new();
    for factory in gst::ElementFactory::factories_with_type(decoder_type, gst::Rank::MARGINAL) {
        let is_hardware = factory
            .metadata(gst::ELEMENT_METADATA_KLASS)
            .is_some_and(|klass| klass.split('/').any(|part| part == "Hardware"));
        if is_hardware {
            disabled.push(factory.name().to_string());
            factory.set_rank(gst::Rank::NONE);
        }
    }
    info!(hwdec = "off", decoders = ?disabled, "hardware video decoders disabled by rank");
}

fn log_selected_pipeline_elements(pipeline: &gst::Pipeline) {
    for result in pipeline.iterate_recurse() {
        let Ok(element) = result else {
            continue;
        };
        let Some(factory) = element.factory() else {
            continue;
        };
        let Some(klass) = factory.metadata(gst::ELEMENT_METADATA_KLASS) else {
            continue;
        };
        let is_decoder = klass.split('/').any(|part| part == "Decoder");
        let is_sink = klass.split('/').any(|part| part == "Sink");
        if is_decoder || is_sink {
            info!(
                element = %element.name(),
                factory = %factory.name(),
                class = klass,
                hardware = klass.split('/').any(|part| part == "Hardware"),
                "selected playback element"
            );
        }
    }
}

fn select_playback_clock(
    pipeline: &gst::Pipeline,
    forced_clock: Option<&gst::Clock>,
    policy: ClockPolicy,
    cue_has_audio: bool,
) -> Result<gst::Clock> {
    if let Some(clock) = forced_clock {
        return Ok(clock.clone());
    }
    if policy == ClockPolicy::System {
        return Ok(gst::SystemClock::obtain().upcast());
    }
    if policy == ClockPolicy::Audio && !cue_has_audio {
        bail!(
            "--clock audio requires the first cue to contain an embedded or external audio stream"
        );
    }
    match pipeline.provide_clock() {
        Some(clock) => Ok(clock),
        None if policy == ClockPolicy::Audio => {
            bail!("--clock audio was requested, but the selected audio sink provides no clock")
        }
        None => {
            warn!("no media clock is available; falling back to the system clock");
            Ok(gst::SystemClock::obtain().upcast())
        }
    }
}

fn combined_gain(cli_gain_db: f64, cue_gain_db: f64) -> Result<f64> {
    gain_db_to_linear(cli_gain_db + cue_gain_db)
}

fn gain_db_to_linear(gain_db: f64) -> Result<f64> {
    if !gain_db.is_finite() {
        bail!("gain must be finite");
    }
    let linear = 10_f64.powf(gain_db / 20.0);
    if !linear.is_finite() || linear > 10.0 {
        bail!("combined gain {gain_db:+.2} dB exceeds the supported maximum of +20 dB");
    }
    Ok(linear)
}

fn format_ns_as_milliseconds(value: u64) -> String {
    format!("{}.{:03}", value / 1_000_000, value % 1_000_000 / 1_000)
}

fn force_clock_sync(element: &gst::Element, description: &str) -> Result<()> {
    if element.find_property("sync").is_none() {
        bail!(
            "{description} '{}' has no clock-sync property",
            element.name()
        );
    }
    element.set_property("sync", true);
    Ok(())
}

fn configure_external_clock_audio_slaving(pipeline: &gst::Pipeline) -> Result<()> {
    let mut configured = 0_u32;
    for result in pipeline.iterate_recurse() {
        let element = result.context("pipeline changed while locating the audio sink")?;
        let Some(klass) = element.metadata(gst::ELEMENT_METADATA_KLASS) else {
            continue;
        };
        let is_audio_sink = klass.split('/').any(|part| part == "Audio")
            && klass.split('/').any(|part| part == "Sink");
        if !is_audio_sink || element.find_property("slave-method").is_none() {
            continue;
        }

        // AudioBaseSink's default `skew` mode corrects drift by moving the
        // playout pointer. That can produce a visible pipeline stall after a
        // long preroll. Resampling instead makes the hardware clock follow an
        // externally supplied pipeline clock continuously.
        element.set_property_from_str("slave-method", "resample");
        configured += 1;
        debug!(
            element = %element.name(),
            factory_class = klass,
            "configured audio sink for external clock resampling"
        );
    }
    if configured == 0 {
        debug!("no resampling-capable audio sink found during preroll");
    }
    Ok(())
}

fn configure_stream(stream: &TcpStream) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
}

fn new_master_session() -> Result<MasterSession> {
    let uuid = gst::glib::uuid_string_random();
    uuid.chars()
        .filter(|character| *character != '-')
        .collect::<String>()
        .parse()
        .context("GLib generated an invalid Master session UUID")
}

fn to_clock_time(duration: Duration) -> Result<gst::ClockTime> {
    let nanos =
        u64::try_from(duration.as_nanos()).context("duration is too large for GStreamer")?;
    Ok(gst::ClockTime::from_nseconds(nanos))
}

fn wait_for_state(
    pipeline: &gst::Pipeline,
    timeout: Duration,
    context: &'static str,
) -> Result<()> {
    let (result, current, pending) = pipeline.state(Some(to_clock_time(timeout)?));
    result.with_context(|| format!("{context}; current={current:?}, pending={pending:?}"))?;
    Ok(())
}

fn positive_seconds(value: &str) -> Result<f64, String> {
    let seconds: f64 = value.parse().map_err(|_| "must be a number".to_owned())?;
    if seconds.is_finite() && seconds > 0.0 {
        Ok(seconds)
    } else {
        Err("must be a finite positive number".to_owned())
    }
}

fn finite_f64(value: &str) -> Result<f64, String> {
    let number: f64 = value.parse().map_err(|_| "must be a number".to_owned())?;
    if number.is_finite() {
        Ok(number)
    } else {
        Err("must be finite".to_owned())
    }
}

fn non_negative_seconds(value: &str) -> Result<f64, String> {
    let seconds: f64 = value.parse().map_err(|_| "must be a number".to_owned())?;
    if seconds.is_finite() && seconds >= 0.0 {
        Ok(seconds)
    } else {
        Err("must be a finite non-negative number".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Authority, Cli, ClockNs, ClockPolicy, Command, DRIFT_RECOVERY_COOLDOWN, DriftAction,
        DriftPolicy, DriftSlewAction, DriftSlewController, DurationMismatchPolicy, FitMode,
        HardwareDecoding, MasterSession, MediaItem, MediaPositionNs, Operation, PlaybackSequence,
        ProgramCompatibility, StateSnapshot, Timeline, TransportState, build_playbin_video_sink,
        can_join_startup, combined_gain, finite_f64, gain_db_to_linear, make_kms_gl_sink,
        non_negative_seconds, recovery_attempt_is_active, select_video_sink,
        validate_playback_args, validate_snapshot_generation, validate_state_datagram,
        validate_subscription,
    };
    use clap::Parser;
    use gst::prelude::*;
    use gstreamer as gst;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use syncplayer::program::ProgramHash;
    use syncplayer::protocol::{Capabilities, StateDatagram, Subscription};

    fn authority() -> Authority {
        Authority {
            session: MasterSession::new(1),
            generation: 7,
        }
    }

    fn state_packet(session: MasterSession, sequence: u64) -> StateDatagram {
        StateDatagram {
            session,
            sequence,
            master_clock: ClockNs::new(123),
            state: snapshot(7),
        }
    }

    fn snapshot(generation: u64) -> StateSnapshot {
        StateSnapshot {
            generation,
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            transport_state: TransportState::Playing,
            operation: Operation::Start,
            media_position: MediaPositionNs::new(0),
            anchor_clock: ClockNs::new(100),
            item_index: 0,
            cue_id: "first".to_owned(),
            cue_occurrence: 0,
            item_position: MediaPositionNs::new(0),
            cue_duration: MediaPositionNs::new(1_000),
            playlist_iteration: 0,
            debug_loop: None,
            osd_enabled: false,
        }
    }

    fn compatibility(policy: DurationMismatchPolicy) -> ProgramCompatibility {
        ProgramCompatibility {
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            master_player_id: "master".to_owned(),
            cue_durations_ns: vec![1_000, 2_000],
            duration_tolerance_ns: 100,
            mismatch_policy: policy,
        }
    }

    fn subscription() -> Subscription {
        Subscription {
            player_id: "left".to_owned(),
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            software_version: "0.1.0".to_owned(),
            capabilities: Capabilities::current(),
            state_port: 46_002,
            cue_durations_ns: vec![1_050, 2_000],
        }
    }

    #[test]
    fn command_line_requires_a_program_and_supports_player_identity() {
        assert!(Cli::try_parse_from(["syncplayer", "master"]).is_err());
        let cli = Cli::try_parse_from(["syncplayer", "master", "show.toml", "--player-id", "left"])
            .unwrap();
        assert!(matches!(cli.command, Command::Master(_)));
    }

    #[test]
    fn command_line_supports_validate_and_rejects_prototype_inputs() {
        assert!(Cli::try_parse_from(["syncplayer", "validate", "show.toml"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "syncplayer",
                "master",
                "show.toml",
                "--state-bind",
                "127.0.0.1:47002",
                "--state-rate-hz",
                "20",
                "--keepalive-timeout",
                "12",
                "--console-bind",
                "127.0.0.1:2323",
                "--diagnostics-log",
                "diagnostics.csv",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "syncplayer",
                "slave",
                "show.toml",
                "--master",
                "127.0.0.1:46000",
                "--state-bind",
                "127.0.0.1:47003",
                "--keepalive-interval",
                "3",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["syncplayer", "master", "show.toml", "--state-rate-hz", "0",])
                .is_err()
        );
        assert!(Cli::try_parse_from(["syncplayer", "master", "--media", "a.mp4"]).is_err());
        assert!(Cli::try_parse_from(["syncplayer", "master", "show.toml", "--loop"]).is_err());
        assert!(
            Cli::try_parse_from(["syncplayer", "master", "show.toml", "--drift-slew"]).is_err()
        );
        let cli = Cli::try_parse_from([
            "syncplayer",
            "slave",
            "show.toml",
            "--master",
            "127.0.0.1:46000",
            "--drift-slew",
        ])
        .unwrap();
        let Command::Slave(args) = cli.command else {
            panic!("expected Slave command");
        };
        assert!(args.drift_slew);
    }

    #[test]
    fn command_line_accepts_local_output_controls() {
        let cli = Cli::try_parse_from([
            "syncplayer",
            "master",
            "show.toml",
            "--gain-db",
            "+3",
            "--fullscreen",
            "--width",
            "1920",
            "--height",
            "1080",
            "--display",
            "HDMI-A-1",
            "--fit",
            "cover",
            "--clock",
            "audio",
            "--hwdec",
            "off",
        ])
        .unwrap();
        let Command::Master(args) = cli.command else {
            panic!("expected Master command");
        };
        assert_eq!(args.playback.gain_db, 3.0);
        assert!(args.playback.fullscreen);
        assert_eq!(args.playback.fit, FitMode::Cover);
        assert_eq!(args.playback.clock, ClockPolicy::Audio);
        assert_eq!(args.playback.hwdec, HardwareDecoding::Off);
        assert!(
            Cli::try_parse_from(["syncplayer", "master", "show.toml", "--width", "1920"]).is_err()
        );
    }

    #[test]
    fn command_line_accepts_kms_gl_and_rejects_ambiguous_sink_selection() {
        let cli = Cli::try_parse_from([
            "syncplayer",
            "master",
            "show.toml",
            "--kms-gl",
            "--fullscreen",
            "--fit",
            "stretch",
        ])
        .unwrap();
        let Command::Master(args) = cli.command else {
            panic!("expected Master command");
        };
        assert!(args.playback.kms_gl);
        assert!(validate_playback_args(&args.playback).is_ok());
        assert!(
            Cli::try_parse_from([
                "syncplayer",
                "master",
                "show.toml",
                "--kms-gl",
                "--video-sink",
                "fakesink",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "syncplayer",
                "master",
                "show.toml",
                "--kms-gl",
                "--display",
                "42",
            ])
            .is_err()
        );
    }

    #[test]
    fn kms_gl_rejects_software_scaling_modes_with_actionable_errors() {
        let cli = Cli::try_parse_from([
            "syncplayer",
            "master",
            "show.toml",
            "--kms-gl",
            "--fit",
            "cover",
        ])
        .unwrap();
        let Command::Master(args) = cli.command else {
            panic!("expected Master command");
        };
        assert!(
            validate_playback_args(&args.playback)
                .unwrap_err()
                .to_string()
                .contains("does not yet support --fit cover")
        );

        let cli = Cli::try_parse_from([
            "syncplayer",
            "slave",
            "show.toml",
            "--master",
            "127.0.0.1:46000",
            "--kms-gl",
            "--width",
            "1920",
            "--height",
            "1080",
        ])
        .unwrap();
        let Command::Slave(args) = cli.command else {
            panic!("expected Slave command");
        };
        assert!(
            validate_playback_args(&args.playback)
                .unwrap_err()
                .to_string()
                .contains("does not yet support --width/--height")
        );
    }

    #[test]
    fn kms_gl_output_bin_uses_gl_sink_without_software_video_transforms() {
        gst::init().unwrap();
        let missing = make_kms_gl_sink("syncplayer-intentionally-missing-gl-sink")
            .unwrap_err()
            .to_string();
        assert!(missing.contains("install the GStreamer GL runtime plugins"));
        let cli = Cli::try_parse_from(["syncplayer", "master", "show.toml", "--kms-gl"]).unwrap();
        let Command::Master(args) = cli.command else {
            panic!("expected Master command");
        };
        if gst::ElementFactory::find("glimagesink").is_none() {
            let error = select_video_sink(&args.playback).unwrap_err().to_string();
            assert!(error.contains("install the GStreamer GL runtime plugins"));
            return;
        }
        let (output, _) = build_playbin_video_sink(&args.playback).unwrap();
        let output = output.downcast::<gst::Bin>().unwrap();
        let factories: Vec<String> = output
            .iterate_recurse()
            .into_iter()
            .filter_map(Result::ok)
            .filter_map(|element| element.factory())
            .map(|factory| factory.name().to_string())
            .collect();
        assert!(factories.iter().any(|factory| factory == "glimagesink"));
        assert!(!factories.iter().any(|factory| factory == "videoconvert"));
        assert!(!factories.iter().any(|factory| factory == "videoscale"));
    }

    #[test]
    fn decibel_gain_is_composed_in_linear_space() {
        assert!((combined_gain(-6.0, 6.0).unwrap() - 1.0).abs() < f64::EPSILON);
        assert!((gain_db_to_linear(20.0).unwrap() - 10.0).abs() < f64::EPSILON);
        assert!(gain_db_to_linear(20.1).is_err());
        assert!(finite_f64("NaN").is_err());
    }

    #[test]
    fn subscription_rejects_identity_program_and_capability_conflicts() {
        let expected = compatibility(DurationMismatchPolicy::Reject);
        let mut candidate = subscription();
        assert!(validate_subscription(&candidate, &expected).is_ok());
        candidate.player_id = "master".to_owned();
        assert!(validate_subscription(&candidate, &expected).is_err());
        candidate = subscription();
        candidate.program_hash = ProgramHash::from_bytes([2; 32]);
        assert!(validate_subscription(&candidate, &expected).is_err());
        candidate = subscription();
        candidate.capabilities = Capabilities::from_bits(0);
        assert!(validate_subscription(&candidate, &expected).is_err());
    }

    #[test]
    fn duration_mismatch_policy_warns_or_rejects() {
        let mut candidate = subscription();
        candidate.cue_durations_ns[0] = 1_101;
        assert!(
            validate_subscription(&candidate, &compatibility(DurationMismatchPolicy::Reject))
                .is_err()
        );
        let differences =
            validate_subscription(&candidate, &compatibility(DurationMismatchPolicy::Warn))
                .unwrap();
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].delta_ns, 101);
    }

    #[test]
    fn startup_delay_accepts_zero_and_positive_finite_values() {
        assert_eq!(non_negative_seconds("0"), Ok(0.0));
        assert_eq!(non_negative_seconds("30"), Ok(30.0));
        assert!(non_negative_seconds("-1").is_err());
        assert!(non_negative_seconds("inf").is_err());
    }

    #[test]
    fn startup_join_requires_the_release_preparation_window() {
        let release_at = ClockNs::new(30_000);
        assert!(can_join_startup(release_at, ClockNs::new(20_000), 5_000));
        assert!(can_join_startup(release_at, ClockNs::new(25_000), 5_000));
        assert!(!can_join_startup(release_at, ClockNs::new(25_001), 5_000));
    }

    #[test]
    fn state_validation_rejects_wrong_session_and_stale_sequence() {
        let authority = authority();
        assert!(
            validate_state_datagram(&state_packet(authority.session, 2), authority, Some(1))
                .unwrap()
        );
        assert!(
            !validate_state_datagram(&state_packet(authority.session, 1), authority, Some(1))
                .unwrap()
        );
        assert!(
            !validate_state_datagram(&state_packet(MasterSession::new(2), 2), authority, Some(1))
                .unwrap()
        );
        let mut old_generation = state_packet(authority.session, 3);
        old_generation.state.generation = authority.generation - 1;
        assert!(!validate_state_datagram(&old_generation, authority, None).unwrap());
        let mut future_generation = state_packet(authority.session, 3);
        future_generation.state.generation = authority.generation + 1;
        assert!(validate_state_datagram(&future_generation, authority, None).is_err());
    }

    #[test]
    fn snapshots_require_the_current_generation() {
        let authority = authority();
        assert!(validate_snapshot_generation(&snapshot(7), authority).unwrap());
        assert!(!validate_snapshot_generation(&snapshot(6), authority).unwrap());
        assert!(validate_snapshot_generation(&snapshot(8), authority).is_err());
    }

    #[test]
    fn playing_snapshot_anchor_cannot_be_in_the_future() {
        let authority = authority();
        let message = StateDatagram {
            session: authority.session,
            sequence: 1,
            master_clock: ClockNs::new(99),
            state: snapshot(authority.generation),
        };
        assert!(validate_state_datagram(&message, authority, None).is_err());
    }

    #[test]
    fn drift_policy_uses_hysteresis_and_requires_sustained_error() {
        let now = Instant::now();
        let mut policy = DriftPolicy::default();
        assert_eq!(policy.observe(now, 300_000_000), DriftAction::None);
        assert_eq!(policy.observe(now, -300_000_000), DriftAction::None);
        assert_eq!(
            policy.observe(now, 300_000_000),
            DriftAction::Repair { consecutive: 3 }
        );
        assert!(matches!(
            policy.observe(now + Duration::from_secs(1), 300_000_000),
            DriftAction::Cooldown { .. }
        ));

        let after_cooldown = now + DRIFT_RECOVERY_COOLDOWN + Duration::from_nanos(1);
        assert_eq!(
            policy.observe(after_cooldown, 150_000_000),
            DriftAction::None
        );
        assert_eq!(
            policy.observe(after_cooldown, 150_000_000),
            DriftAction::None
        );
        assert_eq!(
            policy.observe(after_cooldown, 150_000_000),
            DriftAction::Warn { consecutive: 3 }
        );
    }

    #[test]
    fn drift_policy_clears_on_small_or_suppressed_samples() {
        let now = Instant::now();
        let mut policy = DriftPolicy::default();
        assert_eq!(policy.observe(now, 300_000_000), DriftAction::None);
        assert_eq!(policy.observe(now, 100_000_000), DriftAction::None);
        assert_eq!(policy.observe(now, 300_000_000), DriftAction::None);
        policy.suppress();
        assert_eq!(policy.observe(now, 300_000_000), DriftAction::None);
        assert_eq!(policy.observe(now, 300_000_000), DriftAction::None);
        assert_eq!(
            policy.observe(now, 300_000_000),
            DriftAction::Repair { consecutive: 3 }
        );
    }

    #[test]
    fn drift_slew_requires_consistent_error_and_converges_at_two_percent_or_less() {
        let mut slew = DriftSlewController::new(true);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::Apply(1.015));
        assert_eq!(slew.observe(-240_000_000), DriftSlewAction::Apply(1.02));
        assert_eq!(slew.observe(-80_000_000), DriftSlewAction::Apply(1.008));
        assert_eq!(slew.observe(-40_000_000), DriftSlewAction::Reset);
    }

    #[test]
    fn drift_slew_ignores_disabled_noisy_and_repair_sized_errors() {
        let mut disabled = DriftSlewController::new(false);
        assert_eq!(disabled.observe(150_000_000), DriftSlewAction::None);

        let mut slew = DriftSlewController::new(true);
        assert_eq!(slew.observe(150_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(300_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
    }

    #[test]
    fn drift_slew_defers_transition_reset_until_observation_resumes() {
        let mut slew = DriftSlewController::new(true);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::Apply(1.015));
        assert!(slew.suppress());
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::Reset);
        assert_eq!(slew.observe(-150_000_000), DriftSlewAction::None);
    }

    #[test]
    fn obsolete_recovery_attempt_cannot_publish_release() {
        let active = AtomicU64::new(4);
        assert!(recovery_attempt_is_active(&active, 4));
        active.store(5, Ordering::Release);
        assert!(!recovery_attempt_is_active(&active, 4));
    }

    fn sequence(looping: bool) -> PlaybackSequence {
        PlaybackSequence {
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            player_id: "player".to_owned(),
            cue_ids: vec!["a".to_owned(), "b".to_owned()],
            event_cues: Vec::new(),
            items: vec![
                MediaItem {
                    path: PathBuf::from("a"),
                    uri: "file:///a".to_owned(),
                    audio_uri: "file:///a".to_owned(),
                    external_audio: false,
                    has_audio: true,
                    mute: false,
                    gain_db: 0.0,
                },
                MediaItem {
                    path: PathBuf::from("b"),
                    uri: "file:///b".to_owned(),
                    audio_uri: "file:///b".to_owned(),
                    external_audio: false,
                    has_audio: true,
                    mute: false,
                    gain_db: 0.0,
                },
            ],
            cue_durations_ns: vec![10, 20],
            timeline: Some(Timeline::new(vec![10, 20], looping).unwrap()),
            looping,
            from_playlist: true,
        }
    }

    #[test]
    fn sequence_error_is_correct_across_item_and_loop_boundaries() {
        assert_eq!(
            sequence(false).position_error(MediaPositionNs::new(11), MediaPositionNs::new(9)),
            2
        );
        assert!(
            crate::make_event_runtime(&sequence(false))
                .unwrap()
                .is_none()
        );
        let looping = sequence(true);
        assert_eq!(
            looping.position_error(MediaPositionNs::new(1), MediaPositionNs::new(29)),
            2
        );
        assert_eq!(
            looping.cue_identity(31).unwrap(),
            (
                syncplayer::playlist::ItemPosition {
                    index: 0,
                    local_ns: 1,
                },
                1,
            )
        );
        assert_eq!(
            looping
                .sequence_position_near(
                    syncplayer::playlist::ItemPosition {
                        index: 0,
                        local_ns: 1,
                    },
                    MediaPositionNs::new(29),
                )
                .unwrap(),
            MediaPositionNs::new(31)
        );
    }

    #[test]
    fn shorter_rendition_holds_its_last_in_cue_instant() {
        let sequence = sequence(true);
        assert_eq!(
            sequence
                .position_for_remote_snapshot(
                    syncplayer::playlist::ItemPosition {
                        index: 1,
                        local_ns: 25,
                    },
                    1,
                )
                .unwrap(),
            MediaPositionNs::new(59)
        );
    }

    #[test]
    fn authoritative_cue_maps_through_local_playlist_durations() {
        let sequence = PlaybackSequence {
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            player_id: "player".to_owned(),
            cue_ids: vec!["a".to_owned(), "b".to_owned()],
            event_cues: Vec::new(),
            items: vec![
                MediaItem {
                    path: PathBuf::from("a"),
                    uri: "file:///a".to_owned(),
                    audio_uri: "file:///a".to_owned(),
                    external_audio: false,
                    has_audio: true,
                    mute: false,
                    gain_db: 0.0,
                },
                MediaItem {
                    path: PathBuf::from("b"),
                    uri: "file:///b".to_owned(),
                    audio_uri: "file:///b".to_owned(),
                    external_audio: false,
                    has_audio: true,
                    mute: false,
                    gain_db: 0.0,
                },
            ],
            cue_durations_ns: vec![2, 27],
            timeline: Some(Timeline::new(vec![2, 27], true).unwrap()),
            looping: true,
            from_playlist: true,
        };

        // Master position 181 on a [10, 20] timeline is iteration 6,
        // item 0, local position 1. Interpreting 181 against this host's
        // durations incorrectly selects item 1 in the same iteration.
        assert_eq!(sequence.cue_identity(181).unwrap().0.index, 1);
        let translated = sequence
            .position_for_cue(
                syncplayer::playlist::ItemPosition {
                    index: 0,
                    local_ns: 1,
                },
                6,
            )
            .unwrap();
        assert_eq!(translated, MediaPositionNs::new(175));
        assert_eq!(
            sequence.cue_identity(translated.get()).unwrap(),
            (
                syncplayer::playlist::ItemPosition {
                    index: 0,
                    local_ns: 1,
                },
                6,
            )
        );
    }
}
