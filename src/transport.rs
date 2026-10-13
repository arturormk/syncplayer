//! Authoritative, clock-anchored program transport.

use crate::program::ProgramHash;
use crate::protocol::{MasterSession, Operation, TransportState};
use crate::time::{ClockNs, MediaPositionNs};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DebugLoop {
    pub start_ns: u64,
    pub end_ns: u64,
    pub events_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeState {
    pub session: MasterSession,
    pub generation: u64,
    pub operation: Operation,
    pub program_id: String,
    pub program_hash: ProgramHash,
    pub cue_index: usize,
    pub cue_id: String,
    /// Monotonic traversal identity. It changes at every cue boundary,
    /// including the final-to-first playlist wrap.
    pub cue_occurrence: u64,
    pub playlist_iteration: u64,
    pub anchor_clock: ClockNs,
    pub anchor_cue_position: MediaPositionNs,
    pub cue_duration: MediaPositionNs,
    pub transport_state: TransportState,
    pub debug_loop: Option<DebugLoop>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Cue {
    id: String,
    duration_ns: u64,
}

/// Mutable state owned by the Master.
///
/// Normal cue changes do not increment the generation: they are continuous
/// progression of the same timeline. Discontinuous operator changes do.
#[derive(Clone, Debug)]
pub struct TransportController {
    session: MasterSession,
    generation: u64,
    operation: Operation,
    program_id: String,
    program_hash: ProgramHash,
    cues: Vec<Cue>,
    looping: bool,
    cue_index: usize,
    cue_occurrence: u64,
    playlist_iteration: u64,
    anchor_clock: ClockNs,
    anchor_cue_position_ns: u64,
    state: TransportState,
    debug_loop: Option<DebugLoop>,
}

impl TransportController {
    /// Creates a loaded program, stopped at cue zero.
    ///
    /// # Errors
    ///
    /// Returns an error when cue identities and durations do not describe a
    /// non-empty program of non-zero cues.
    pub fn new(
        session: MasterSession,
        program_id: String,
        program_hash: ProgramHash,
        cue_ids: Vec<String>,
        cue_durations_ns: Vec<u64>,
        looping: bool,
        now: ClockNs,
    ) -> Result<Self, TransportError> {
        let cues = validate_cues(cue_ids, cue_durations_ns)?;
        Ok(Self {
            session,
            generation: 1,
            operation: Operation::Start,
            program_id,
            program_hash,
            cues,
            looping,
            cue_index: 0,
            cue_occurrence: 0,
            playlist_iteration: 0,
            anchor_clock: now,
            anchor_cue_position_ns: 0,
            state: TransportState::Paused,
            debug_loop: None,
        })
    }

    #[must_use]
    pub const fn session(&self) -> MasterSession {
        self.session
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Starts the initially loaded program without creating a second program
    /// generation. `release_at` may be in the future.
    pub fn start(&mut self, release_at: ClockNs) {
        self.anchor_clock = release_at;
        self.operation = Operation::Start;
        self.state = TransportState::Playing;
    }

    #[must_use]
    pub fn snapshot(&self, now: ClockNs) -> RuntimeState {
        let mut projected = self.clone();
        projected.materialize(now);
        let mut state = projected.runtime_state();
        if state.transport_state == TransportState::Playing && now < state.anchor_clock {
            state.transport_state = TransportState::Waiting;
        }
        state
    }

    /// Freezes cue time at `now` and starts a new generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the generation counter is exhausted.
    pub fn pause(&mut self, now: ClockNs) -> Result<(), TransportError> {
        self.materialize(now);
        self.bump_generation()?;
        self.anchor_clock = now;
        self.operation = Operation::Pause;
        self.state = TransportState::Paused;
        Ok(())
    }

    /// Schedules playback from the frozen position as a new generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the generation counter is exhausted.
    pub fn release(&mut self, release_at: ClockNs) -> Result<(), TransportError> {
        self.bump_generation()?;
        self.anchor_clock = release_at;
        self.operation = Operation::Resume;
        self.state = TransportState::Playing;
        Ok(())
    }

    /// Moves to a cue-local position while preserving paused/playing mode.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown cue, an endpoint/out-of-range
    /// position, or generation exhaustion.
    pub fn goto(
        &mut self,
        cue_index: usize,
        cue_position_ns: u64,
        anchor_clock: ClockNs,
    ) -> Result<(), TransportError> {
        let cue = self
            .cues
            .get(cue_index)
            .ok_or(TransportError::UnknownCue(cue_index))?;
        if cue_position_ns >= cue.duration_ns {
            return Err(TransportError::PastCueEnd {
                cue_index,
                position_ns: cue_position_ns,
                duration_ns: cue.duration_ns,
            });
        }
        self.bump_generation()?;
        self.cue_index = cue_index;
        self.cue_occurrence = self.cue_occurrence.saturating_add(1);
        self.anchor_cue_position_ns = cue_position_ns;
        self.anchor_clock = anchor_clock;
        self.operation = Operation::Goto;
        Ok(())
    }

    /// Returns to paused cue zero/time zero as a new generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the generation counter is exhausted.
    pub fn stop(&mut self, now: ClockNs) -> Result<(), TransportError> {
        self.bump_generation()?;
        self.cue_index = 0;
        self.cue_occurrence = self.cue_occurrence.saturating_add(1);
        self.playlist_iteration = 0;
        self.anchor_cue_position_ns = 0;
        self.anchor_clock = now;
        self.state = TransportState::Paused;
        self.debug_loop = None;
        self.operation = Operation::Stop;
        Ok(())
    }

    /// Replaces the loaded program and stops at its first cue.
    ///
    /// # Errors
    ///
    /// Returns an error when the new cue data is invalid or the generation
    /// counter is exhausted. Validation happens before existing state changes.
    pub fn load_program(
        &mut self,
        program_id: String,
        program_hash: ProgramHash,
        cue_ids: Vec<String>,
        cue_durations_ns: Vec<u64>,
        looping: bool,
        now: ClockNs,
    ) -> Result<(), TransportError> {
        let cues = validate_cues(cue_ids, cue_durations_ns)?;
        self.bump_generation()?;
        self.program_id = program_id;
        self.program_hash = program_hash;
        self.cues = cues;
        self.looping = looping;
        self.cue_index = 0;
        self.cue_occurrence = self.cue_occurrence.saturating_add(1);
        self.playlist_iteration = 0;
        self.anchor_clock = now;
        self.anchor_cue_position_ns = 0;
        self.state = TransportState::Paused;
        self.debug_loop = None;
        self.operation = Operation::Load;
        Ok(())
    }

    /// Enables a cue-local loop as a new generation.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, reversed, or cross-boundary loops, or if
    /// the generation counter is exhausted.
    pub fn set_debug_loop(
        &mut self,
        start_ns: u64,
        end_ns: u64,
        events_enabled: bool,
        now: ClockNs,
    ) -> Result<(), TransportError> {
        self.materialize(now);
        let duration_ns = self.cues[self.cue_index].duration_ns;
        if start_ns >= end_ns || end_ns > duration_ns {
            return Err(TransportError::InvalidDebugLoop {
                start_ns,
                end_ns,
                duration_ns,
            });
        }
        self.bump_generation()?;
        self.debug_loop = Some(DebugLoop {
            start_ns,
            end_ns,
            events_enabled,
        });
        self.anchor_cue_position_ns = start_ns;
        self.anchor_clock = now;
        self.operation = Operation::Loop;
        Ok(())
    }

    /// Removes the current debug loop, incrementing generation when changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the generation counter is exhausted.
    pub fn clear_debug_loop(&mut self, now: ClockNs) -> Result<(), TransportError> {
        self.materialize(now);
        if self.debug_loop.take().is_some() {
            self.bump_generation()?;
            self.anchor_clock = now;
            self.operation = Operation::Unloop;
        }
        Ok(())
    }

    /// Re-anchors a continuous timeline after local pipeline clock recovery.
    /// This does not invalidate participants and therefore retains generation.
    pub fn reanchor(&mut self, now: ClockNs) {
        self.materialize(now);
        self.anchor_clock = now;
    }

    /// Records Master EOS. Early EOS is held at the current cue; an on-time
    /// EOS advances through the same boundary logic used by clock projection.
    pub fn master_eos(&mut self, now: ClockNs) -> EosDecision {
        let duration_ns = self.cues[self.cue_index].duration_ns;
        let elapsed = if self.state == TransportState::Playing && now >= self.anchor_clock {
            now.get().saturating_sub(self.anchor_clock.get())
        } else {
            0
        };
        let projected = self.anchor_cue_position_ns.saturating_add(elapsed);
        if projected < duration_ns {
            return EosDecision::HeldEarly {
                remaining_ns: duration_ns - projected,
            };
        }
        self.materialize(now);
        if self.state == TransportState::Paused
            && !self.looping
            && self.cue_index + 1 == self.cues.len()
        {
            EosDecision::ProgramEnded
        } else {
            EosDecision::Advanced
        }
    }

    fn materialize(&mut self, now: ClockNs) {
        if self.state != TransportState::Playing || now < self.anchor_clock {
            return;
        }
        let elapsed = now.get().saturating_sub(self.anchor_clock.get());
        self.anchor_clock = now;
        if let Some(debug_loop) = self.debug_loop {
            let loop_len = debug_loop.end_ns - debug_loop.start_ns;
            let from_start = self
                .anchor_cue_position_ns
                .saturating_sub(debug_loop.start_ns);
            let loop_position =
                (u128::from(from_start) + u128::from(elapsed)) % u128::from(loop_len);
            self.anchor_cue_position_ns = debug_loop.start_ns
                + u64::try_from(loop_position)
                    .expect("a modulo-u64 loop position always fits in u64");
            return;
        }

        let mut remaining = elapsed;
        loop {
            let duration = self.cues[self.cue_index].duration_ns;
            let available = duration.saturating_sub(self.anchor_cue_position_ns);
            if remaining < available {
                self.anchor_cue_position_ns += remaining;
                return;
            }
            remaining -= available;
            self.anchor_cue_position_ns = duration;
            if !self.advance_cue() {
                self.state = TransportState::Paused;
                return;
            }
        }
    }

    fn advance_cue(&mut self) -> bool {
        if self.cue_index + 1 < self.cues.len() {
            self.cue_index += 1;
        } else if self.looping {
            self.cue_index = 0;
            self.playlist_iteration = self.playlist_iteration.saturating_add(1);
        } else {
            return false;
        }
        self.cue_occurrence = self.cue_occurrence.saturating_add(1);
        self.anchor_cue_position_ns = 0;
        true
    }

    fn runtime_state(&self) -> RuntimeState {
        let cue = &self.cues[self.cue_index];
        RuntimeState {
            session: self.session,
            generation: self.generation,
            operation: self.operation,
            program_id: self.program_id.clone(),
            program_hash: self.program_hash,
            cue_index: self.cue_index,
            cue_id: cue.id.clone(),
            cue_occurrence: self.cue_occurrence,
            playlist_iteration: self.playlist_iteration,
            anchor_clock: self.anchor_clock,
            anchor_cue_position: MediaPositionNs::new(self.anchor_cue_position_ns),
            cue_duration: MediaPositionNs::new(cue.duration_ns),
            transport_state: self.state,
            debug_loop: self.debug_loop,
        }
    }

    fn bump_generation(&mut self) -> Result<(), TransportError> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TransportError::GenerationOverflow)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EosDecision {
    Advanced,
    HeldEarly { remaining_ns: u64 },
    ProgramEnded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportError {
    EmptyProgram,
    CueCountMismatch,
    EmptyCueId(usize),
    ZeroDuration(usize),
    UnknownCue(usize),
    PastCueEnd {
        cue_index: usize,
        position_ns: u64,
        duration_ns: u64,
    },
    InvalidDebugLoop {
        start_ns: u64,
        end_ns: u64,
        duration_ns: u64,
    },
    GenerationOverflow,
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for TransportError {}

fn validate_cues(ids: Vec<String>, durations: Vec<u64>) -> Result<Vec<Cue>, TransportError> {
    if ids.is_empty() {
        return Err(TransportError::EmptyProgram);
    }
    if ids.len() != durations.len() {
        return Err(TransportError::CueCountMismatch);
    }
    ids.into_iter()
        .zip(durations)
        .enumerate()
        .map(|(index, (id, duration_ns))| {
            if id.is_empty() {
                return Err(TransportError::EmptyCueId(index));
            }
            if duration_ns == 0 {
                return Err(TransportError::ZeroDuration(index));
            }
            Ok(Cue { id, duration_ns })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(looping: bool) -> TransportController {
        TransportController::new(
            MasterSession::new(1),
            "show".to_owned(),
            ProgramHash::from_bytes([7; 32]),
            vec!["a".to_owned(), "b".to_owned()],
            vec![10, 20],
            looping,
            ClockNs::new(100),
        )
        .unwrap()
    }

    #[test]
    fn projects_exact_boundaries_and_playlist_wrap() {
        let mut transport = controller(true);
        transport.start(ClockNs::new(100));
        let boundary = transport.snapshot(ClockNs::new(110));
        assert_eq!(
            (boundary.cue_index, boundary.anchor_cue_position.get()),
            (1, 0)
        );
        assert_eq!(boundary.cue_occurrence, 1);
        let wrapped = transport.snapshot(ClockNs::new(130));
        assert_eq!(
            (wrapped.cue_index, wrapped.anchor_cue_position.get()),
            (0, 0)
        );
        assert_eq!((wrapped.cue_occurrence, wrapped.playlist_iteration), (2, 1));
    }

    #[test]
    fn non_looping_program_pauses_at_the_final_endpoint() {
        let mut transport = controller(false);
        transport.start(ClockNs::new(5));
        let ended = transport.snapshot(ClockNs::new(35));
        assert_eq!(ended.transport_state, TransportState::Paused);
        assert_eq!((ended.cue_index, ended.anchor_cue_position.get()), (1, 20));
    }

    #[test]
    fn discontinuities_increment_generation_but_boundaries_do_not() {
        let mut transport = controller(true);
        transport.start(ClockNs::new(0));
        assert_eq!(transport.snapshot(ClockNs::new(10)).generation, 1);
        transport.pause(ClockNs::new(11)).unwrap();
        transport.release(ClockNs::new(20)).unwrap();
        transport.goto(1, 2, ClockNs::new(21)).unwrap();
        transport.stop(ClockNs::new(22)).unwrap();
        assert_eq!(transport.generation(), 5);
    }

    #[test]
    fn debug_loop_is_bounded_to_one_cue_and_suppresses_events_by_default() {
        let mut transport = controller(true);
        transport.start(ClockNs::new(0));
        transport
            .set_debug_loop(2, 5, false, ClockNs::new(0))
            .unwrap();
        let state = transport.snapshot(ClockNs::new(7));
        assert_eq!(state.anchor_cue_position.get(), 3);
        assert!(!state.debug_loop.unwrap().events_enabled);
        assert!(
            transport
                .set_debug_loop(8, 11, false, ClockNs::new(7))
                .is_err()
        );
    }

    #[test]
    fn early_master_eos_is_diagnostic_and_does_not_advance_or_freeze_authority() {
        let mut transport = controller(true);
        transport.start(ClockNs::new(100));
        assert_eq!(
            transport.master_eos(ClockNs::new(106)),
            EosDecision::HeldEarly { remaining_ns: 4 }
        );
        let state = transport.snapshot(ClockNs::new(110));
        assert_eq!(state.cue_index, 1);
        assert_eq!(state.transport_state, TransportState::Playing);
    }

    #[test]
    fn loading_a_program_stops_it_and_increments_generation() {
        let mut transport = controller(true);
        transport
            .load_program(
                "replacement".to_owned(),
                ProgramHash::from_bytes([8; 32]),
                vec!["only".to_owned()],
                vec![50],
                false,
                ClockNs::new(200),
            )
            .unwrap();
        let state = transport.snapshot(ClockNs::new(300));
        assert_eq!(state.generation, 2);
        assert_eq!(state.program_id, "replacement");
        assert_eq!(state.cue_id, "only");
        assert_eq!(state.operation, Operation::Load);
        assert_eq!(state.transport_state, TransportState::Paused);
    }
}
