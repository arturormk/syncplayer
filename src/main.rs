use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_net as gst_net;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use syncplayer::protocol::{Message, Operation, ReleasePlan, read_message, write_message};
use syncplayer::time::{ClockNs, MediaPositionNs};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

const PREROLL_TIMEOUT: Duration = Duration::from_secs(30);
const CLOCK_SYNC_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Local media file. Master and Slave paths may differ.
    #[arg(long, value_name = "PATH")]
    media: PathBuf,

    /// Silence the selected audio stream.
    #[arg(long)]
    mute: bool,

    /// Repeat the media continuously using playbin3's gapless handoff.
    #[arg(long)]
    r#loop: bool,

    /// Override `GStreamer`'s automatically selected video sink.
    #[arg(long)]
    video_sink: Option<String>,

    /// Override `GStreamer`'s automatically selected audio sink.
    #[arg(long)]
    audio_sink: Option<String>,
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
    }
}

fn run_master(args: &MasterArgs) -> Result<()> {
    // Let the pipeline choose its natural clock. With a real audio sink this
    // is normally the device-backed audio clock, so the Master does not have
    // to slave its own output to an unrelated system clock.
    let player = Arc::new(Player::new(&args.playback, None)?);
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
    let initial_plan = player.schedule_start(startup_delay)?;
    if startup_delay.is_zero() {
        info!(
            control = %listener.local_addr()?,
            clock_port,
            "Master playback started; ready to welcome Slaves"
        );
    } else {
        info!(
            control = %listener.local_addr()?,
            clock_port,
            release_at_ns = initial_plan.release_at.get(),
            startup_delay_s = args.startup_delay,
            "Master prerolled on the first frame; synchronized startup scheduled"
        );
    }

    serve_slaves(
        listener,
        player.clone(),
        clock,
        clock_port,
        release_delay,
        initial_plan,
    );
    player.monitor(initial_plan)
}

fn run_slave(args: &SlaveArgs) -> Result<()> {
    let (mut stream, clock) = connect_to_master(args.master)?;
    let player = Arc::new(Player::new(&args.playback, Some(&clock))?);
    let plan = receive_release(&mut stream, &clock)?;
    player.resynchronize(&clock, plan)?;
    info!(
        release_at_ns = plan.release_at.get(),
        "accepted shared-clock release"
    );

    monitor_master(stream, args.master, player.clone());
    player.monitor(plan)
}

