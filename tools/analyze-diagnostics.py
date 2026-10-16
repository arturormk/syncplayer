#!/usr/bin/env python3
"""Summarize Syncplayer's centralized diagnostics CSV using only the standard library."""

from __future__ import annotations

import argparse
import bisect
import csv
import math
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


REQUIRED_COLUMNS = {
    "kind",
    "player",
    "clock_ns",
    "generation",
    "error_ns",
    "cue_index",
    "cue_position_ns",
    "pipeline_playing",
    "transition",
    "clock_healthy",
    "disconnects",
    "recoveries",
}
DEFAULT_THRESHOLDS_MS = (40.0, 50.0, 100.0, 250.0)


@dataclass(frozen=True)
class Sample:
    row_number: int
    player: str
    clock_ns: int
    generation: int
    error_ns: int
    cue_index: int
    cue_position_ns: int
    pipeline_playing: bool
    transition: bool
    clock_healthy: bool
    disconnects: int
    recoveries: int
    seconds_since_transition: float | None = None
    seconds_since_discontinuity: float | None = None

    @property
    def valid(self) -> bool:
        """Match the validity rule used by src/diagnostics.rs."""
        return self.pipeline_playing and not self.transition and self.clock_healthy


@dataclass(frozen=True)
class Event:
    row_number: int
    kind: str
    player: str
    clock_ns: int
    detail: str


@dataclass(frozen=True)
class PairSample:
    clock_ns: int
    error_ns: float
    left_row: int
    right_before_row: int
    right_after_row: int
    settled: bool


def parse_bool(value: str, row_number: int, column: str) -> bool:
    lowered = value.strip().lower()
    if lowered == "true":
        return True
    if lowered == "false":
        return False
    raise ValueError(f"row {row_number}: {column} must be true or false, got {value!r}")


def parse_int(value: str, row_number: int, column: str) -> int:
    try:
        return int(value)
    except ValueError as error:
        raise ValueError(f"row {row_number}: invalid {column} value {value!r}") from error


def read_csv(path: Path) -> tuple[dict[str, list[Sample]], list[Event]]:
    samples: dict[str, list[Sample]] = defaultdict(list)
    events: list[Event] = []
    last_transition_ns: dict[str, int] = {}
    last_discontinuity_ns: dict[str, int] = {}
    previous_sample: dict[str, Sample] = {}

    with path.open(newline="", encoding="utf-8") as handle:
        reader = csv.DictReader(handle)
        if reader.fieldnames is None:
            raise ValueError("CSV has no header")
        missing = REQUIRED_COLUMNS.difference(reader.fieldnames)
        if missing:
            raise ValueError(f"CSV is missing columns: {', '.join(sorted(missing))}")

        for row_number, row in enumerate(reader, start=2):
            kind = (row.get("kind") or "").strip()
            player = (row.get("player") or "").strip()
            if not kind:
                continue
            if not player:
                raise ValueError(f"row {row_number}: player is empty")

            clock_ns = parse_int(row.get("clock_ns") or "", row_number, "clock_ns")
            if kind != "sample":
                events.append(
                    Event(
                        row_number=row_number,
                        kind=kind,
                        player=player,
                        clock_ns=clock_ns,
                        detail=(row.get("detail") or "").strip(),
                    )
                )
                last_discontinuity_ns[player] = clock_ns
                continue

            transition = parse_bool(row.get("transition") or "", row_number, "transition")
            if transition:
                last_transition_ns[player] = clock_ns
            transition_clock = last_transition_ns.get(player)
            seconds_since_transition = (
                None
                if transition_clock is None or clock_ns < transition_clock
                else (clock_ns - transition_clock) / 1_000_000_000
            )
            generation = parse_int(row.get("generation") or "", row_number, "generation")
            disconnects = parse_int(
                row.get("disconnects") or "", row_number, "disconnects"
            )
            recoveries = parse_int(row.get("recoveries") or "", row_number, "recoveries")
            pipeline_playing = parse_bool(
                row.get("pipeline_playing") or "", row_number, "pipeline_playing"
            )
            clock_healthy = parse_bool(
                row.get("clock_healthy") or "", row_number, "clock_healthy"
            )
            previous = previous_sample.get(player)
            discontinuity = (
                not pipeline_playing
                or transition
                or not clock_healthy
                or (
                    previous is not None
                    and (
                        generation != previous.generation
                        or disconnects != previous.disconnects
                        or recoveries != previous.recoveries
                    )
                )
            )
            if discontinuity:
                last_discontinuity_ns[player] = clock_ns
            discontinuity_clock = last_discontinuity_ns.get(player)
            seconds_since_discontinuity = (
                None
                if discontinuity_clock is None or clock_ns < discontinuity_clock
                else (clock_ns - discontinuity_clock) / 1_000_000_000
            )
            sample = Sample(
                row_number=row_number,
                player=player,
                clock_ns=clock_ns,
                generation=generation,
                error_ns=parse_int(row.get("error_ns") or "", row_number, "error_ns"),
                cue_index=parse_int(row.get("cue_index") or "", row_number, "cue_index"),
                cue_position_ns=parse_int(
                    row.get("cue_position_ns") or "", row_number, "cue_position_ns"
                ),
                pipeline_playing=pipeline_playing,
                transition=transition,
                clock_healthy=clock_healthy,
                disconnects=disconnects,
                recoveries=recoveries,
                seconds_since_transition=seconds_since_transition,
                seconds_since_discontinuity=seconds_since_discontinuity,
            )
            samples[player].append(sample)
            previous_sample[player] = sample

    return dict(samples), events


