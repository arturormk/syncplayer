//! Host-local timed-event scheduling and asynchronous execution.

use crate::program::{Event, EventAction, EventTiming};
use crate::protocol::{MasterSession, TransportState};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use tracing::{error, info};

#[derive(Clone, Debug)]
pub struct EventCue {
    pub id: String,
    pub duration_ns: u64,
    pub events: Vec<Event>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventObservation {
    pub session: MasterSession,
    pub generation: u64,
    pub cue_index: usize,
    pub cue_occurrence: u64,
    pub playlist_iteration: u64,
    pub cue_position_ns: u64,
    pub transport_state: TransportState,
    pub debug_loop_events_enabled: Option<bool>,
}

#[derive(Clone, Debug)]
struct ScheduledEvent {
    event: Event,
    time_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Traversal {
    session: MasterSession,
    generation: u64,
    cue_index: usize,
    cue_occurrence: u64,
    playlist_iteration: u64,
}

impl Traversal {
    const fn from_observation(observation: EventObservation) -> Self {
        Self {
            session: observation.session,
            generation: observation.generation,
            cue_index: observation.cue_index,
            cue_occurrence: observation.cue_occurrence,
            playlist_iteration: observation.playlist_iteration,
        }
    }
}

#[derive(Debug)]
struct Cursor {
    traversal: Traversal,
    position_ns: u64,
    fired: HashSet<String>,
}

/// Detects normal forward crossings and suppresses duplicates within one traversal.
#[derive(Debug)]
pub struct EventScheduler {
    cues: Vec<(String, Vec<ScheduledEvent>)>,
    cursor: Option<Cursor>,
    discontinuity_pending: bool,
}

impl EventScheduler {
    /// Resolves cue-relative event markers to integer nanoseconds.
    ///
    /// # Errors
    ///
    /// Returns an error for an event outside its cue or an empty cue duration.
    pub fn new(cues: Vec<EventCue>) -> Result<Self> {
        let mut resolved = Vec::with_capacity(cues.len());
        for cue in cues {
            if cue.duration_ns == 0 {
                bail!("cue '{}' has zero duration", cue.id);
            }
            let mut events = Vec::with_capacity(cue.events.len());
            for event in cue.events {
                let time_ns = match event.timing {
                    EventTiming::At(time_ns) if time_ns < cue.duration_ns => time_ns,
                    EventTiming::At(time_ns) => bail!(
                        "event '{}' at {time_ns} ns is at or beyond cue '{}' EOF {} ns",
                        event.id,
                        cue.id,
                        cue.duration_ns
                    ),
                    EventTiming::BeforeEnd(offset_ns) if offset_ns <= cue.duration_ns => {
                        cue.duration_ns - offset_ns
                    }
                    EventTiming::BeforeEnd(offset_ns) => bail!(
                        "event '{}' before_end {offset_ns} ns exceeds cue '{}' duration {} ns",
                        event.id,
                        cue.id,
                        cue.duration_ns
                    ),
                };
                events.push(ScheduledEvent { event, time_ns });
            }
            events.sort_by_key(|event| event.time_ns);
            resolved.push((cue.id, events));
        }
        Ok(Self {
            cues: resolved,
            cursor: None,
            discontinuity_pending: false,
        })
    }

    /// Makes the next observation a non-firing baseline after a local seek or repair.
    pub fn mark_discontinuity(&mut self) {
        self.discontinuity_pending = true;
    }

