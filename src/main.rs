use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_net as gst_net;
use gstreamer_pbutils as gst_pbutils;
use std::io::Read;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use syncplayer::playlist::{ItemPosition, PlaylistCursor, Timeline};
use syncplayer::program::{ProgramConfig, ProgramHash, ResolvedProgram};
use syncplayer::protocol::{
    Capabilities, DebugLoopState, MAX_LINE_BYTES, MAX_SUBSCRIPTION_CUES, MasterSession, Message,
    Operation, ReleasePlan, StateSnapshot, Subscription, TransportState, read_message,
    write_message,
};
use syncplayer::subscriber::{ParticipantLease, ParticipantRegistry, StateEndpoint};
use syncplayer::time::{ClockNs, MediaPositionNs};
use syncplayer::transport::{EosDecision, RuntimeState, TransportController};
use syncplayer::validation::{DiagnosticLevel, GstMediaProbe, validate_program};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

const PREROLL_TIMEOUT: Duration = Duration::from_secs(30);
const CLOCK_SYNC_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MASTER_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(8);
const REJOIN_PREPARATION_TIMEOUT: Duration = Duration::from_secs(95);
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(1);
const DRIFT_WARN_THRESHOLD_NS: u128 = 100_000_000;
const DRIFT_REPAIR_THRESHOLD_NS: u128 = 250_000_000;
const DRIFT_REQUIRED_SAMPLES: u32 = 3;
const DRIFT_RECOVERY_COOLDOWN: Duration = Duration::from_secs(10);
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

    /// Override `GStreamer`'s automatically selected video sink.
    #[arg(long)]
    video_sink: Option<String>,

    /// Override `GStreamer`'s automatically selected audio sink.
    #[arg(long)]
    audio_sink: Option<String>,
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

fn run_master(args: &MasterArgs) -> Result<()> {
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
    let player = Arc::new(Player::new(&args.playback, sequence, None)?);
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
    let clock_port = provider
        .port()
        .try_into()
        .context("invalid provider port")?;
    let startup_delay = Duration::from_secs_f64(args.startup_delay);
    let release_delay = Duration::from_secs_f64(args.release_delay);
    let session = new_master_session()?;
    let initial_plan = player.schedule_start(startup_delay)?;
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
            clock_port,
            master_session = %session,
            transport_generation = generation,
            "Master playback started; ready to welcome Slaves"
        );
    } else {
        info!(
            control = %listener.local_addr()?,
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
        startup_plan: initial_plan,
        transport: Arc::new(Mutex::new(transport)),
        compatibility,
        registry: ParticipantRegistry::new(),
    });
    serve_slaves(listener, server.clone());
    player.monitor(initial_plan, Some(&server.transport))
}

fn run_slave(args: &SlaveArgs) -> Result<()> {
    let program = load_runtime_program(&args.playback)?;
    let sequence = PlaybackSequence::prepare(&program)?;
    let subscription = create_subscription(&program, &sequence)?;
    let (mut stream, clock, authority) = wait_for_master(args.master, &subscription);
    let player = Arc::new(Player::new(&args.playback, sequence, Some(&clock))?);
    let plan = receive_release(&mut stream, &clock, authority)?;
    player.resynchronize(&clock, plan, "initial-join")?;
    report_prepared(&mut stream, authority)?;
    info!(
        release_at_ns = plan.release_at.get(),
        "accepted shared-clock release"
    );

    monitor_master(stream, args.master, authority, player.clone(), subscription);
    player.monitor(plan, None)
}

fn create_subscription(
    program: &ResolvedProgram,
    sequence: &PlaybackSequence,
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
        state_port: 0,
        cue_durations_ns: sequence.cue_durations_ns.clone(),
    })
}

