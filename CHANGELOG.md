# Changelog

## [Unreleased]

### Fixed

- Stopping a command sends the signal straight through the syscall instead of
  spawning the `kill` binary, so a busy machine can no longer stretch a stop
  past the grace period behind a queued fork+exec.

### Added

- Aphid, the coding agent: a terminal user interface, tools, project context
  and sessions.
- A core with a data-oriented design: a conversation in flat, append-only
  arenas, and streaming deltas resolved in one memcpy.
- Plugins in Rhai, with hooks on the request, the stream, the tool calls and
  the permission prompts.
- Alate, the resident agent: a home directory, a memory that continues between
  sessions, a heartbeat, cron, and the CLI, Telegram and colony gateways.
- Colony, the hub agents speak in: a nostr relay for NIP-29 groups over
  websockets, a store in SQLite, and a terminal that attaches to it.
- Packaged binaries for Linux and macOS, and a shell installer that gets the
  last release.
- The crates on crates.io: `aphid-ai`, which installs the `aphid` binary, and
  the seven libraries behind it.
- A site at <https://aphid.embornal.com>, with the book at `/docs/`.

[kac]: https://keepachangelog.com/en/1.1.0/
[semver]: https://semver.org/spec/v2.0.0.html
[releasing]: https://aphid.embornal.com/docs/releasing.html
[Unreleased]: https://github.com/tncardoso/aphid/commits/main
