//! Sequence timeline calculations for resolved logical program cues.

use std::fmt;

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

    /// Converts an item-local position into the canonical sequence position.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown item or a local position at/beyond that
    /// item's duration. Exact endpoints cannot reliably identify which side
    /// of a playlist handoff is currently rendering.
    pub fn sequence_position(&self, position: ItemPosition) -> Result<u64, TimelineError> {
        let duration_ns = self
            .durations_ns
            .get(position.index)
            .copied()
            .ok_or(TimelineError::UnknownItem(position.index))?;
        if position.local_ns >= duration_ns {
            return Err(TimelineError::PastItemEnd {
                index: position.index,
                local_ns: position.local_ns,
                duration_ns,
            });
        }
        self.offsets_ns[position.index]
            .checked_add(position.local_ns)
            .ok_or(TimelineError::DurationOverflow)
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

    /// Advances an absolute sequence position while preserving loop occurrence.
    ///
    /// Unlike [`Self::project`], a looping result is not reduced modulo the
    /// playlist duration. This makes repeated cues distinguishable in state
    /// snapshots and release plans.
    ///
    /// # Errors
    ///
    /// Returns an error on arithmetic overflow or when a non-looping sequence
    /// reaches its endpoint.
    pub fn project_absolute(&self, position_ns: u64, lead_ns: u64) -> Result<u64, TimelineError> {
        let projected = position_ns
            .checked_add(lead_ns)
            .ok_or(TimelineError::DurationOverflow)?;
        if !self.looping && projected >= self.total_ns {
            return Err(TimelineError::PastEnd {
                position_ns: projected,
                total_ns: self.total_ns,
            });
        }
        Ok(projected)
    }

    #[must_use]
    pub const fn playlist_iteration(&self, absolute_position_ns: u64) -> u64 {
        if self.looping {
            absolute_position_ns / self.total_ns
        } else {
            0
        }
    }

    /// Maps an authoritative cue occurrence onto this host's local timeline.
    ///
    /// # Errors
    ///
    /// Returns an error when the item-local position is invalid, a non-looping
    /// timeline is given a non-zero occurrence, or arithmetic overflows.
    pub fn absolute_position(
        &self,
        position: ItemPosition,
        playlist_iteration: u64,
    ) -> Result<u64, TimelineError> {
        let position_in_iteration = self.sequence_position(position)?;
        if !self.looping {
            if playlist_iteration != 0 {
                return Err(TimelineError::UnexpectedIteration(playlist_iteration));
            }
            return Ok(position_in_iteration);
        }
        playlist_iteration
            .checked_mul(self.total_ns)
            .and_then(|offset| offset.checked_add(position_in_iteration))
            .ok_or(TimelineError::DurationOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineError {
    Empty,
    ZeroDuration(usize),
    DurationOverflow,
    UnknownItem(usize),
    UnexpectedIteration(u64),
    PastItemEnd {
        index: usize,
        local_ns: u64,
        duration_ns: u64,
    },
    PastEnd {
        position_ns: u64,
        total_ns: u64,
    },
}

impl fmt::Display for TimelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("media sequence is empty"),
            Self::ZeroDuration(index) => write!(formatter, "media item {index} has zero duration"),
            Self::DurationOverflow => {
                formatter.write_str("media sequence duration exceeds u64 nanoseconds")
            }
            Self::UnknownItem(index) => write!(formatter, "media item {index} does not exist"),
            Self::UnexpectedIteration(iteration) => write!(
                formatter,
                "non-looping timeline cannot use playlist iteration {iteration}"
            ),
            Self::PastItemEnd {
                index,
                local_ns,
                duration_ns,
            } => write!(
                formatter,
                "item {index} position {local_ns} is at or beyond duration {duration_ns}"
            ),
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
        assert_eq!(timeline.project_absolute(29, 2).unwrap(), 31);
        assert_eq!(timeline.playlist_iteration(61), 2);
    }

    #[test]
    fn timeline_converts_item_positions_and_rejects_ambiguous_endpoints() {
        let timeline = Timeline::new(vec![10, 20], true).unwrap();
        assert_eq!(
            timeline
                .sequence_position(ItemPosition {
                    index: 1,
                    local_ns: 3,
                })
                .unwrap(),
            13
        );
        assert!(matches!(
            timeline.sequence_position(ItemPosition {
                index: 0,
                local_ns: 10,
            }),
            Err(TimelineError::PastItemEnd { .. })
        ));
        assert_eq!(
            timeline.sequence_position(ItemPosition {
                index: 2,
                local_ns: 0,
            }),
            Err(TimelineError::UnknownItem(2))
        );
        assert_eq!(
            timeline
                .absolute_position(
                    ItemPosition {
                        index: 1,
                        local_ns: 3,
                    },
                    2,
                )
                .unwrap(),
            73
        );
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
