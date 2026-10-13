//! Master-side aggregation of periodic playback observations.

use crate::protocol::DiagnosticTelemetry;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct DiagnosticSummary {
    pub player_id: String,
    pub samples: u64,
    pub valid_samples: u64,
    pub latest_error_ns: i64,
    pub minimum_error_ns: i64,
    pub maximum_error_ns: i64,
    pub mean_error_ns: f64,
    pub rms_error_ns: f64,
    /// Error slope relative to shared-clock elapsed time. 1000 ppm is 0.1%.
    pub rate_error_ppm: Option<f64>,
    pub disconnect_count: u64,
    pub recovery_count: u64,
    pub pipeline_playing: bool,
    pub transition_in_progress: bool,
    pub clock_healthy: bool,
}

#[derive(Debug)]
struct Series {
    generation: u64,
    samples: u64,
    valid_samples: u64,
    rate_samples: u64,
    sum: f64,
    sum_squares: f64,
    minimum: i64,
    maximum: i64,
    first_clock_ns: u64,
    first_error_ns: i64,
    latest: DiagnosticTelemetry,
}

#[allow(clippy::cast_precision_loss)]
impl Series {
    fn new(sample: DiagnosticTelemetry) -> Self {
        let valid = Self::sample_is_valid(sample);
        let mut series = Self {
            generation: sample.generation,
            samples: 1,
            valid_samples: 0,
            rate_samples: 0,
            sum: 0.0,
            sum_squares: 0.0,
            minimum: i64::MAX,
            maximum: i64::MIN,
            first_clock_ns: sample.clock_time.get(),
            first_error_ns: sample.error_ns,
            latest: sample,
        };
        if valid {
            series.record_valid_sample(sample);
        }
        series
    }

    fn record(&mut self, sample: DiagnosticTelemetry) {
        let discontinuity = sample.generation != self.generation
            || sample.disconnect_count != self.latest.disconnect_count
            || sample.recovery_count != self.latest.recovery_count
            || !Self::sample_is_valid(sample);
        if discontinuity {
            self.generation = sample.generation;
            self.rate_samples = 0;
        }
        self.samples = self.samples.saturating_add(1);
        self.latest = sample;
        if Self::sample_is_valid(sample) {
            self.record_valid_sample(sample);
        }
    }

    fn sample_is_valid(sample: DiagnosticTelemetry) -> bool {
        sample.pipeline_playing && !sample.transition_in_progress && sample.clock_healthy
    }

    fn record_valid_sample(&mut self, sample: DiagnosticTelemetry) {
        if self.rate_samples == 0 {
            self.first_clock_ns = sample.clock_time.get();
            self.first_error_ns = sample.error_ns;
        }
        self.valid_samples = self.valid_samples.saturating_add(1);
        self.rate_samples = self.rate_samples.saturating_add(1);
        self.sum += sample.error_ns as f64;
        self.sum_squares += (sample.error_ns as f64).powi(2);
        self.minimum = self.minimum.min(sample.error_ns);
        self.maximum = self.maximum.max(sample.error_ns);
    }

    fn summary(&self, player_id: String) -> DiagnosticSummary {
        let elapsed = self
            .latest
            .clock_time
            .get()
            .saturating_sub(self.first_clock_ns);
        let rate_error_ppm = (elapsed > 0 && self.rate_samples > 1).then(|| {
            (self.latest.error_ns - self.first_error_ns) as f64 / elapsed as f64 * 1_000_000.0
        });
        DiagnosticSummary {
            player_id,
            samples: self.samples,
            valid_samples: self.valid_samples,
            latest_error_ns: self.latest.error_ns,
            minimum_error_ns: if self.valid_samples > 0 {
                self.minimum
            } else {
                0
            },
            maximum_error_ns: if self.valid_samples > 0 {
                self.maximum
            } else {
                0
            },
            mean_error_ns: if self.valid_samples > 0 {
                self.sum / self.valid_samples as f64
            } else {
                0.0
            },
            rms_error_ns: if self.valid_samples > 0 {
                (self.sum_squares / self.valid_samples as f64).sqrt()
            } else {
                0.0
            },
            rate_error_ppm,
            disconnect_count: self.latest.disconnect_count,
            recovery_count: self.latest.recovery_count,
            pipeline_playing: self.latest.pipeline_playing,
            transition_in_progress: self.latest.transition_in_progress,
            clock_healthy: self.latest.clock_healthy,
        }
    }
}

#[derive(Debug)]
struct State {
    series: BTreeMap<String, Series>,
    log: Option<File>,
}

#[derive(Debug)]
pub struct DiagnosticsHub {
    state: Mutex<State>,
}

impl DiagnosticsHub {
    /// Creates an aggregator and optionally appends samples to a CSV file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the log cannot be opened or initialized.
    pub fn new(log_path: Option<&Path>) -> io::Result<Self> {
        let log = if let Some(path) = log_path {
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            if file.metadata()?.len() == 0 {
                writeln!(
                    file,
                    "kind,player,clock_ns,generation,expected_position_ns,actual_position_ns,error_ns,cue_index,cue_position_ns,pipeline_playing,transition,clock_healthy,disconnects,recoveries,detail"
                )?;
            }
            Some(file)
        } else {
            None
        };
        Ok(Self {
            state: Mutex::new(State {
                series: BTreeMap::new(),
                log,
            }),
        })
    }

