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
    let clock = gst::SystemClock::obtain();
    let player = Arc::new(Player::new(&args.playback, &clock)?);
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
    let initial_plan = player.start_now()?;
    info!(
        control = %listener.local_addr()?,
        clock_port,
        "Master playback started; ready to welcome Slaves"
    );

    serve_slaves(
        listener,
        player.clone(),
        clock.clone().upcast(),
        clock_port,
        Duration::from_secs_f64(args.release_delay),
    );
    player.monitor(initial_plan)
}

fn run_slave(args: &SlaveArgs) -> Result<()> {
    let (mut stream, clock) = connect_to_master(args.master)?;
    let player = Arc::new(Player::new(&args.playback, &clock)?);
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
) {
    thread::spawn(move || {
        let generation = Arc::new(AtomicU64::new(1));
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
            let player = player.clone();
            let clock = clock.clone();
            let generation = generation.clone();
            thread::spawn(move || {
                if let Err(error) = welcome_slave(
                    stream,
                    peer,
                    &player,
                    &clock,
                    clock_port,
                    release_delay,
                    &generation,
                ) {
                    warn!(%peer, %error, "Slave synchronization session ended");
                }
            });
        }
    });
}

fn welcome_slave(
    mut stream: TcpStream,
    peer: SocketAddr,
    player: &Player,
    clock: &gst::Clock,
    clock_port: u16,
    release_delay: Duration,
    generation: &AtomicU64,
) -> Result<()> {
    configure_stream(&stream)?;
    write_message(&mut stream, &Message::Clock { port: clock_port })?;
    match read_message(&mut stream).context("failed to read Slave readiness")? {
        Message::Ready => info!(%peer, "Slave clock is synchronized and ready to rejoin"),
        Message::Error(message) => bail!("Slave rejected preparation: {message}"),
        other => bail!("expected READY from Slave, received {other:?}"),
    }

    let now = ClockNs::new(clock.time().nseconds());
    let release_at = now
        .checked_add(release_delay)
        .ok_or_else(|| anyhow!("release timestamp overflow"))?;
    let plan = player.join_plan(
        generation.fetch_add(1, Ordering::Relaxed) + 1,
        now,
        release_at,
    )?;
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
    loop_duration_ns: Option<u64>,
}

impl Player {
    fn new(args: &PlaybackArgs, clock: &impl IsA<gst::Clock>) -> Result<Self> {
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
        pipeline.use_clock(Some(clock));
        pipeline
            .set_state(gst::State::Paused)
            .context("failed to request PAUSED")?;
        wait_for_state(
            &pipeline,
            PREROLL_TIMEOUT,
            "media did not preroll before timeout",
        )?;
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
            clock: Mutex::new(clock.clone().upcast()),
            plan: Mutex::new(None),
            loop_duration_ns,
        })
    }

    fn start_now(&self) -> Result<ReleasePlan> {
        let release_at = ClockNs::new(self.clock()?.time().nseconds());
        let plan = ReleasePlan {
            generation: 1,
            operation: Operation::Start,
            media_position: MediaPositionNs::new(0),
            release_at,
        };
        self.start_playing(plan)?;
        Ok(plan)
    }

    fn resynchronize(&self, clock: &gst::Clock, plan: ReleasePlan) -> Result<()> {
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
        self.start_playing(plan)
    }

    fn start_playing(&self, plan: ReleasePlan) -> Result<()> {
        self.pipeline.set_start_time(gst::ClockTime::NONE);
        self.pipeline
            .set_base_time(gst::ClockTime::from_nseconds(plan.release_at.get()));
        self.pipeline
            .set_state(gst::State::Playing)
            .context("failed to request PLAYING")?;
        *self
            .plan
            .lock()
            .map_err(|_| anyhow!("plan lock poisoned"))? = Some(plan);
        Ok(())
    }

    fn join_plan(&self, generation: u64, now: ClockNs, release_at: ClockNs) -> Result<ReleasePlan> {
        let position = self
            .pipeline
            .query_position::<gst::ClockTime>()
            .ok_or_else(|| anyhow!("cannot query current Master position"))?;
        let lead = release_at.get().saturating_sub(now.get());
        Ok(ReleasePlan {
            generation,
            operation: Operation::Goto,
            media_position: projected_position(position.nseconds(), lead, self.loop_duration_ns),
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

fn projected_position(
    position_ns: u64,
    lead_ns: u64,
    loop_duration_ns: Option<u64>,
) -> MediaPositionNs {
    let projected = MediaPositionNs::new(position_ns.saturating_add(lead_ns));
    loop_duration_ns.map_or(projected, |duration_ns| projected.modulo(duration_ns))
}

impl Drop for Player {
    fn drop(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::projected_position;

    #[test]
    fn late_join_position_accounts_for_release_lead_time() {
        assert_eq!(projected_position(12_000, 5_000, None).get(), 17_000);
    }

    #[test]
    fn late_join_position_wraps_across_a_loop_boundary() {
        assert_eq!(projected_position(58_000, 5_000, Some(60_000)).get(), 3_000);
    }
}
