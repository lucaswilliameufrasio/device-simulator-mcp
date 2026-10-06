# Feature: Equivalent capture performance validation

## Scope

Continue the approved plan with read-only, paired CLI/MCP capture comparisons
and bounded measurements of idle MCP process resources. Do not infer performance
from different image formats, scaling or cached observations.

### CI device smoke scope — 2026-10-07

The CI E2E harness uses disposable simulators without a controlled fixture app.
It validates MCP startup, status, fresh capture and cleanup only; it must not
tap, swipe or type into an unknown home screen. MCP input dispatch is covered by
stdio integration tests and separate authorized tests on known screens. Revisit
this boundary if CI gains an installed, controlled fixture app. The smoke driver
keeps one stdio process alive and waits for the initialize response before
sending `notifications/initialized` and tool requests, matching MCP's session
handshake and allowing session-owned resources to be stopped explicitly.

The iOS `device_start` tool deadline is 60 seconds: it covers a dedicated,
bounded 30-second `serve-sim --list` enumeration command, the separate 20-second
helper-readiness window, and 10 seconds of orchestration overhead. CI evidence
showed enumeration can exceed the general 20-second command cap; the 45-second
outer deadline did not help because that inner command timed out first. Only this
read-only startup enumeration gets the longer command cap. The general 20-second
command cap, readiness window, and status, capture, and input deadlines remain
unchanged. Start performs setup only; input tool deadlines remain unchanged.
If readiness expires, the startup error includes only the last sanitized status
probe error (never endpoint response bodies or screen content), so CI can
distinguish a missing helper endpoint from a helper that has not published
capture dimensions yet without widening the readiness window speculatively.

### MCP-owned serve-sim idle shutdown — 2026-10-07

The operator reported that an active `serve-sim` Node process consumed about one
CPU core and made the Mac unusable, then stopped it manually. The pinned
`serve-sim@0.1.47` source describes event-driven capture with a 5-fps idle floor;
`--no-preview` disables only the web UI, not native framebuffer capture or MJPEG
encoding. There is no CLI frame-rate throttle. To reduce idle load without
changing third-party code, MCP-owned helpers will be stopped after 30 seconds
without helper-dependent MCP activity and started again on the next explicit
`device_start` or default iOS CLI input. Externally managed helpers are never
stopped or adopted for timeout management. A cold restart adds startup latency;
status and fresh CLI screenshots do not start the helper. **Source:** operator
choice, 2026-10-07; pinned `serve-sim@0.1.47` `FrameCapture.swift` and README.

## Local Decisions

- CLI comparisons require explicit UDID/serial and default fresh full-resolution
  PNG arguments. Android must use the default ADB backend. Alternate CLI-first
  and MCP-first sample ordering within one persistent MCP session to reduce
  warming/order bias. Both sides include capture/read costs, but MCP also includes
  protocol/image encoding/delivery and client-side JSON decoding.
- Compare latencies and report raw CLI PNG bytes separately from JSON MCP bytes.
  PNG file metadata/compression may differ; screen pixel content is never logged.
- Direct CLI commands receive argv arrays, deadlines and bounded output reads;
  use unique temporary screenshot paths with cleanup, never shared names.
- Optional idle sampling measures this MCP process's CPU-time delta and RSS,
  using `ps` with only those fields. It does not measure Simulator/Emulator or
  external backend resources; report this limitation explicitly. No persistent
  background profiler, trace arguments or screenshots in logs.
- Measurements are local evidence, not an invented SLA. Input/harness recovery
  flows and cross-platform installers still require separate validation.

**Source:** ai-memory
`ursoc/device-simulator-mcp/plans/persistent-device-performance.md`.
Recorded while Arandu was unavailable; mirrored on 2026-10-06 to Arandu page
`01a10f82-5903-7a21-bd7c-9bcd97200961`.

## Live iOS backend validation — 2026-10-06

The npm registry's `latest` metadata reported `serve-sim@0.1.47`, matching the
repository pin. Node 24.20.0 satisfied its `>=20` engine requirement. With
authorized npx provisioning, ownership-aware MCP lifecycle startup took 2.93 s;
read-only status, JPEG capture and capture-only step all passed (two samples
each), and `serve-sim --list` confirmed the MCP-owned helper was stopped.

A separately managed helper was then started on loopback and exercised through
the opt-in persistent backend: status, three JPEG `latest_frame` captures and
three capture-only steps all succeeded. Capture p50 was 21.23 ms and step p50
21.79 ms in this small run. These frames can be replayed and have unknown source
age; their latency/payload is not comparable to fresh full-resolution PNG.
The MCP did not stop the externally managed helper; the test harness terminated
only the process group it had started and verified the helper was gone. No
device inputs, app launch, global package install or MCP client config changes.

In a separate recovery smoke, one MCP process successfully captured, observed
an expected status failure while its test-owned helper was stopped, and recovered
status plus capture after a new helper started on the same loopback port. The
test-owned helper was stopped and verified absent afterward; images were not
printed and no inputs were sent.

These checks validate helper lifecycle and persistent read-only capture/recovery,
not persistent WebSocket input reconnection or app-render acknowledgement. Those
still require a controlled app/device scenario.

### Idle resource sample

After warming the helper with status and three captures, a 10 s idle sample with
MCP attached measured 1,350 ms aggregate CPU-time delta across the three-process
`npx` process group. MCP itself had no detectable CPU-time delta at `ps` precision
and RSS changed from 6,619,136 to 4,308,992 bytes. A separate 10 s helper-only
sample (no MCP client connected) measured 1,430 ms aggregate CPU-time delta across
three processes; summed RSS changed from 266,371,072 to 202,162,176 bytes.

