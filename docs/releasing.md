# Releasing

A release starts with a tag. Everything after the tag is automatic: the CI
builds one binary for each platform, makes the GitHub release, and sends the
eight crates to crates.io.

## Once, before the first release

Write one secret in the repository, at Settings, Secrets and variables,
Actions:

| Secret | Where it comes from |
| ------ | ------------------- |
| `CARGO_REGISTRY_TOKEN` | crates.io, at Account Settings, API Tokens, with the scope `publish-update` |

`GITHUB_TOKEN` needs no work, because GitHub gives it to each workflow.

## The steps

1. Move the facts of the release into the changelog. In `CHANGELOG.md`, the
   heading `## [Unreleased]` becomes the version and the day:

   ```markdown
   ## [0.2.0] - 2026-08-14
   ```

   Then write a new empty `## [Unreleased]` above it. The release notes on
   GitHub are this section, so what it does not say, the release does not say.

2. Write the same version in `Cargo.toml`. It is in two places: the `version`
   of `[workspace.package]`, and the version of each aphid crate in
   `[workspace.dependencies]`. One command does both, and writes `Cargo.lock`:

   ```bash
   cargo install cargo-edit    # once
   cargo set-version --workspace 0.2.0
   ```

3. Run what each change runs:

   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

4. Read what goes to crates.io, without sending it:

   ```bash
   cargo publish --workspace --dry-run --locked
   ```

5. Commit, tag and push. The tag is the version with a `v` in front of it:

   ```bash
   git commit -am "release: 0.2.0"
   git tag v0.2.0
   git push && git push --tags
   ```

The number itself follows [Semantic Versioning](https://semver.org). A change
that makes an old command answer in a new way is a major release, even when the
code of the change is small.

## What the tag starts

| Workflow | What it does |
| -------- | ------------ |
| `release.yml` | Plans the release, builds each platform on its own runner, and makes the GitHub release with the archives, the checksums and the installer. |
| `publish-crates.yml` | Waits for that release, and then sends the crates to crates.io. |

`publish-crates.yml` runs after the release exists, so a failure at crates.io
leaves the binaries where they are. To send the crates again after such a
failure, start `Publish to crates.io` by hand from the Actions page and give it
the tag.

A crate on crates.io is permanent. A version that went out cannot go out again
with different contents, so step 4 is the step to do carefully.

## The order of the crates

`cargo publish --workspace` reads the graph and sends each crate after the
crates it needs. The order is `aphid-core`, `aphid-agent`, `aphid-plugin`,
`aphid-code`, `aphid-nostr`, `aphid-colony`, `aphid-alate`, `aphid-ai`. Each
crate of the workspace names a version as well as a path in
`[workspace.dependencies]`, because a path alone is enough to build and not
enough to publish.

## The configuration of the release

`dist-workspace.toml` holds the platforms, the installer and the tools that
each runner installs. `.github/workflows/release.yml` comes from that file, so
no hand edits go in it. After a change:

```bash
dist generate
git add dist-workspace.toml .github/workflows/release.yml
```

To read what a release would hold, without a build and without a tag:

```bash
dist plan
```

To make the installer on this machine, which is how to read what it does:

```bash
dist build --artifacts=global
```

To build the archive of this machine, which takes as long as one runner takes:

```bash
dist build --artifacts=local
```

Each of the three writes to `target/distrib`.

## A newer dist

`cargo-dist-version` in `dist-workspace.toml` says which version of `dist` the
CI uses. To move to a newer one, install it and let it write the file again:

```bash
cargo install cargo-dist --locked
dist init
dist generate
```

Read the difference in `release.yml` before the commit. That file decides which
runner builds each platform, and a new version of `dist` can move a build to
another image of the operating system.

## A release that must not go out yet

A tag such as `v0.2.0-rc.1` makes a pre-release on GitHub. `dist` marks it as
one, so the address `releases/latest/download/...` still gives the version
before it, and the installer of a user gives the stable release.

## The site

The site is not part of a release. Each push to `main` that touches `docs/`,
`site/`, `book-theme/`, `book.toml` or the `justfile` builds it again and
deploys it, with `.github/workflows/pages.yml`. To read it first:

```bash
just serve
```

The site is at <https://aphid.embornal.com>, and the book at `/docs/` under it.
Two settings hold that address, and a move to a different one changes both:

- `baseURL` in `site/hugo.toml`.
- The custom domain of the repository, at Settings, Pages. A workflow that
  deploys reads the domain from there, so a `CNAME` file in the tree does
  nothing.

The domain also needs one record in DNS, a `CNAME` of `aphid.embornal.com` that
gives `tncardoso.github.io`. The source of the Pages of the repository must be
`GitHub Actions`.

A site under a path, such as `example.com/aphid/`, needs more: `site-url` in
`book.toml`, and the links of the nav bar in `book-theme/index.hbs`, which
start at the root of the domain.
