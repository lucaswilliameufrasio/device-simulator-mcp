# Device Simulator MCP

- Keep the MCP server framework-agnostic.
- Keep device I/O in platform-specific helpers and presentation in the MCP tool layer.
- Use `cargo add` for Rust dependencies and keep `Cargo.lock` committed.
- Use pnpm 12.4.1 and Node.js 24.21.0 or newer for `docs-site`.
- Keep code, comments, and tests in English.
- Do not use shell interpolation for user-provided tool arguments.
- Run Rust checks and `pnpm --dir docs-site build` before handoff.
- Keep the repository private until the release installer and device flows have been validated.