fn serve_slaves(
    listener: TcpListener,
    player: Arc<Player>,
    clock: gst::Clock,
    clock_port: u16,
    release_delay: Duration,
    startup_plan: ReleasePlan,
) {
    thread::spawn(move || {
        let server = Arc::new(SlaveServer {
            player,
            clock,
            clock_port,
            release_delay,
            startup_plan,
            generation: AtomicU64::new(1),
        });
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
    generation: AtomicU64,
}

fn welcome_slave(mut stream: TcpStream, peer: SocketAddr, server: &SlaveServer) -> Result<()> {
    configure_stream(&stream)?;
    write_message(
        &mut stream,
        &Message::Clock {
            port: server.clock_port,
        },
    )?;
    match read_message(&mut stream).context("failed to read Slave readiness")? {
        Message::Ready => info!(%peer, "Slave clock is synchronized and ready to rejoin"),
        Message::Error(message) => bail!("Slave rejected preparation: {message}"),
        other => bail!("expected READY from Slave, received {other:?}"),
    }

    let now = ClockNs::new(server.clock.time().nseconds());
    let minimum_lead_ns = u64::try_from(server.release_delay.as_nanos())
        .context("release delay is too large for the clock")?;
    let plan = if can_join_startup(server.startup_plan.release_at, now, minimum_lead_ns) {
        info!(%peer, "Slave is ready for the shared startup deadline");
        server.startup_plan
    } else {
        let release_at = now
            .checked_add(server.release_delay)
            .ok_or_else(|| anyhow!("release timestamp overflow"))?;
        server.player.join_plan(
            server.generation.fetch_add(1, Ordering::Relaxed) + 1,
            release_at,
        )?
    };
    write_message(&mut stream, &Message::Release(plan))
        .context("failed to send synchronization state")?;
    info!(
        %peer,
        position_ns = plan.media_position.get(),
        release_at_ns = plan.release_at.get(),
        "Slave welcomed with current playback state"
    );

    // Keeping this otherwise-idle connection open gives the Slave a prompt,
    // reliable indication that this Master session has disappeared.
    stream.set_read_timeout(None)?;
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn connect_to_master(master: SocketAddr) -> Result<(TcpStream, gst::Clock)> {
    let mut stream = TcpStream::connect_timeout(&master, IO_TIMEOUT)
        .with_context(|| format!("failed to connect to Master {master}"))?;
    configure_stream(&stream)?;
    let port = match read_message(&mut stream).context("failed to read Master clock endpoint")? {
        Message::Clock { port } => port,
        Message::Error(message) => bail!("Master rejected connection: {message}"),
        other => bail!("expected CLOCK from Master, received {other:?}"),
    };
    let master_ip = master.ip().to_string();
    let clock = gst_net::NetClientClock::new(
        Some("syncplayer-lab-clock"),
        &master_ip,
        i32::from(port),
        Some(gst::ClockTime::ZERO),
    );
    info!(master = %master_ip, clock_port = port, "calibrating network clock");
    clock
        .wait_for_sync(Some(to_clock_time(CLOCK_SYNC_TIMEOUT)?))
        .context("network clock did not synchronize before timeout")?;
    info!(
        clock_time_ns = clock.time().nseconds(),
        "network clock synchronized"
    );
    Ok((stream, clock.upcast()))
}

fn receive_release(stream: &mut TcpStream, clock: &gst::Clock) -> Result<ReleasePlan> {
    write_message(stream, &Message::Ready).context("failed to report readiness")?;
    let plan = match read_message(stream).context("failed to read release plan")? {
        Message::Release(plan) => plan,
        Message::Error(message) => bail!("Master cancelled release: {message}"),
        other => bail!("expected RELEASE from Master, received {other:?}"),
    };
    plan.validate(ClockNs::new(clock.time().nseconds()), None)
        .context("Master supplied an unusable release plan")
}

fn monitor_master(mut stream: TcpStream, master: SocketAddr, player: Arc<Player>) {
    thread::spawn(move || {
        loop {
            stream.set_read_timeout(None).ok();
            let mut byte = [0_u8; 1];
            while matches!(stream.read(&mut byte), Ok(1)) {}
            warn!(%master, "Master connection lost; continuing on the current clock");

            loop {
                match connect_to_master(master).and_then(|(mut new_stream, clock)| {
                    let plan = receive_release(&mut new_stream, &clock)?;
                    player.resynchronize(&clock, plan)?;
                    Ok((new_stream, plan))
                }) {
                    Ok((new_stream, plan)) => {
                        stream = new_stream;
                        info!(
                            %master,
                            release_at_ns = plan.release_at.get(),
                            "Master reconnected; Slave paused and resynchronized"
                        );
                        break;
                    }
                    Err(error) => {
                        warn!(%master, %error, "Master reconnect attempt failed; still free-running");
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        }
    });
}

struct Player {
    pipeline: gst::Pipeline,
    clock: Mutex<gst::Clock>,
    plan: Mutex<Option<ReleasePlan>>,
    scheduled_release: Arc<Mutex<Option<ScheduledRelease>>>,
    release_error: Arc<Mutex<Option<String>>>,
    loop_duration_ns: Option<u64>,
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

        // Hold this lock through set_state so cancellation cannot pause and
        // then lose a race to an obsolete release worker.
        let mut scheduled = self
            .scheduled_release
            .lock()
            .map_err(|_| anyhow!("scheduled release lock poisoned"))?;
        if !release_is_active(scheduled.as_ref(), &self.release_id) {
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
    fn new(args: &PlaybackArgs, forced_clock: Option<&gst::Clock>) -> Result<Self> {
        let uri = media_uri(&args.media)?;
        let element = gst::ElementFactory::make("playbin3")
            .property("uri", &uri)
            .property("mute", args.mute)
            .build()
            .context("playbin3 is unavailable")?;
        if args.r#loop {
            let weak_element = element.downgrade();
            let loop_uri = uri.clone();
            element.connect("about-to-finish", false, move |_| {
                if let Some(element) = weak_element.upgrade() {
                    // Supplying the next URI before EOS lets playbin3 prepare
                    // it and carry the running timeline across the boundary.
                    element.set_property("uri", &loop_uri);
                }
                None
            });
        }
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
        let loop_duration_ns = if args.r#loop {
            let duration_ns = duration
                .map(gst::ClockTime::nseconds)
                .filter(|duration_ns| *duration_ns > 0)
                .ok_or_else(|| anyhow!("--loop requires media with a known, non-zero duration"))?;
            Some(duration_ns)
        } else {
            None
        };
        info!(media = %args.media.display(), duration_ns = duration.map(gst::ClockTime::nseconds), mute = args.mute, looped = args.r#loop, "media prerolled");
        Ok(Self {
            pipeline,
            clock: Mutex::new(clock),
            plan: Mutex::new(None),
            scheduled_release: Arc::new(Mutex::new(None)),
            release_error: Arc::new(Mutex::new(None)),
            loop_duration_ns,
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
        self.schedule_playing(plan)?;
        Ok(plan)
    }

    fn resynchronize(&self, clock: &gst::Clock, plan: ReleasePlan) -> Result<()> {
        self.cancel_scheduled_release()?;
        self.pipeline
            .set_state(gst::State::Paused)
            .context("failed to pause for synchronization")?;
        wait_for_state(
            &self.pipeline,
            PREROLL_TIMEOUT,
            "media did not pause before synchronization timeout",
        )?;
        self.pipeline.use_clock(Some(clock));
        *self
            .clock
            .lock()
            .map_err(|_| anyhow!("clock lock poisoned"))? = clock.clone();

        let media_position = self.normalize(plan.media_position);
        self.pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::from_nseconds(media_position.get()),
            )
            .context("failed to seek to synchronization position")?;
        wait_for_state(
            &self.pipeline,
            PREROLL_TIMEOUT,
            "synchronization seek did not preroll before timeout",
        )?;
        self.schedule_playing(plan)
    }

    fn schedule_playing(&self, plan: ReleasePlan) -> Result<()> {
        self.cancel_scheduled_release()?;
        *self
            .release_error
            .lock()
            .map_err(|_| anyhow!("release error lock poisoned"))? = None;
        *self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))? = Some(plan);

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
            media_position: projected_position(
                current_plan.media_position.get(),
                elapsed,
                self.loop_duration_ns,
            ),
            release_at,
        })
    }

    fn monitor(&self, initial_plan: ReleasePlan) -> Result<()> {
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
                        if self.loop_duration_ns.is_some() {
                            break Err(anyhow!(
                                "looping pipeline reached EOS instead of performing a gapless handoff"
                            ));
                        }
                        info!("end of stream");
                        break Ok(());
                    }
                    gst::MessageView::ClockLost(..) => {
                        warn!(
                            "pipeline reported CLOCK_LOST; continuing while synchronization recovers"
                        );
                    }
                    _ => unreachable!(),
                }
            }
            if Instant::now() >= next_telemetry {
                self.log_telemetry(initial_plan);
                next_telemetry = Instant::now() + Duration::from_secs(1);
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

    fn log_telemetry(&self, fallback_plan: ReleasePlan) {
        let plan = self
            .plan
            .lock()
            .ok()
            .and_then(|plan| *plan)
            .unwrap_or(fallback_plan);
        let Ok(clock) = self.clock() else {
            return;
        };
        let clock_now = clock.time().nseconds();
        let expected = clock_now
            .saturating_sub(plan.release_at.get())
            .saturating_add(plan.media_position.get());
        if let Some(actual) = self.pipeline.query_position::<gst::ClockTime>() {
            let expected = self.normalize(MediaPositionNs::new(expected));
            let actual = self.normalize(MediaPositionNs::new(actual.nseconds()));
            let error_ns = self.loop_duration_ns.map_or_else(
                || actual - expected,
                |duration_ns| actual.circular_error(expected, duration_ns),
            );
            debug!(
                clock_ns = clock_now,
                expected_ns = expected.get(),
                actual_ns = actual.get(),
                error_ns,
                "position telemetry"
            );
        }
    }

    fn normalize(&self, position: MediaPositionNs) -> MediaPositionNs {
        self.loop_duration_ns
            .map_or(position, |duration_ns| position.modulo(duration_ns))
    }

    fn clock(&self) -> Result<gst::Clock> {
        self.clock
            .lock()
            .map(|clock| clock.clone())
            .map_err(|_| anyhow!("clock lock poisoned"))
    }
}

fn record_release_error(release_error: &Mutex<Option<String>>, message: String) {
    error!(error = %message, "scheduled release failed");
    if let Ok(mut release_error) = release_error.lock() {
        *release_error = Some(message);
    }
}

fn projected_position(
    position_ns: u64,
    lead_ns: u64,
    loop_duration_ns: Option<u64>,
) -> MediaPositionNs {
    let projected = MediaPositionNs::new(position_ns.saturating_add(lead_ns));
    loop_duration_ns.map_or(projected, |duration_ns| projected.modulo(duration_ns))
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
    use super::{ClockNs, can_join_startup, non_negative_seconds, projected_position};

    #[test]
    fn late_join_position_accounts_for_release_lead_time() {
        assert_eq!(projected_position(12_000, 5_000, None).get(), 17_000);
    }

    #[test]
    fn late_join_position_wraps_across_a_loop_boundary() {
        assert_eq!(projected_position(58_000, 5_000, Some(60_000)).get(), 3_000);
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
}