def percentile(sorted_values: list[float], percentage: float) -> float:
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return sorted_values[0]
    position = (len(sorted_values) - 1) * percentage / 100.0
    lower = math.floor(position)
    upper = math.ceil(position)
    fraction = position - lower
    return sorted_values[lower] * (1.0 - fraction) + sorted_values[upper] * fraction


def current_rate_ppm(samples: list[Sample]) -> tuple[float | None, int]:
    """Reproduce the console's rate window reset behavior."""
    first: Sample | None = None
    previous: Sample | None = None
    count = 0
    for sample in samples:
        discontinuity = (
            previous is not None
            and (
                sample.generation != previous.generation
                or sample.disconnects != previous.disconnects
                or sample.recoveries != previous.recoveries
                or not sample.valid
            )
        )
        if discontinuity or not sample.valid:
            first = None
            count = 0
        if sample.valid:
            if first is None:
                first = sample
            count += 1
        previous = sample

    if first is None or previous is None or count < 2 or not previous.valid:
        return None, count
    elapsed_ns = previous.clock_ns - first.clock_ns
    if elapsed_ns <= 0:
        return None, count
    rate_ppm = (previous.error_ns - first.error_ns) / elapsed_ns * 1_000_000.0
    return rate_ppm, count


def format_ms(nanoseconds: float) -> str:
    return f"{nanoseconds / 1_000_000:+.3f}ms"


def format_duration(seconds: float) -> str:
    hours, remainder = divmod(seconds, 3600)
    minutes, seconds = divmod(remainder, 60)
    return f"{int(hours)}h {int(minutes):02d}m {seconds:06.3f}s"


def recovery_summary(samples: list[Sample]) -> str:
    first_by_attempt: dict[int, Sample] = {}
    for sample in samples:
        first_by_attempt.setdefault(sample.recoveries, sample)
    attempts = sorted(attempt for attempt in first_by_attempt if attempt > 0)
    corrective = [attempt for attempt in attempts if attempt > 1]
    if not corrective:
        return "recoveries=0 (initial synchronization excluded)"

    corrective_times = [first_by_attempt[attempt].clock_ns for attempt in corrective]
    if len(corrective_times) >= 2:
        intervals = [
            (later - earlier) / 1_000_000_000
            for earlier, later in zip(corrective_times, corrective_times[1:])
        ]
        mean_interval = math.fsum(intervals) / len(intervals)
        interval_text = f"mean interval={format_duration(mean_interval)}"
    else:
        interval_text = "mean interval=unavailable (only one recovery)"

    initial = first_by_attempt.get(1)
    if initial is not None:
        first_delay = (corrective_times[0] - initial.clock_ns) / 1_000_000_000
        first_text = f"first recovery {format_duration(first_delay)} after initial sync"
    else:
        first_text = "initial synchronization was not present in this CSV"
    return (
        f"recoveries={len(corrective)} (initial synchronization excluded); "
        f"{interval_text}; {first_text} "
        "(timestamps are first subsequent telemetry samples)"
    )