    #[must_use]
    pub fn observe(&mut self, observation: EventObservation) -> Vec<FiredEvent> {
        let Some((cue_id, events)) = self.cues.get(observation.cue_index) else {
            self.cursor = None;
            return Vec::new();
        };
        let traversal = Traversal::from_observation(observation);
        let traversal_changed = self
            .cursor
            .as_ref()
            .is_none_or(|cursor| cursor.traversal != traversal);
        let continuous_cue_change = self.cursor.as_ref().is_some_and(|cursor| {
            cursor.traversal.session == traversal.session
                && cursor.traversal.generation == traversal.generation
                && cursor.traversal.cue_occurrence != traversal.cue_occurrence
        });

        if self.discontinuity_pending || (traversal_changed && !continuous_cue_change) {
            self.discontinuity_pending = false;
            let preserve_start_marker = observation.cue_position_ns == 0
                && observation.cue_occurrence == 0
                && observation.playlist_iteration == 0;
            self.cursor = Some(Cursor {
                traversal,
                position_ns: observation.cue_position_ns,
                fired: events
                    .iter()
                    .filter(|event| {
                        event.time_ns < observation.cue_position_ns
                            || (!preserve_start_marker
                                && event.time_ns == observation.cue_position_ns)
                    })
                    .map(|event| event.event.id.clone())
                    .collect(),
            });
            return Vec::new();
        }

        if continuous_cue_change {
            self.cursor = Some(Cursor {
                traversal,
                position_ns: 0,
                fired: HashSet::new(),
            });
        }
        let Some(cursor) = self.cursor.as_mut() else {
            return Vec::new();
        };
        if observation.transport_state != TransportState::Playing
            || observation.cue_position_ns < cursor.position_ns
        {
            cursor.position_ns = observation.cue_position_ns;
            return Vec::new();
        }

        let events_enabled = observation.debug_loop_events_enabled.unwrap_or(true);
        let mut fired = Vec::new();
        for scheduled in events {
            if scheduled.time_ns >= cursor.position_ns
                && scheduled.time_ns <= observation.cue_position_ns
                && cursor.fired.insert(scheduled.event.id.clone())
                && events_enabled
            {
                fired.push(FiredEvent {
                    player: String::new(),
                    cue: cue_id.clone(),
                    cue_time_ns: scheduled.time_ns,
                    traversal,
                    event: scheduled.event.clone(),
                });
            }
        }
        cursor.position_ns = observation.cue_position_ns;
        fired
    }
}

#[derive(Clone, Debug)]
pub struct FiredEvent {
    player: String,
    cue: String,
    cue_time_ns: u64,
    traversal: Traversal,
    event: Event,
}

enum Work {
    Fire(FiredEvent),
    Shutdown,
}

/// Owns prepared UDP sockets and a non-blocking event-dispatch worker.
pub struct EventExecutor {
    player: String,
    sender: Sender<Work>,
    worker: Option<JoinHandle<()>>,
}

impl EventExecutor {
    /// Prepares the UDP resources required by the configured actions.
    ///
    /// # Errors
    ///
    /// Returns an error if a required local UDP socket cannot be created.
    pub fn new(player: String, cues: &[EventCue]) -> Result<Self> {
        let needs_v4 = cues.iter().flat_map(|cue| &cue.events).any(|event| {
            matches!(
                event.action,
                EventAction::Udp {
                    host: IpAddr::V4(_),
                    ..
                }
            )
        });
        let needs_v6 = cues.iter().flat_map(|cue| &cue.events).any(|event| {
            matches!(
                event.action,
                EventAction::Udp {
                    host: IpAddr::V6(_),
                    ..
                }
            )
        });
        let ipv4 = needs_v4
            .then(|| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
            .transpose()
            .context("failed to prepare IPv4 event socket")?;
        let ipv6 = needs_v6
            .then(|| UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)))
            .transpose()
            .context("failed to prepare IPv6 event socket")?;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("syncplayer-events".to_owned())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    match work {
                        Work::Fire(event) => execute_event(event, ipv4.as_ref(), ipv6.as_ref()),
                        Work::Shutdown => break,
                    }
                }
            })
            .context("failed to start event worker")?;
        Ok(Self {
            player,
            sender,
            worker: Some(worker),
        })
    }

    pub fn dispatch(&self, mut event: FiredEvent) {
        event.player.clone_from(&self.player);
        if self.sender.send(Work::Fire(event)).is_err() {
            error!(player = %self.player, "event worker is unavailable");
        }
    }
}

impl Drop for EventExecutor {
    fn drop(&mut self) {
        let _ = self.sender.send(Work::Shutdown);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            error!(player = %self.player, "event worker panicked during shutdown");
        }
    }
}

