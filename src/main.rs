use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_net as gst_net;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
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
    /// Export a pipeline clock and coordinate one Slave.
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
    let player = Player::new(&args.playback, &clock)?;
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
    info!(control = %listener.local_addr()?, clock_port = provider.port(), "Master ready; waiting for one Slave");
    let (mut stream, peer) = listener.accept().context("failed to accept Slave")?;
    configure_stream(&stream)?;
    info!(%peer, "Slave connected");

    write_message(
        &mut stream,
        &Message::Clock {
            port: provider
                .port()
                .try_into()
                .context("invalid provider port")?,
        },
    )?;
    match read_message(&mut stream).context("failed to read Slave readiness")? {
        Message::Ready => info!(%peer, "Slave clock is synchronized and media is prerolled"),
        Message::Error(message) => bail!("Slave rejected preparation: {message}"),
        other => bail!("expected READY from Slave, received {other:?}"),
    }

    let delay = Duration::from_secs_f64(args.release_delay);
    let now = ClockNs::new(clock.time().nseconds());
    let release_at = now
        .checked_add(delay)
        .ok_or_else(|| anyhow!("release timestamp overflow"))?;
    let plan = ReleasePlan {
        generation: 1,
        operation: Operation::Start,
        media_position: MediaPositionNs::new(0),
        release_at,
    };
    plan.validate(now, None).context("invalid release plan")?;
    write_message(&mut stream, &Message::Release(plan)).context("failed to send release plan")?;
    player.release(plan)?;
    info!(
        release_at_ns = release_at.get(),
        delay_s = args.release_delay,
        "scheduled shared-clock release"
    );

    // The control connection is intentionally no longer a playback dependency.
    drop(stream);
    player.monitor(plan)
}

fn run_slave(args: &SlaveArgs) -> Result<()> {
    let mut stream = TcpStream::connect_timeout(&args.master, IO_TIMEOUT)
        .with_context(|| format!("failed to connect to Master {}", args.master))?;
    configure_stream(&stream)?;
    let port = match read_message(&mut stream).context("failed to read Master clock endpoint")? {
        Message::Clock { port } => port,
        Message::Error(message) => bail!("Master rejected connection: {message}"),
        other => bail!("expected CLOCK from Master, received {other:?}"),
    };
    let master_ip = args.master.ip().to_string();
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

    let player = Player::new(&args.playback, &clock)?;
    write_message(&mut stream, &Message::Ready).context("failed to report readiness")?;
    let plan = match read_message(&mut stream).context("failed to read release plan")? {
        Message::Release(plan) => plan,
        Message::Error(message) => bail!("Master cancelled release: {message}"),
        other => bail!("expected RELEASE from Master, received {other:?}"),
    };
    plan.validate(ClockNs::new(clock.time().nseconds()), None)
        .context("Master supplied an unusable release plan")?;
    player.release(plan)?;
    info!(
        release_at_ns = plan.release_at.get(),
        "accepted shared-clock release"
    );

    // Free-running after release is intentional for this experiment.
    drop(stream);
    player.monitor(plan)
}

struct Player {
    pipeline: gst::Pipeline,
    clock: gst::Clock,
}

impl Player {
    fn new(args: &PlaybackArgs, clock: &impl IsA<gst::Clock>) -> Result<Self> {
        let uri = media_uri(&args.media)?;
        let element = gst::ElementFactory::make("playbin3")
            .property("uri", &uri)
            .property("mute", args.mute)
            .build()
            .context("playbin3 is unavailable")?;
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
        info!(media = %args.media.display(), duration_ns = duration.map(gst::ClockTime::nseconds), mute = args.mute, "media prerolled");
        Ok(Self {
            pipeline,
            clock: clock.clone().upcast(),
        })
    }

    fn release(&self, plan: ReleasePlan) -> Result<()> {
        if plan.media_position.get() != 0 {
            self.pipeline
                .seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::from_nseconds(plan.media_position.get()),
                )
                .context("failed to seek to release position")?;
            wait_for_state(
                &self.pipeline,
                PREROLL_TIMEOUT,
                "seek did not preroll before timeout",
            )?;
        }
        self.pipeline.set_start_time(gst::ClockTime::NONE);
        self.pipeline
            .set_base_time(gst::ClockTime::from_nseconds(plan.release_at.get()));
        self.pipeline
            .set_state(gst::State::Playing)
            .context("failed to request PLAYING")?;
        Ok(())
    }

    fn monitor(&self, plan: ReleasePlan) -> Result<()> {
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
                        info!("end of stream");
                        break Ok(());
                    }
                    gst::MessageView::ClockLost(..) => {
                        warn!("pipeline reported CLOCK_LOST; experiment is no longer valid");
                        break Err(anyhow!("pipeline lost the shared clock"));
                    }
                    _ => unreachable!(),
                }
            }
            if Instant::now() >= next_telemetry {
                self.log_telemetry(plan);
                next_telemetry = Instant::now() + Duration::from_secs(1);
            }
        };
        self.pipeline
            .set_state(gst::State::Null)
            .context("failed to stop pipeline")?;
        result
    }

    fn log_telemetry(&self, plan: ReleasePlan) {
        let clock_now = self.clock.time().nseconds();
        let expected = clock_now
            .saturating_sub(plan.release_at.get())
            .saturating_add(plan.media_position.get());
        if let Some(actual) = self.pipeline.query_position::<gst::ClockTime>() {
            let error_ns = i128::from(actual.nseconds()) - i128::from(expected);
            debug!(
                clock_ns = clock_now,
                expected_ns = expected,
                actual_ns = actual.nseconds(),
                error_ns,
                "position telemetry"
            );
        }
    }
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