def print_population(
    label: str,
    population: list[Sample] | list[PairSample],
    thresholds_ms: Iterable[float],
) -> None:
    if not population:
        print(f"  {label}: no samples")
        return
    errors = [float(sample.error_ns) for sample in population]
    absolute = sorted(abs(error) for error in errors)
    mean = math.fsum(errors) / len(errors)
    absolute_mean = math.fsum(absolute) / len(absolute)
    rms = math.sqrt(math.fsum(error * error for error in errors) / len(errors))
    variance = math.fsum((error - mean) ** 2 for error in errors) / len(errors)

    print(
        f"  {label}: n={len(errors)} mean={format_ms(mean)} "
        f"mean_abs={absolute_mean / 1_000_000:.3f}ms "
        f"stddev={math.sqrt(variance) / 1_000_000:.3f}ms "
        f"rms={rms / 1_000_000:.3f}ms"
    )
    print(
        f"    signed range={format_ms(min(errors))}..{format_ms(max(errors))}; "
        "absolute percentiles "
        f"p50={percentile(absolute, 50) / 1_000_000:.3f}ms "
        f"p90={percentile(absolute, 90) / 1_000_000:.3f}ms "
        f"p95={percentile(absolute, 95) / 1_000_000:.3f}ms "
        f"p99={percentile(absolute, 99) / 1_000_000:.3f}ms "
        f"p99.9={percentile(absolute, 99.9) / 1_000_000:.3f}ms"
    )
    threshold_parts = []
    for threshold_ms in thresholds_ms:
        count = sum(value > threshold_ms * 1_000_000 for value in absolute)
        threshold_parts.append(
            f">{threshold_ms:g}ms={count} ({count / len(absolute) * 100:.3f}%)"
        )
    print(f"    absolute excursions: {', '.join(threshold_parts)}")


def pair_samples(
    left: list[Sample],
    right: list[Sample],
    max_gap_seconds: float,
    settle_seconds: float,
) -> list[PairSample]:
    """Estimate left-minus-right playback separation at common clock times."""
    left_valid = [sample for sample in left if sample.valid]
    right_valid = [sample for sample in right if sample.valid]
    right_clocks = [sample.clock_ns for sample in right_valid]
    max_gap_ns = max_gap_seconds * 1_000_000_000
    pairs: list[PairSample] = []

    for left_sample in left_valid:
        after_index = bisect.bisect_left(right_clocks, left_sample.clock_ns)
        if after_index == 0 or after_index == len(right_valid):
            continue
        before = right_valid[after_index - 1]
        after = right_valid[after_index]
        span_ns = after.clock_ns - before.clock_ns
        if span_ns <= 0 or span_ns > max_gap_ns:
            continue
        if left_sample.clock_ns - before.clock_ns > max_gap_ns:
            continue
        if after.clock_ns - left_sample.clock_ns > max_gap_ns:
            continue
        if not (
            left_sample.generation == before.generation == after.generation
            and before.disconnects == after.disconnects
            and before.recoveries == after.recoveries
        ):
            continue

        fraction = (left_sample.clock_ns - before.clock_ns) / span_ns
        right_error_ns = before.error_ns + fraction * (after.error_ns - before.error_ns)
        settled = all(
            sample.seconds_since_discontinuity is None
            or sample.seconds_since_discontinuity >= settle_seconds
            for sample in (left_sample, before, after)
        )
        pairs.append(
            PairSample(
                clock_ns=left_sample.clock_ns,
                error_ns=left_sample.error_ns - right_error_ns,
                left_row=left_sample.row_number,
                right_before_row=before.row_number,
                right_after_row=after.row_number,
                settled=settled,
            )
        )
    return pairs