fn execute_event(event: FiredEvent, ipv4: Option<&UdpSocket>, ipv6: Option<&UdpSocket>) {
    let action_type = match &event.event.action {
        EventAction::Process { .. } => "process",
        EventAction::Udp { .. } => "udp",
    };
    info!(
        player = %event.player,
        cue = %event.cue,
        cue_time_ns = event.cue_time_ns,
        event = %event.event.id,
        event_type = action_type,
        session = %event.traversal.session,
        generation = event.traversal.generation,
        playlist_iteration = event.traversal.playlist_iteration,
        cue_occurrence = event.traversal.cue_occurrence,
        "timed event fired"
    );
    match event.event.action {
        EventAction::Process { command } => execute_process(&event.event.id, &command),
        EventAction::Udp {
            host,
            port,
            payload,
        } => {
            let socket = match host {
                IpAddr::V4(_) => ipv4,
                IpAddr::V6(_) => ipv6,
            };
            let Some(socket) = socket else {
                error!(event = %event.event.id, %host, port, "prepared UDP event socket is unavailable");
                return;
            };
            let target = SocketAddr::new(host, port);
            match socket.send_to(&payload, target) {
                Ok(bytes) => info!(event = %event.event.id, %target, bytes, "UDP event sent"),
                Err(error) => error!(event = %event.event.id, %target, %error, "UDP event failed"),
            }
        }
    }
}

fn execute_process(event_id: &str, command: &[String]) {
    let Some((program, arguments)) = command.split_first() else {
        error!(event = %event_id, "process event has no command");
        return;
    };
    let program = program.clone();
    let arguments = arguments.to_vec();
    let reaper_event = event_id.to_owned();
    let spawn = thread::Builder::new()
        .name("syncplayer-event-process".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let child = Command::new("nice")
                .args(["-n", "19", "--", &program])
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match child {
                Ok(mut child) => match child.wait() {
                    Ok(status) => info!(
                        event = %reaper_event,
                        exit = status.code(),
                        success = status.success(),
                        duration_ms = started.elapsed().as_millis(),
                        "process event completed"
                    ),
                    Err(error) => {
                        error!(event = %reaper_event, %error, "process event wait failed");
                    }
                },
                Err(error) => error!(event = %reaper_event, %error, "process event launch failed"),
            }
        });
    if let Err(error) = spawn {
        error!(event = %event_id, %error, "failed to start process event worker");
    }
}

/// Combines the crossing scheduler with its asynchronous executor.
pub struct EventRuntime {
    scheduler: EventScheduler,
    executor: EventExecutor,
}

impl EventRuntime {
    /// Creates a complete host-local runtime and prepares its network resources.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid event timing, unavailable UDP sockets, or
    /// failure to start the asynchronous executor.
    pub fn new(player: String, cues: Vec<EventCue>) -> Result<Self> {
        let executor = EventExecutor::new(player, &cues)?;
        let scheduler = EventScheduler::new(cues)?;
        Ok(Self {
            scheduler,
            executor,
        })
    }

    pub fn observe(&mut self, observation: EventObservation) {
        for event in self.scheduler.observe(observation) {
            self.executor.dispatch(event);
        }
    }