    /// Records one observation. Logging is flushed so a crash does not erase a long test run.
    ///
    /// # Errors
    ///
    /// Returns an error if the aggregator lock is poisoned or the log write fails.
    pub fn record(&self, player_id: &str, sample: DiagnosticTelemetry) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("diagnostics lock poisoned"))?;
        state
            .series
            .entry(player_id.to_owned())
            .and_modify(|series| series.record(sample))
            .or_insert_with(|| Series::new(sample));
        if let Some(log) = &mut state.log {
            let player = player_id.replace('"', "\"\"");
            writeln!(
                log,
                "sample,\"{}\",{},{},{},{},{},{},{},{},{},{},{},{},",
                player,
                sample.clock_time.get(),
                sample.generation,
                sample.expected_position.get(),
                sample.actual_position.get(),
                sample.error_ns,
                sample.cue_index,
                sample.cue_position.get(),
                sample.pipeline_playing,
                sample.transition_in_progress,
                sample.clock_healthy,
                sample.disconnect_count,
                sample.recovery_count
            )?;
            log.flush()?;
        }
        Ok(())
    }

    /// Appends a connection or recovery lifecycle event to the optional CSV log.
    ///
    /// # Errors
    ///
    /// Returns an error if the aggregator lock is poisoned or the log write fails.
    pub fn record_event(
        &self,
        player_id: &str,
        clock_ns: u64,
        kind: &str,
        detail: &str,
    ) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("diagnostics lock poisoned"))?;
        if let Some(log) = &mut state.log {
            let player = player_id.replace('"', "\"\"");
            let kind = kind.replace('"', "\"\"");
            let detail = detail.replace('"', "\"\"");
            writeln!(
                log,
                "\"{kind}\",\"{player}\",{clock_ns},,,,,,,,,,,,\"{detail}\""
            )?;
            log.flush()?;
        }
        Ok(())
    }

    /// Returns one statistical summary per player.
    ///
    /// # Errors
    ///
    /// Returns an error if the aggregator lock is poisoned.
    pub fn summaries(&self) -> io::Result<Vec<DiagnosticSummary>> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("diagnostics lock poisoned"))?;
        Ok(state
            .series
            .iter()
            .map(|(player, series)| series.summary(player.clone()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{ClockNs, MediaPositionNs};

    fn sample(clock: u64, error: i64) -> DiagnosticTelemetry {
        DiagnosticTelemetry {
            clock_time: ClockNs::new(clock),
            generation: 1,
            expected_position: MediaPositionNs::new(clock),
            actual_position: MediaPositionNs::new(clock.saturating_add_signed(error)),
            error_ns: error,
            cue_index: 0,
            cue_position: MediaPositionNs::new(clock),
            pipeline_playing: true,
            transition_in_progress: false,
            clock_healthy: true,
            disconnect_count: 0,
            recovery_count: 0,
        }
    }

    #[test]
    fn derives_rate_error_from_error_slope() {
        let hub = DiagnosticsHub::new(None).unwrap();
        hub.record("slow", sample(1_000_000_000, 0)).unwrap();
        hub.record("slow", sample(11_000_000_000, -10_000_000))
            .unwrap();
        let summary = hub.summaries().unwrap().remove(0);
        assert!((summary.rate_error_ppm.unwrap() + 1000.0).abs() < 0.001);
    }

    #[test]
    fn resets_rate_window_across_recovery() {
        let hub = DiagnosticsHub::new(None).unwrap();
        hub.record("left", sample(1, 10)).unwrap();
        let mut recovered = sample(2, 0);
        recovered.recovery_count = 1;
        hub.record("left", recovered).unwrap();
        let summary = hub.summaries().unwrap().remove(0);
        assert_eq!(summary.samples, 2);
        assert!(summary.rate_error_ppm.is_none());
    }

    #[test]
    fn transition_samples_do_not_pollute_statistics_or_rate() {
        let hub = DiagnosticsHub::new(None).unwrap();
        hub.record("left", sample(1_000_000_000, 10)).unwrap();

        let mut transition = sample(2_000_000_000, 10_000_000_000);
        transition.transition_in_progress = true;
        hub.record("left", transition).unwrap();

        hub.record("left", sample(3_000_000_000, 20)).unwrap();
        let summary = hub.summaries().unwrap().remove(0);
        assert_eq!(summary.samples, 3);
        assert_eq!(summary.valid_samples, 2);
        assert_eq!(summary.minimum_error_ns, 10);
        assert_eq!(summary.maximum_error_ns, 20);
        assert_eq!(summary.mean_error_ns, 15.0);
        assert!(summary.rate_error_ppm.is_none());

        hub.record("left", sample(4_000_000_000, 1_000_020))
            .unwrap();
        let summary = hub.summaries().unwrap().remove(0);
        assert_eq!(summary.samples, 4);
        assert_eq!(summary.valid_samples, 3);
        assert!((summary.rate_error_ppm.unwrap() - 1000.0).abs() < 0.001);
    }
}
