# Aphid

Aphid is a fast and hackable agent harness.

## Goal

- Zero memory copy whenever possible
- Data oriented design
- Fast startup times
- Full debuggability, and extensibility via plugins
- Plugins can be written in rhai (https://github.com/rhaiscript/rhai) — see `docs/aphid/plugins.md`
  and `aphid-code`'s `scripting` module. WebAssembly plugins are still to come

## Instructions

- Run lint, format and tests at every code change
    - `cargo clippy`, `cargo fmt`, `cargo test`
- When fixing bugs, add regression tests
- Documentation should be written in ASD-STE100 simplified technical english
- Keep documentation up to date with changes
- Create git commits with Conventional Commits specification
- Record each change a user sees in `CHANGELOG.md`, under `## [Unreleased]`
    - Put the line in an `### Added`, `### Changed`, `### Fixed`,
      `### Deprecated`, `### Removed` or `### Security` subsection
    - The release notes are that text, so write for a user of aphid and not
      for a reader of the code
    - Do not write a version heading or a date. A release does that
    - A change that only touches the build, the tests or the internals needs
      no line

## Releasing

A tag starts the release: the CI builds the binaries, makes the GitHub release
from the changelog, and sends the crates to crates.io. The steps are in
`docs/releasing.md`. Do not edit `.github/workflows/release.yml`, because
`dist generate` writes it from `dist-workspace.toml`.