def print_pairwise_report(
    samples_by_player: dict[str, list[Sample]],
    thresholds_ms: tuple[float, ...],
    top_count: int,
    settle_seconds: float,
    max_gap_seconds: float,
) -> None:
    players = sorted(samples_by_player)
    if len(players) < 2:
        return
    print("\nPairwise clock-aligned separation")
    print(
        "  Positive means the first named player is ahead of the second. "
        f"Interpolation requires neighboring samples no more than {max_gap_seconds:g}s apart."
    )
    for left_index, left_player in enumerate(players):
        for right_player in players[left_index + 1 :]:
            pairs = pair_samples(
                samples_by_player[left_player],
                samples_by_player[right_player],
                max_gap_seconds,
                settle_seconds,
            )
            print(f"\n  {left_player} - {right_player}")
            print_population("aligned valid samples", pairs, thresholds_ms)
            if settle_seconds > 0:
                print_population(
                    f"settled aligned samples (at least {settle_seconds:g}s)",
                    [pair for pair in pairs if pair.settled],
                    thresholds_ms,
                )
            if top_count > 0 and pairs:
                print(f"    worst {min(top_count, len(pairs))} aligned estimates:")
                for pair in sorted(
                    pairs, key=lambda item: abs(item.error_ns), reverse=True
                )[:top_count]:
                    print(
                        f"      clock={pair.clock_ns / 1e9:.3f}s "
                        f"separation={format_ms(pair.error_ns)} "
                        f"rows={pair.left_row},{pair.right_before_row}-{pair.right_after_row}"
                    )


def print_report(
    path: Path,
    samples_by_player: dict[str, list[Sample]],
    events: list[Event],
    thresholds_ms: tuple[float, ...],
    top_count: int,
    settle_seconds: float,
    pair_max_gap_seconds: float,
) -> None:
    sample_count = sum(len(samples) for samples in samples_by_player.values())
    print(f"Syncplayer diagnostics: {path}")
    print(
        f"players={len(samples_by_player)} samples={sample_count} lifecycle_events={len(events)}"
    )

    event_counts = Counter((event.player, event.kind) for event in events)
    for player in sorted(samples_by_player):
        samples = samples_by_player[player]
        valid = [sample for sample in samples if sample.valid]
        invalid = len(samples) - len(valid)
        transitions = sum(sample.transition for sample in samples)
        stopped = sum(not sample.pipeline_playing for sample in samples)
        unhealthy = sum(not sample.clock_healthy for sample in samples)
        elapsed_hours = (
            (samples[-1].clock_ns - samples[0].clock_ns) / 3_600_000_000_000
            if len(samples) > 1
            else 0.0
        )
        rate_ppm, rate_count = current_rate_ppm(samples)
        latest = samples[-1]

        print(f"\n{player}")
        print(
            f"  coverage: {len(valid)}/{len(samples)} valid over {elapsed_hours:.3f}h; "
            f"invalid={invalid} transition={transitions} not_playing={stopped} "
            f"clock_unhealthy={unhealthy}"
        )
        print(
            f"  latest: error={format_ms(latest.error_ns)} generation={latest.generation} "
            f"cue={latest.cue_index} cue_position={latest.cue_position_ns / 1e9:.3f}s "
            f"disconnects={latest.disconnects} "
            f"recoveries={max(0, latest.recoveries - 1)}"
        )
        print(f"  recovery activity: {recovery_summary(samples)}")
        if rate_ppm is None:
            print(f"  current rate window: unavailable ({rate_count} samples)")
        else:
            print(
                f"  current rate window: {rate_ppm:+.3f}ppm "
                f"({rate_ppm / 10_000:+.6f}%) across {rate_count} samples"
            )

        player_events = {
            kind: count
            for (event_player, kind), count in event_counts.items()
            if event_player == player
        }
        if player_events:
            rendered = ", ".join(
                f"{kind}={count}" for kind, count in sorted(player_events.items())
            )
            print(f"  lifecycle CSV events: {rendered}")

        print_population("console-compatible valid samples", valid, thresholds_ms)
        if settle_seconds > 0:
            settled = [
                sample
                for sample in valid
                if sample.seconds_since_discontinuity is None
                or sample.seconds_since_discontinuity >= settle_seconds
            ]
            print_population(
                f"settled samples (at least {settle_seconds:g}s after a discontinuity)",
                settled,
                thresholds_ms,
            )

        if top_count > 0 and valid:
            print(f"  worst {min(top_count, len(valid))} valid samples by absolute error:")
            for sample in sorted(valid, key=lambda item: abs(item.error_ns), reverse=True)[
                :top_count
            ]:
                since_transition = (
                    "never observed"
                    if sample.seconds_since_transition is None
                    else f"{sample.seconds_since_transition:.3f}s"
                )
                since_discontinuity = (
                    "none observed"
                    if sample.seconds_since_discontinuity is None
                    else f"{sample.seconds_since_discontinuity:.3f}s"
                )
                print(
                    f"    row={sample.row_number} clock={sample.clock_ns / 1e9:.3f}s "
                    f"error={format_ms(sample.error_ns)} cue={sample.cue_index} "
                    f"cue_position={sample.cue_position_ns / 1e9:.3f}s "
                    f"since_transition={since_transition} "
                    f"since_discontinuity={since_discontinuity} generation={sample.generation} "
                    f"recovery_attempt={sample.recoveries}"
                )

    print_pairwise_report(
        samples_by_player,
        thresholds_ms,
        top_count,
        settle_seconds,
        pair_max_gap_seconds,
    )


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be zero or greater")
    return parsed


