# Feature: Equivalent capture performance validation

## Scope

Continue the approved plan with read-only, paired CLI/MCP capture comparisons
and bounded measurements of idle MCP process resources. Do not infer performance
from different image formats, scaling or cached observations.

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
Arandu is unavailable; mirror the local decision record when access returns.
