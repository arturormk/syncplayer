//! Active Slave participant registry.

use crate::protocol::{Capabilities, Subscription};
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    Subscribed,
    ClockSynchronized,
    Ready,
}

#[derive(Clone, Debug)]
pub struct ParticipantSnapshot {
    pub player_id: String,
    pub remote_address: SocketAddr,
    pub state_endpoint: StateEndpoint,
    pub software_version: String,
    pub capabilities: Capabilities,
    pub cue_durations_ns: Vec<u64>,
    pub readiness: Readiness,
    pub clock_synchronized: bool,
    pub last_seen_age: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateEndpoint {
    ControlConnection,
    Udp(SocketAddr),
}

#[derive(Debug)]
pub struct ParticipantRegistry {
    entries: Mutex<HashMap<String, Participant>>,
    next_token: AtomicU64,
}

#[derive(Debug)]
struct Participant {
    token: u64,
    remote_address: SocketAddr,
    state_endpoint: StateEndpoint,
    software_version: String,
    capabilities: Capabilities,
    cue_durations_ns: Vec<u64>,
    readiness: Readiness,
    clock_synchronized: bool,
    last_seen: Instant,
}

#[derive(Debug)]
pub struct ParticipantLease {
    registry: Arc<ParticipantRegistry>,
    player_id: String,
    token: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    DuplicatePlayer(String),
    LockPoisoned,
    ParticipantExpired(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePlayer(player) => {
                write!(formatter, "player ID '{player}' is already connected")
            }
            Self::LockPoisoned => formatter.write_str("participant registry lock poisoned"),
            Self::ParticipantExpired(player) => {
                write!(formatter, "participant '{player}' is no longer registered")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

impl ParticipantRegistry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            next_token: AtomicU64::new(1),
        })
    }

    /// Registers an active participant and returns its connection-scoped lease.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate non-stale player or poisoned registry.
    pub fn register(
        self: &Arc<Self>,
        subscription: &Subscription,
        remote_address: SocketAddr,
        now: Instant,
        stale_after: Duration,
    ) -> Result<ParticipantLease, RegistryError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| RegistryError::LockPoisoned)?;
        entries.retain(|_, participant| now.duration_since(participant.last_seen) <= stale_after);
        if entries.contains_key(&subscription.player_id) {
            return Err(RegistryError::DuplicatePlayer(
                subscription.player_id.clone(),
            ));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let state_endpoint = if subscription.state_port == 0 {
            StateEndpoint::ControlConnection
        } else {
            StateEndpoint::Udp(SocketAddr::new(
                remote_address.ip(),
                subscription.state_port,
            ))
        };
        entries.insert(
            subscription.player_id.clone(),
            Participant {
                token,
                remote_address,
                state_endpoint,
                software_version: subscription.software_version.clone(),
                capabilities: subscription.capabilities,
                cue_durations_ns: subscription.cue_durations_ns.clone(),
                readiness: Readiness::Subscribed,
                clock_synchronized: false,
                last_seen: now,
            },
        );
        Ok(ParticipantLease {
            registry: self.clone(),
            player_id: subscription.player_id.clone(),
            token,
        })
    }

    /// Returns current participant status, expiring entries beyond the liveness limit.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lock is poisoned.
    pub fn snapshot(
        &self,
        now: Instant,
        stale_after: Duration,
    ) -> Result<Vec<ParticipantSnapshot>, RegistryError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| RegistryError::LockPoisoned)?;
        entries.retain(|_, participant| now.duration_since(participant.last_seen) <= stale_after);
        let mut snapshot = entries
            .iter()
            .map(|(player_id, participant)| ParticipantSnapshot {
                player_id: player_id.clone(),
                remote_address: participant.remote_address,
                state_endpoint: participant.state_endpoint,
                software_version: participant.software_version.clone(),
                capabilities: participant.capabilities,
                cue_durations_ns: participant.cue_durations_ns.clone(),
                readiness: participant.readiness,
                clock_synchronized: participant.clock_synchronized,
                last_seen_age: now.duration_since(participant.last_seen),
            })
            .collect::<Vec<_>>();
        snapshot.sort_by(|left, right| left.player_id.cmp(&right.player_id));
        Ok(snapshot)
    }

    fn update(
        &self,
        player_id: &str,
        token: u64,
        now: Instant,
        readiness: Option<Readiness>,
    ) -> Result<(), RegistryError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| RegistryError::LockPoisoned)?;
        let participant = entries
            .get_mut(player_id)
            .filter(|participant| participant.token == token)
            .ok_or_else(|| RegistryError::ParticipantExpired(player_id.to_owned()))?;
        participant.last_seen = now;
        if let Some(readiness) = readiness {
            participant.readiness = readiness;
            participant.clock_synchronized = readiness != Readiness::Subscribed;
        }
        Ok(())
    }

    fn remove(&self, player_id: &str, token: u64) {
        if let Ok(mut entries) = self.entries.lock()
            && entries
                .get(player_id)
                .is_some_and(|participant| participant.token == token)
        {
            entries.remove(player_id);
        }
    }
}