    pub fn mark_discontinuity(&mut self) {
        self.scheduler.mark_discontinuity();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_event(id: &str, time_ns: u64) -> Event {
        Event {
            id: id.to_owned(),
            timing: EventTiming::At(time_ns),
            action: EventAction::Process {
                command: vec!["true".to_owned()],
            },
        }
    }

    fn scheduler() -> EventScheduler {
        EventScheduler::new(vec![
            EventCue {
                id: "a".to_owned(),
                duration_ns: 100,
                events: vec![process_event("start", 0), process_event("middle", 50)],
            },
            EventCue {
                id: "b".to_owned(),
                duration_ns: 100,
                events: vec![process_event("next", 10)],
            },
        ])
        .unwrap()
    }

    fn observation(cue: usize, occurrence: u64, position: u64) -> EventObservation {
        EventObservation {
            session: MasterSession::new(1),
            generation: 1,
            cue_index: cue,
            cue_occurrence: occurrence,
            playlist_iteration: u64::from(cue == 0 && occurrence > 1),
            cue_position_ns: position,
            transport_state: TransportState::Playing,
            debug_loop_events_enabled: None,
        }
    }

    #[test]
    fn normal_crossings_fire_once_despite_jitter() {
        let mut scheduler = scheduler();
        assert!(scheduler.observe(observation(0, 0, 0)).is_empty());
        assert_eq!(scheduler.observe(observation(0, 0, 1)).len(), 1);
        assert_eq!(scheduler.observe(observation(0, 0, 60)).len(), 1);
        assert!(scheduler.observe(observation(0, 0, 49)).is_empty());
        assert!(scheduler.observe(observation(0, 0, 60)).is_empty());
    }

    #[test]
    fn initial_join_seek_and_resynchronization_do_not_fire_skipped_events() {
        let mut joined = scheduler();
        assert!(joined.observe(observation(0, 0, 60)).is_empty());
        assert!(joined.observe(observation(0, 0, 70)).is_empty());

        let mut repaired = scheduler();
        let _ = repaired.observe(observation(0, 0, 10));
        repaired.mark_discontinuity();
        assert!(repaired.observe(observation(0, 0, 60)).is_empty());

        let mut sought = scheduler();
        let _ = sought.observe(observation(0, 0, 10));
        let mut after_goto = observation(0, 1, 60);
        after_goto.generation = 2;
        assert!(sought.observe(after_goto).is_empty());
    }

    #[test]
    fn new_normal_traversal_reenables_events() {
        let mut scheduler = scheduler();
        let _ = scheduler.observe(observation(0, 0, 0));
        assert_eq!(scheduler.observe(observation(0, 0, 60)).len(), 2);
        assert_eq!(scheduler.observe(observation(1, 1, 20)).len(), 1);
        assert_eq!(scheduler.observe(observation(0, 2, 60)).len(), 2);
    }

    #[test]
    fn pause_and_debug_suppression_preserve_crossing_semantics() {
        let mut paused_scheduler = scheduler();
        let _ = paused_scheduler.observe(observation(0, 0, 10));
        let mut paused = observation(0, 0, 10);
        paused.generation = 2;
        paused.transport_state = TransportState::Paused;
        assert!(paused_scheduler.observe(paused).is_empty());
        let mut resumed = observation(0, 0, 10);
        resumed.generation = 3;
        assert!(paused_scheduler.observe(resumed).is_empty());
        let mut after_resume = observation(0, 0, 70);
        after_resume.generation = 3;
        assert_eq!(paused_scheduler.observe(after_resume).len(), 1);

        let mut debug = scheduler();
        let _ = debug.observe(observation(0, 0, 10));
        let mut suppressed = observation(0, 0, 60);
        suppressed.debug_loop_events_enabled = Some(false);
        assert!(debug.observe(suppressed).is_empty());
        let mut enabled = observation(0, 0, 70);
        enabled.debug_loop_events_enabled = Some(true);
        assert!(debug.observe(enabled).is_empty());
    }

    #[test]
    fn before_end_is_resolved_and_out_of_range_events_are_rejected() {
        let cue = EventCue {
            id: "cue".to_owned(),
            duration_ns: 100,
            events: vec![Event {
                id: "end".to_owned(),
                timing: EventTiming::BeforeEnd(10),
                action: EventAction::Process {
                    command: vec!["true".to_owned()],
                },
            }],
        };
        let mut scheduler = EventScheduler::new(vec![cue]).unwrap();
        let _ = scheduler.observe(observation(0, 0, 80));
        assert_eq!(scheduler.observe(observation(0, 0, 90)).len(), 1);

        let invalid = EventCue {
            id: "cue".to_owned(),
            duration_ns: 100,
            events: vec![process_event("late", 100)],
        };
        assert!(EventScheduler::new(vec![invalid]).is_err());
    }
}
