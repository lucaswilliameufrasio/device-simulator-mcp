# Feature: Platform I/O boundary

## Scope

Complete the approved separation of platform operations from MCP presentation.
Keep public tool schemas, transport defaults and device behavior unchanged.

## Local Decisions

- Move legacy CLI I/O, target resolution and command diagnostics to
  `src/platform.rs`. This module has no dependency on rmcp or MCP argument
  structures. Persistent transport modules remain separately platform-specific.
- Represent swipe coordinates as a plain tuple at the backend boundary; the
  MCP layer translates its typed arguments without performing device I/O.
- Keep input/schema validation and MCP content/error rendering in `src/mcp.rs`.
- Preserve the existing command-runner test seams and fallback regression tests.
  This is a mechanical extraction, not a backend-default or lifecycle change.
- Expose configured capabilities through a separate `device_capabilities` tool,
  without changing legacy status output or spawning CLI commands. Clearly mark
  that availability/readiness was not probed; configuration support is not proof
  of a connected device. Report backend/experimental status, observation/wait
  support and operation limits, never endpoints or credential values.

**Source:** Approved ai-memory plan in
`ursoc/device-simulator-mcp/plans/persistent-device-performance.md`.
Arandu remains unavailable; mirror this local record when access returns.

## Open Questions

- Capability discovery needs real-harness adoption and runtime-readiness validation.
- Further separation into iOS/Android submodules is useful if CLI code grows;
  do not introduce new dependencies or transport abstractions during extraction.