impl ParticipantLease {
    #[must_use]
    pub fn player_id(&self) -> &str {
        &self.player_id
    }

    /// Marks the participant's network clock as synchronized.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease expired or the registry lock is poisoned.
    pub fn mark_clock_synchronized(&self) -> Result<(), RegistryError> {
        self.registry.update(
            &self.player_id,
            self.token,
            Instant::now(),
            Some(Readiness::ClockSynchronized),
        )
    }

    /// Marks the participant prepared for authoritative playback.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease expired or the registry lock is poisoned.
    pub fn mark_ready(&self) -> Result<(), RegistryError> {
        self.registry.update(
            &self.player_id,
            self.token,
            Instant::now(),
            Some(Readiness::Ready),
        )
    }

    /// Refreshes the participant liveness timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease expired or the registry lock is poisoned.
    pub fn mark_seen(&self) -> Result<(), RegistryError> {
        self.registry
            .update(&self.player_id, self.token, Instant::now(), None)
    }
}

impl Drop for ParticipantLease {
    fn drop(&mut self) {
        self.registry.remove(&self.player_id, self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::ProgramHash;
    use std::net::IpAddr;

    fn subscription(player_id: &str) -> Subscription {
        Subscription {
            player_id: player_id.to_owned(),
            program_id: "show".to_owned(),
            program_hash: ProgramHash::from_bytes([1; 32]),
            software_version: "0.1.0".to_owned(),
            capabilities: Capabilities::current(),
            state_port: 0,
            cue_durations_ns: vec![10, 20],
        }
    }

    #[test]
    fn rejects_duplicates_and_releases_identity_on_drop() {
        let registry = ParticipantRegistry::new();
        let now = Instant::now();
        let first = registry
            .register(
                &subscription("left"),
                "127.0.0.1:1000".parse().unwrap(),
                now,
                Duration::from_secs(8),
            )
            .unwrap();
        assert!(matches!(
            registry.register(
                &subscription("left"),
                "127.0.0.1:1001".parse().unwrap(),
                now,
                Duration::from_secs(8)
            ),
            Err(RegistryError::DuplicatePlayer(_))
        ));
        drop(first);
        assert!(
            registry
                .register(
                    &subscription("left"),
                    "127.0.0.1:1001".parse().unwrap(),
                    now,
                    Duration::from_secs(8)
                )
                .is_ok()
        );
    }

    #[test]
    fn expires_stale_entries_and_tracks_readiness() {
        let registry = ParticipantRegistry::new();
        let now = Instant::now();
        let lease = registry
            .register(
                &subscription("left"),
                "127.0.0.1:1000".parse().unwrap(),
                now,
                Duration::from_secs(8),
            )
            .unwrap();
        lease.mark_clock_synchronized().unwrap();
        lease.mark_ready().unwrap();
        let current = registry
            .snapshot(Instant::now(), Duration::from_secs(8))
            .unwrap();
        assert_eq!(current[0].readiness, Readiness::Ready);
        assert!(current[0].clock_synchronized);

        let expired = registry
            .snapshot(now + Duration::from_secs(9), Duration::from_secs(8))
            .unwrap();
        assert!(expired.is_empty());
        assert!(matches!(
            lease.mark_seen(),
            Err(RegistryError::ParticipantExpired(_))
        ));
    }

    #[test]
    fn records_udp_state_endpoint() {
        let registry = ParticipantRegistry::new();
        let mut details = subscription("left");
        details.state_port = 5_000;
        let _lease = registry
            .register(
                &details,
                "192.0.2.10:1000".parse().unwrap(),
                Instant::now(),
                Duration::from_secs(8),
            )
            .unwrap();
        let snapshot = registry
            .snapshot(Instant::now(), Duration::from_secs(8))
            .unwrap();
        assert_eq!(
            snapshot[0].state_endpoint,
            StateEndpoint::Udp(SocketAddr::new(
                "192.0.2.10".parse::<IpAddr>().unwrap(),
                5_000
            ))
        );
    }
}