fn load_runtime_program(args: &PlaybackArgs) -> Result<ResolvedProgram> {
    let config = ProgramConfig::load(&args.program)
        .with_context(|| format!("failed to load program {}", args.program.display()))?;
    let program = config.resolve_player(args.player_id.as_deref())?;
    program.ensure_runtime_supported()?;
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

struct SlaveServer {
    player: Arc<Player>,
    clock: gst::Clock,
    clock_port: u16,
    release_delay: Duration,
    startup_plan: ReleasePlan,
    transport: Arc<Mutex<TransportController>>,
    compatibility: ProgramCompatibility,
    registry: Arc<ParticipantRegistry>,
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

fn welcome_slave(mut stream: TcpStream, peer: SocketAddr, server: &SlaveServer) -> Result<()> {
    configure_stream(&stream)?;
    let authority = server.authority()?;
    let (subscription, lease) = accept_subscription(&mut stream, peer, server, authority)?;
    match read_message(&mut stream).context("failed to read Slave readiness")? {
        Message::Ready => {
            lease.mark_clock_synchronized()?;
            info!(player = %subscription.player_id, %peer, "Slave clock is synchronized and ready to rejoin");
            log_registry(&server.registry)?;
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
            info!(
                player = %subscription.player_id,
                %peer,
                master_session = %session,
                transport_generation = generation,
                "Slave prepared release; starting liveness monitoring"
            );
            log_registry(&server.registry)?;
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

    let result = run_master_heartbeats(&mut stream, peer, server, authority, &lease);
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
    let lease =
        match server
            .registry
            .register(&subscription, peer, Instant::now(), LIVENESS_TIMEOUT)
        {
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
    log_registry(&server.registry)?;
    write_message(
        stream,
        &Message::Subscribed {
            session: authority.session,
            generation: authority.generation,
            clock_port: server.clock_port,
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
    let now = ClockNs::new(server.clock.time().nseconds());
    let minimum_lead_ns = u64::try_from(server.release_delay.as_nanos())
        .context("release delay is too large for the clock")?;
    let plan = if server.startup_plan.generation == authority.generation
        && can_join_startup(server.startup_plan.release_at, now, minimum_lead_ns)
    {
        info!(%peer, "Slave is ready for the shared startup deadline");
        server.startup_plan
    } else {
        let release_at = now
            .checked_add(server.release_delay)
            .ok_or_else(|| anyhow!("release timestamp overflow"))?;
        server.player.join_plan(authority.generation, release_at)?
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

fn log_registry(registry: &ParticipantRegistry) -> Result<()> {
    for participant in registry.snapshot(Instant::now(), LIVENESS_TIMEOUT)? {
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

fn run_master_heartbeats(
    stream: &mut TcpStream,
    peer: SocketAddr,
    server: &SlaveServer,
    authority: Authority,
    lease: &ParticipantLease,
) -> Result<()> {
    stream.set_read_timeout(Some(LIVENESS_TIMEOUT))?;
    stream.set_write_timeout(Some(LIVENESS_TIMEOUT))?;
    let mut sequence = 0_u64;
    loop {
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| anyhow!("heartbeat sequence overflow"))?;
        let sent_at = Instant::now();
        let master_clock = ClockNs::new(server.clock.time().nseconds());
        let runtime = server
            .transport
            .lock()
            .map_err(|_| anyhow!("transport lock poisoned"))?
            .snapshot(master_clock);
        let state = server.player.authoritative_snapshot(runtime)?;
        write_message(
            stream,
            &Message::Heartbeat {
                session: authority.session,
                sequence,
                master_clock,
                state,
            },
        )
        .context("failed to send heartbeat")?;
        match read_control_message_until(stream, sent_at + LIVENESS_TIMEOUT)
            .context("failed to read heartbeat reply")?
        {
            Message::Alive {
                session,
                sequence: reply_sequence,
            } if session == authority.session && reply_sequence == sequence => {
                lease.mark_seen()?;
            }
            Message::Alive {
                session,
                sequence: reply_sequence,
            } => bail!(
                "invalid heartbeat reply: session {session}, sequence {reply_sequence}; expected session {}, sequence {sequence}",
                authority.session
            ),
            Message::Error(message) => bail!("Slave rejected heartbeat: {message}"),
            other => bail!("expected ALIVE from Slave, received {other:?}"),
        }
        let round_trip = sent_at.elapsed();
        debug!(
            %peer,
            master_session = %authority.session,
            heartbeat_sequence = sequence,
            round_trip_ms = round_trip.as_millis(),
            "Slave heartbeat acknowledged"
        );
        thread::sleep(HEARTBEAT_INTERVAL.saturating_sub(round_trip));
    }
}

fn monitor_master_session(
    stream: &mut TcpStream,
    master: SocketAddr,
    authority: Authority,
    player: &Player,
) -> Result<()> {
    let mut last_heartbeat_at = Instant::now();
    let mut last_sequence = None;
    loop {
        let age = last_heartbeat_at.elapsed();
        let remaining = LIVENESS_TIMEOUT
            .checked_sub(age)
            .ok_or_else(|| anyhow!("heartbeat timeout after {} ms", age.as_millis()))?;
        stream.set_read_timeout(Some(remaining))?;
        let message = read_control_message_until(stream, last_heartbeat_at + LIVENESS_TIMEOUT)
            .context("Master heartbeat unavailable before liveness deadline")?;
        let received_at = Instant::now();
        let (sequence, master_clock, state) =
            validate_heartbeat(&message, authority, last_sequence)?;
        player.accept_authoritative_snapshot(&state, authority)?;
        write_message(
            stream,
            &Message::Alive {
                session: authority.session,
                sequence,
            },
        )
        .context("failed to acknowledge Master heartbeat")?;
        last_heartbeat_at = received_at;
        last_sequence = Some(sequence);
        debug!(
            %master,
            master_session = %authority.session,
            transport_generation = authority.generation,
            heartbeat_sequence = sequence,
            master_clock_ns = master_clock.get(),
            snapshot_anchor_clock_ns = state.anchor_clock.get(),
            snapshot_position_ns = state.media_position.get(),
            snapshot_item_index = state.item_index,
            snapshot_item_position_ns = state.item_position.get(),
            snapshot_playlist_iteration = state.playlist_iteration,
            last_heartbeat_age_ms = last_heartbeat_at.elapsed().as_millis(),
            "Master heartbeat received"
        );
    }
}

fn validate_heartbeat(
    message: &Message,
    authority: Authority,
    last_sequence: Option<u64>,
) -> Result<(u64, ClockNs, StateSnapshot)> {
    let Message::Heartbeat {
        session,
        sequence,
        master_clock,
        state,
    } = message
    else {
        bail!("expected HEARTBEAT from Master, received {message:?}");
    };
    if *session != authority.session {
        bail!(
            "heartbeat session {session} does not match authority {}",
            authority.session
        );
    }
    if last_sequence.is_some_and(|last| *sequence <= last) {
        bail!("stale heartbeat sequence {sequence}; last accepted was {last_sequence:?}");
    }
    if state.transport_state == TransportState::Playing && state.anchor_clock > *master_clock {
        bail!("playing snapshot anchor is later than its heartbeat clock");
    }
    Ok((*sequence, *master_clock, state.clone()))
}

fn read_control_message_until(stream: &mut TcpStream, deadline: Instant) -> Result<Message> {
    let mut bytes = Vec::with_capacity(64);
    let mut byte = [0_u8; 1];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("control-message deadline elapsed"))?;
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut byte) {
            Ok(0) if bytes.is_empty() => bail!("control connection reached EOF"),
            Ok(0) => bail!("control connection ended during a message"),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                if bytes.len() == MAX_LINE_BYTES {
                    bail!("control message exceeds {MAX_LINE_BYTES} bytes");
                }
                bytes.push(byte[0]);
            }
            Err(error) => return Err(error).context("failed to read control message"),
        }
    }
    let line = std::str::from_utf8(&bytes).context("control message is not UTF-8")?;
    Message::decode(line.trim_end_matches('\r')).context("malformed control message")
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
                master_cue_durations_ns,
            } => (
                clock_port,
                Authority {
                    session,
                    generation,
                },
                master_cue_durations_ns,
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
    master: SocketAddr,
    mut authority: Authority,
    player: Arc<Player>,
    subscription: Subscription,
) {
    thread::spawn(move || {
        loop {
            let error = monitor_master_session(&mut stream, master, authority, &player)
                .err()
                .unwrap_or_else(|| anyhow!("Master heartbeat monitor stopped"));
            player.set_clock_health(false);
            if let Err(shutdown_error) = stream.shutdown(Shutdown::Both) {
                debug!(%master, %shutdown_error, "old Master connection was already closed");
            }
            warn!(
                %master,
                master_session = %authority.session,
                transport_generation = authority.generation,
                recovery_reason = "master-liveness",
                %error,
                "Master liveness lost; entering free-running mode"
            );

            loop {
                match connect_to_master(master, &subscription).and_then(
                    |(mut new_stream, clock, new_authority)| {
                        let plan = receive_release(&mut new_stream, &clock, new_authority)?;
                        player.resynchronize(&clock, plan, "master-reconnect")?;
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
}

#[derive(Debug)]
struct PlaybackSequence {
    program_id: String,
    program_hash: ProgramHash,
    cue_ids: Vec<String>,
    items: Vec<MediaItem>,
    cue_durations_ns: Vec<u64>,
    timeline: Option<Timeline>,
    looping: bool,
    from_playlist: bool,
}

impl PlaybackSequence {
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
            info!(item_index = index, cue = %cue.id, media = %path.display(), duration_ns, "program cue discovered");
            items.push(MediaItem { path, uri });
            durations.push(duration_ns);
        }
        let timeline = Timeline::new(durations.clone(), program.looping)
            .context("invalid program sequence duration")?;
        let from_playlist = items.len() > 1;
        Ok(Self {
            program_id: program.program_id.clone(),
            program_hash: program.hash,
            cue_ids: program.cues.iter().map(|cue| cue.id.clone()).collect(),
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

    fn position_error(&self, actual: MediaPositionNs, expected: MediaPositionNs) -> i128 {
        match &self.timeline {
            Some(timeline) if timeline.is_looping() => {
                actual.circular_error(expected, timeline.total_ns())
            }
            _ => actual - expected,
        }
    }

    fn unique_item_for_uri(&self, uri: &str) -> Option<usize> {
        let mut matches = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| (item.uri == uri).then_some(index));
        let index = matches.next()?;
        matches.next().is_none().then_some(index)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlayerRole {
    Master,
    Slave,
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

struct Player {
    pipeline: gst::Pipeline,
    sequence: PlaybackSequence,
    cursor: Arc<Mutex<PlaylistCursor>>,
    role: PlayerRole,
    clock_healthy: AtomicBool,
    clock: Mutex<gst::Clock>,
    plan: Mutex<Option<ReleasePlan>>,
    transport_state: Mutex<TransportState>,
    recovery_lock: Mutex<()>,
    recovery_in_progress: AtomicBool,
    recovery_attempt: Arc<AtomicU64>,
    drift_policy: Mutex<DriftPolicy>,
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
            .set_state(gst::State::Playing)
            .context("failed to request PLAYING at release")?;
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

impl Player {
    fn new(
        args: &PlaybackArgs,
        sequence: PlaybackSequence,
        forced_clock: Option<&gst::Clock>,
    ) -> Result<Self> {
        let first_uri = sequence.items[0].uri.clone();
        let element = gst::ElementFactory::make("playbin3")
            .property("uri", &first_uri)
            .property("mute", args.mute)
            .build()
            .context("playbin3 is unavailable")?;
        let cursor = Arc::new(Mutex::new(PlaylistCursor::new(
            sequence.items.len(),
            sequence.looping,
            0,
        )));
        let weak_element = element.downgrade();
        let callback_cursor = cursor.clone();
        let uris: Vec<String> = sequence.items.iter().map(|item| item.uri.clone()).collect();
        element.connect("about-to-finish", false, move |_| {
            let next = callback_cursor
                .lock()
                .ok()
                .and_then(|mut cursor| cursor.queue_next());
            if let (Some(index), Some(element)) = (next, weak_element.upgrade()) {
                debug!(next_item_index = index, next_uri = %uris[index], "queueing gapless playlist item");
                // Supplying the next URI before EOS lets playbin3 prepare it
                // and carry the running timeline across the boundary.
                element.set_property("uri", &uris[index]);
                debug!(next_item_index = index, "gapless playlist item queued");
            }
            None
        });
        if let Some(name) = &args.video_sink {
            let sink = make_element(name, "video sink")?;
            force_clock_sync(&sink, "video sink")?;
            element.set_property("video-sink", &sink);
        }
        if let Some(name) = &args.audio_sink {
            let sink = make_element(name, "audio sink")?;
            force_clock_sync(&sink, "audio sink")?;
            element.set_property("audio-sink", &sink);
        }
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("playbin3 did not create a GstPipeline"))?;
        if let Some(clock) = forced_clock {
            pipeline.use_clock(Some(clock));
        }
        pipeline
            .set_state(gst::State::Paused)
            .context("failed to request PAUSED")?;
        wait_for_state(
            &pipeline,
            PREROLL_TIMEOUT,
            "media did not preroll before timeout",
        )?;
        if forced_clock.is_some() {
            configure_external_clock_audio_slaving(&pipeline)?;
        }
        let clock = forced_clock.cloned().unwrap_or_else(|| {
            pipeline
                .provide_clock()
                .unwrap_or_else(|| gst::SystemClock::obtain().upcast())
        });
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
            mute = args.mute,
            looped = sequence.looping,
            playlist = sequence.from_playlist,
            "media sequence prerolled"
        );
        Ok(Self {
            pipeline,
            sequence,
            cursor,
            role: PlayerRole::from_external_clock(forced_clock.is_some()),
            clock_healthy: AtomicBool::new(true),
            clock: Mutex::new(clock),
            plan: Mutex::new(None),
            transport_state: Mutex::new(TransportState::Waiting),
            recovery_lock: Mutex::new(()),
            recovery_in_progress: AtomicBool::new(false),
            recovery_attempt: Arc::new(AtomicU64::new(0)),
            drift_policy: Mutex::new(DriftPolicy::default()),
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

    fn resynchronize_inner(
        &self,
        clock: &gst::Clock,
        plan: ReleasePlan,
        recovery_attempt: u64,
        update_authority: bool,
    ) -> Result<()> {
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
        self.pipeline
            .set_property("uri", &self.sequence.items[item_position.index].uri);
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
        let media_position = self
            .sequence
            .position_for_snapshot(item, runtime.playlist_iteration)?;
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
            debug_loop: runtime.debug_loop.map(|debug_loop| DebugLoopState {
                start: MediaPositionNs::new(debug_loop.start_ns),
                end: MediaPositionNs::new(debug_loop.end_ns),
                events_enabled: debug_loop.events_enabled,
            }),
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
        let local_media_position = self.sequence.position_for_snapshot(
            ItemPosition {
                index: item_index,
                local_ns: snapshot.item_position.get(),
            },
            snapshot.playlist_iteration,
        )?;

        let refreshed = ReleasePlan {
            generation: snapshot.generation,
            operation: snapshot.operation,
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
    ) -> Result<()> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| anyhow!("pipeline has no bus"))?;
        let mut next_telemetry = Instant::now();
        let result = loop {
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
                        break Err(anyhow!(
                            "playback pipeline failed: {}",
                            error_message.error()
                        ));
                    }
                    gst::MessageView::Eos(..) => {
                        if self.handle_eos(master_transport)? {
                            info!("end of stream");
                            break Ok(());
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
                next_telemetry = Instant::now() + TELEMETRY_INTERVAL;
            }
            if let Some(error) = self
                .release_error
                .lock()
                .map_err(|_| anyhow!("release error lock poisoned"))?
                .take()
            {
                break Err(anyhow!(error));
            }
        };
        self.pipeline
            .set_state(gst::State::Null)
            .context("failed to stop pipeline")?;
        result
    }

    /// Returns true only for the authoritative end of a non-looping program.
    fn handle_eos(
        &self,
        master_transport: Option<&Arc<Mutex<TransportController>>>,
    ) -> Result<bool> {
        if self.role == PlayerRole::Slave {
            let current_uri = self.pipeline.property::<Option<String>>("current-uri");
            warn!(
                ?current_uri,
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
        let current_uri = self.pipeline.property::<Option<String>>("current-uri");
        let final_uri = self.sequence.items.last().map(|item| item.uri.as_str());
        if current + 1 < self.sequence.items.len() || current_uri.as_deref() != final_uri {
            bail!(
                "media sequence reached EOS before its final item (cursor item {current}, current URI {current_uri:?})"
            );
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

        if sample.transition_in_progress || !self.clock_healthy.load(Ordering::Acquire) {
            self.drift_policy
                .lock()
                .map_err(|_| anyhow!("drift policy lock poisoned"))?
                .suppress();
            debug!(
                transition_in_progress = sample.transition_in_progress,
                clock_healthy = self.clock_healthy.load(Ordering::Acquire),
                "drift sample suppressed"
            );
            return Ok(());
        }

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
        let Some(actual_uri) = self.pipeline.property::<Option<String>>("current-uri") else {
            return Ok(None);
        };
        let Some(actual_item_index) = self.sequence.unique_item_for_uri(&actual_uri) else {
            return Ok(None);
        };
        let actual_item = ItemPosition {
            index: actual_item_index,
            local_ns: actual_local.nseconds(),
        };
        let actual_sequence_position = match self.sequence.sequence_position(actual_item) {
            Ok(position) => position,
            Err(error) => {
                debug!(%error, "item-local position is ambiguous at a cue boundary");
                return Ok(None);
            }
        };
        let cursor_item = self
            .cursor
            .lock()
            .map_err(|_| anyhow!("playlist cursor lock poisoned"))?
            .current();
        let pipeline_state = self.pipeline.current_state();
        let transition_in_progress = self.recovery_in_progress.load(Ordering::Acquire)
            || self
                .scheduled_release
                .lock()
                .map_err(|_| anyhow!("scheduled release lock poisoned"))?
                .is_some()
            || pipeline_state != gst::State::Playing
            || self.pipeline.pending_state() != gst::State::VoidPending
            || cursor_item != actual_item_index;
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

    fn set_clock_health(&self, healthy: bool) {
        self.clock_healthy.store(healthy, Ordering::Release);
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
        Authority, Cli, ClockNs, Command, DRIFT_RECOVERY_COOLDOWN, DriftAction, DriftPolicy,
        DurationMismatchPolicy, MasterSession, MediaItem, MediaPositionNs, Message, Operation,
        PlaybackSequence, ProgramCompatibility, StateSnapshot, Timeline, TransportState,
        can_join_startup, non_negative_seconds, recovery_attempt_is_active, validate_heartbeat,
        validate_snapshot_generation, validate_subscription,
    };
    use clap::Parser;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use syncplayer::program::ProgramHash;
    use syncplayer::protocol::{Capabilities, Subscription};

    fn authority() -> Authority {
        Authority {
            session: MasterSession::new(1),
            generation: 7,
        }
    }

    fn heartbeat(session: MasterSession, sequence: u64) -> Message {
        Message::Heartbeat {
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
            state_port: 0,
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
        assert!(Cli::try_parse_from(["syncplayer", "master", "--media", "a.mp4"]).is_err());
        assert!(Cli::try_parse_from(["syncplayer", "master", "show.toml", "--loop"]).is_err());
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
    fn heartbeat_validation_rejects_wrong_session_and_stale_sequence() {
        let authority = authority();
        assert!(validate_heartbeat(&heartbeat(authority.session, 2), authority, Some(1)).is_ok());
        assert!(validate_heartbeat(&heartbeat(authority.session, 1), authority, Some(1)).is_err());
        assert!(
            validate_heartbeat(&heartbeat(MasterSession::new(2), 2), authority, Some(1)).is_err()
        );
        assert!(validate_heartbeat(&Message::Ready, authority, None).is_err());
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
        let message = Message::Heartbeat {
            session: authority.session,
            sequence: 1,
            master_clock: ClockNs::new(99),
            state: snapshot(authority.generation),
        };
        assert!(validate_heartbeat(&message, authority, None).is_err());
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
    fn obsolete_recovery_attempt_cannot_publish_release() {
        let active = AtomicU64::new(4);
        assert!(recovery_attempt_is_active(&active, 4));
        active.store(5, Ordering::Release);
        assert!(!recovery_attempt_is_active(&active, 4));
    }

    #[test]
    fn sequence_error_is_correct_across_item_and_loop_boundaries() {
        let sequence = |looping| PlaybackSequence {
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            cue_ids: vec!["a".to_owned(), "b".to_owned()],
            items: vec![
                MediaItem {
                    path: PathBuf::from("a"),
                    uri: "file:///a".to_owned(),
                },
                MediaItem {
                    path: PathBuf::from("b"),
                    uri: "file:///b".to_owned(),
                },
            ],
            cue_durations_ns: vec![10, 20],
            timeline: Some(Timeline::new(vec![10, 20], looping).unwrap()),
            looping,
            from_playlist: true,
        };
        assert_eq!(
            sequence(false).position_error(MediaPositionNs::new(11), MediaPositionNs::new(9)),
            2
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
    }

    #[test]
    fn authoritative_cue_maps_through_local_playlist_durations() {
        let sequence = PlaybackSequence {
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            cue_ids: vec!["a".to_owned(), "b".to_owned()],
            items: vec![
                MediaItem {
                    path: PathBuf::from("a"),
                    uri: "file:///a".to_owned(),
                },
                MediaItem {
                    path: PathBuf::from("b"),
                    uri: "file:///b".to_owned(),
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
