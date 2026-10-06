# Feature: Bounded observations and visual waits

## Scope

Continue the approved persistent-device performance plan without promoting the
experimental Android backend. Preserve legacy tool defaults.

## Local Decisions

- Visual waits use fresh platform screenshots, never a latest/replayed frame or
  local cache. Compare a bounded grayscale thumbnail, including original image
  dimensions; this is a heuristic, not application-idle acknowledgement.
- `visual_change` compares a pre-input baseline against post-input samples.
  `visual_stability` requires consecutive equal post-input samples. Poll at most
  once every 200 ms with at most 32 samples under the step's existing deadline.
- Accessibility in a step is explicit, mutually exclusive with a screenshot,
  and requires persistent iOS. Validate capability before submitting any input.
- Cache only one bounded raw capture per process, keyed by platform/target and
  capture source. Reuse is opt-in with `max_age_ms` (at most 5000 ms). Cache age
  measures local acquisition time, never source freshness for replayed frames.
  Invalidate before mutations, repair, start and stop, including failed inputs.
- Cache reuse requires an explicit device target; implicit `booted`/ADB target
  changes cannot safely be inferred without another backend query.
- No retries of mutations, no always-on capture, no image content in logs.
- After a successful visual wait, return the exact fresh sample that satisfied
  the condition when a screenshot was requested. Preserve its acquisition time
  and mark `observation_source: visual_wait_sample` in the step summary.
  Do not reacquire a redundant final screenshot or claim it is a later frame.
  Resizing/cropping applies only to presentation after evaluating the full
  screen. Independent captures without waits keep their existing semantics.
- `element_present` waits require persistent iOS and exact label/identifier
  selectors (at least one, at most 256 bytes each). Both selectors must match
  the same element. Search only the bounded accessibility projection; absence
  in a truncated tree is not evidence of global absence. Poll under the same
  sample/deadline limits and return the matched AX snapshot when AX observation
  was requested, avoiding another fetch. Unsupported capabilities are rejected
  before input. No automatic screenshot fallback or semantic mutation.

**Source:** User-approved plan in ai-memory
`ursoc/device-simulator-mcp/plans/persistent-device-performance.md`.
Arandu is currently unavailable; this canonical local record preserves these
decisions before implementation and must be mirrored when access returns.

## Open Questions

- Real controlled-app animation/rotation/recovery and harness validation.
- Choosing a perceptual threshold beyond exact thumbnail equality needs measured
  evidence; exact comparison is intentionally conservative for this iteration.
