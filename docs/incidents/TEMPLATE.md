# INC-XXXX — Short descriptive title

- Status: Open
- Date: YYYY-MM-DD
- Affected component: Master / Slave / protocol / media pipeline / other
- Severity: Development / Installation / Production

## Context and trigger

Describe the operating mode, media, sinks, relevant command-line options, and the condition that
exposed the problem. Include a minimal reproduction command when useful.

## Symptom

Describe what an operator saw or heard. Include timing patterns, logs, telemetry, and whether the
problem reproduced with real sinks, `fakesink`, or both.

## Impact

Describe the effect on playback, synchronization, operators, or development confidence.

## Investigation

Summarize the decisive experiments and evidence. Record important hypotheses that were rejected and
why.

## Root Cause

State the faulty assumption or mechanism. Clearly label any part that remains an inference.

## Resolution

Describe the implementation change and why it corrects the failure.

## Verification

List the automated checks and real-system reproduction matrix used to establish the result.

## Prevention and guardrails

- Record architectural rules, tests, telemetry, or operating guidance that reduce recurrence.

## References

- Code:
- Design or ADR:
- Related documentation:
- Commit or pull request:

## Tags

- Add searchable terms such as `clock`, `audio`, `startup`, `gstreamer`, or `synchronization`.