These small single-machine samples suggest the external helper remains active
without an MCP client. `ps` CPU-time resolution is coarse; process-group RSS is a
sum that can double-count shared pages; Simulator resources are excluded. Treat
this as a follow-up signal, not a universal idle-resource limit or SLA.

### TixNow Android navigation smoke — 2026-10-06

On the existing `emulator-5554`, a read-only capture showed the TixNow launcher
icon. Opened the app and observed its Highlights landing screen, which reported
no highlights available. A single tap on the public Discover tab navigated to
the search screen and displayed `Unable to load events`; no Retry or search was
submitted. Returned to Highlights. Each known navigation tap used a fresh
capture and `visual_stability(stable_samples=2)`; all three actions completed
and no text entry, ticket/profile flow, login, payment or account mutation was
performed.

The first stability wait after launch returned the splash screen; a later
read-only capture after a 1 s settle showed the landing screen. This is direct
evidence that visual stability does not mean app startup/network loading is
complete. The smoke validates safe navigation/input and visual wait/capture
delivery, not business flows, network success or app-render acknowledgement.

A single upward swipe on the empty Highlights content area completed, but its
visual-stability observation showed only the purple background and bottom tabs;
a separate fresh capture confirmed the same state. No swipe retry was sent. A
single tap on the known Highlights tab restored the normal landing screen. This
is a useful recovery smoke, while also showing that visual stability can accept
a stable but incomplete/blank app state. It does not establish why the app
content disappeared or validate a business interaction.

A separate `visual_change` smoke tapped the public Discover tab, returned the
changed Search events screen with `Unable to load events`, then tapped Highlights
and returned to its landing screen. The returned observation from the second
transition briefly showed the Highlights page content while the bottom-nav
selection still appeared on Discover; a later fresh capture confirmed the
Highlights selection. Thus `visual_change` detects a changed frame but does not
guarantee all UI components have finished rendering or that network loading has
completed. No Retry or search was submitted.

### TixNow Android rotation and targeted ADB reconnect — 2026-10-06

The configured device tools do not expose rotation or transport-fault controls,
so a reversible, serial-scoped ADB operation was used on `emulator-5554`. Before
the test, Android reported `accelerometer_rotation=1` and `user_rotation=0`.
Temporarily disabled auto-rotation and set landscape; the same MCP capture
reported source dimensions 1920x1080 and rendered TixNow Highlights in
landscape. Restored `user_rotation=0` and `accelerometer_rotation=1`; a fresh
MCP capture confirmed portrait and the settings were re-read as 1/0.

Ran `adb -s emulator-5554 reconnect` (did not stop the emulator). The targeted
device returned to ADB `device`; status and fresh capture through the existing
MCP session succeeded. The app content was blank purple after reconnect and
remained blank after a 1 s settle. A single tap on the known Highlights tab was
submitted to recover; `device_step` returned an internally ambiguous failure
(`completed_actions=1` with `current_action_may_have_applied=false`). No input
was retried. Code review confirms `completed_actions` counts actions whose
backend submission returned successfully; `current_action_may_have_applied`
is for an action that itself failed/timed out, so `false` is expected when the
failure is in the later visual wait. The backend submission succeeded, but that
does not acknowledge app rendering. A subsequent read-only capture showed the
normal Highlights page, and final ADB/settings checks confirmed the emulator
online, portrait, and auto-rotation restored. This validates transport recovery
for read-only ADB observations and exposes an app-render/recovery limitation; it
does not prove reconnect behavior for a persistent input transport.

### Cicleta iOS persistent-backend input smoke — 2026-10-06

The user confirmed Cicleta was running on the sole booted iOS Simulator. A direct
read-only simulator screenshot showed its login screen. Started the pinned
`serve-sim@0.1.47` helper and an isolated local MCP process configured for the
known simulator UDID; MCP status and fresh capture succeeded. One `device_step`
tap targeted the empty E-mail field and used `visual_change` with a fresh JPEG
observation. The backend reported one completed submission; the app visibly
focused the field (blue outline/caret). No text was entered and no login/create
account action was tapped. The keyboard was not visible in the resulting frame.
The MCP-owned test process and only its test helper were stopped; `serve-sim
--list` confirmed cleanup. Temporary screenshots were removed. This validates a
real persistent iOS input submission and observed UI response on Cicleta's login
screen, not authentication, business flows, app-render acknowledgement beyond
the visible focus change, or WebSocket input recovery after helper reconnect.

In a follow-up with the same MCP process, focused the password field, stopped
only the test-owned helper, observed the expected status failure, then restarted
the helper on the same loopback port. That MCP process recovered status/capture
and accepted a subsequent tap without recreating the MCP process. The resulting
frame unexpectedly showed Cicleta's blank Create account form; no values were
entered and its submit button was not used. An initial attempt to navigate back
used a misaligned coordinate and did not change the page. A fresh accessibility
query found the enabled `Já tenho conta` control, but its frame did not map to
the tested normalized coordinate as expected. A tap on the visible top-left
back arrow returned to login. The iOS keyboard appeared with the E-mail field
empty. A swipe intended to dismiss that keyboard traversed keyboard keys and
entered incidental characters into the field. No submit/login/account action
occurred; the text was cleared using observed backspace controls and a fresh
capture confirmed the field empty. The keyboard remains visible. This input
mistake is a caution: do not swipe across an on-screen keyboard to dismiss it.
This smoke confirms helper/status recovery and backend acceptance of an input
after helper restart in the same MCP process, but the unexpected navigation and
keyboard state mean it is not a clean assertion of post-restart app rendering.
Record as an ambiguous smoke, not an end-to-end reconnect guarantee. Each
test-owned helper was terminated and checked absent.