def non_negative_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < 0:
        raise argparse.ArgumentTypeError("must be a finite number zero or greater")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("must be a finite number greater than zero")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Summarize Syncplayer diagnostic telemetry. The primary population uses the same "
            "validity rule as the installation console."
        )
    )
    parser.add_argument(
        "csv_path",
        nargs="?",
        type=Path,
        default=Path("output.csv"),
        help="diagnostics CSV written by --diagnostics-log (default: output.csv)",
    )
    parser.add_argument(
        "--top",
        type=positive_int,
        default=5,
        help="number of worst valid samples to print per player (default: 5; 0 disables)",
    )
    parser.add_argument(
        "--threshold-ms",
        action="append",
        type=positive_float,
        metavar="MS",
        help=(
            "absolute-error threshold to count; repeat for several thresholds "
            "(defaults: 40, 50, 100, and 250; 40 ms is one frame at 25 fps)"
        ),
    )
    parser.add_argument(
        "--settle-seconds",
        type=non_negative_float,
        default=0.0,
        metavar="SECONDS",
        help=(
            "also report a population excluding this interval after transitions, non-playing or "
            "unhealthy samples, lifecycle events, generation changes, disconnects, and recoveries "
            "(default: 0, disabled)"
        ),
    )
    parser.add_argument(
        "--pair-max-gap-seconds",
        type=positive_float,
        default=2.5,
        metavar="SECONDS",
        help=(
            "largest gap between samples used for pairwise interpolation "
            "(default: 2.5)"
        ),
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    thresholds_ms = tuple(args.threshold_ms or DEFAULT_THRESHOLDS_MS)
    try:
        samples_by_player, events = read_csv(args.csv_path)
    except (OSError, ValueError, csv.Error) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    if not samples_by_player:
        print("error: CSV contains no diagnostic samples", file=sys.stderr)
        return 2
    print_report(
        args.csv_path,
        samples_by_player,
        events,
        thresholds_ms,
        args.top,
        args.settle_seconds,
        args.pair_max_gap_seconds,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
