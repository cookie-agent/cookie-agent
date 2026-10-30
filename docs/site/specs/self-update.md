# `cookie update`: updating to the newest stable or nightly release

Status: **implemented** (2026-10-30; no protocol change); decisions in
[section 11](#11-decisions), implementation notes in
[section 13](#13-implementation-notes). Builds on the release process in
`RELEASING.md` and the [install page](../install.md).

## 1. Problem

Cookie has no way to update itself. A user reruns the installer, which
always installs the latest **stable** release. A user on a nightly has to
find the newest prerelease on GitHub and run that release's own installer by
hand.

The binary also cannot tell which channel it came from:

- `build.rs` sets `COOKIE_VERSION` to `CARGO_PKG_VERSION`, plus
  `+<commit>` when `COOKIE_GIT_HASH` is set.
- The release workflow sets `COOKIE_GIT_HASH` for stable and nightly builds
  alike. Stable v0.4.0 and nightly `v0.4.0-alpha.1793047587` both report
  `cookie 0.4.0+<sha>`.
- A local `cargo install` reports `cookie 0.4.0`, which is the same as a
  stable 0.4.0 without the hash.
- The installer's receipt (`~/.config/cookie_agent/cookie_agent-receipt.json`)
  holds the full version (`0.4.0-alpha.1793047587`). But it exists only for
  installer installs, it lives apart from the binary, and archive installs
  and copied binaries don't have one.

Release facts this design relies on (observed 2026-10-30):

- Stable tags are `vX.Y.Z`, published by hand. GitHub's `releases/latest`
  is the newest stable (today `v0.3.0`).
- Nightlies are `vX.Y.Z-alpha.<unix seconds>` prereleases, at most one a
  day. They are pruned after 14 days, and the newest three are always kept.
- **Semver does not order nightlies against stables.** Main keeps the
  version it had at the last bump, so nightlies built after stable `v0.4.0`
  stay `v0.4.0-alpha.T`, which semver ranks *below* `v0.4.0`. The
  timestamp is the only reliable order for nightlies.
- Every release has `cookie_agent-<target>.tar.xz` (`.zip` on Windows) for
  each of the seven targets, with a `.sha256` file beside it. A release
  whose upload is still running is public but missing those assets.

## 2. Goals

- `cookie update` (alias `cookie upgrade`) replaces the running `cookie`
  binary with the newest release **on the channel it was installed from**:
  a stable stays on stable, a nightly stays on nightly.
- Switching channels and pinning a release are explicit flags.
- It never breaks a working install: the download is checked and the new
  binary is tested before anything is replaced, and the swap is atomic.
- It never restarts the daemon; it only says when a restart is due.

Non-goals: updating remote runtimes ([Q4](#11-decisions)), an
"update available" notice in the clients ([Q3](#11-decisions)), background
auto-update, package managers (cookie ships none), and replacing
source builds without being asked.

## 3. Channel identity: embed the release tag

The engine's `build.rs` reads `COOKIE_RELEASE_TAG`, `COOKIE_GIT_HASH`, and
`TARGET`. It emits the first two, possibly empty, and `COOKIE_TARGET`, the
target triple the binary was built for.

`release.yml`'s `build-local-artifacts` sets
`COOKIE_RELEASE_TAG: ${{ needs.plan.outputs.tag }}` beside
`COOKIE_GIT_HASH`.
`RELEASING.md`'s list of maintained workflow edits gains this line.

`cookie_agent_engine::release` derives the channel:

| `COOKIE_RELEASE_TAG` | Channel |
|---|---|
| `vX.Y.Z` | `stable` |
| `vX.Y.Z-<pre>` (today `-alpha.<ts>`) | `nightly` |
| empty | `source` (a `cargo install`, a local build, CI test builds) |

`cookie --version` shows it:

```
cookie 0.4.0-alpha.1793047587 (nightly, 67b3d81)
cookie 0.4.0 (stable, 1a2b3c4)
cookie 0.4.0 (source)
```

The daemon reports the same version, the tag without its `v`, as
`engine_version` in its handshake. The TUI's "the daemon runs another
version" notice and `cookie update`'s restart hint compare it, so they tell
two nightlies of the same `0.4.0` apart.

## 4. Command

```
cookie update [--check] [--channel stable|nightly] [--version <tag>] [--force]
```

`upgrade` is a visible clap alias. `cookie sessions upgrade` is a different
subcommand under `sessions`, so the two do not collide.

| Flag | Meaning |
|---|---|
| (none) | Update to the newest release on this binary's channel. |
| `--check` | Report the installed and newest versions and change nothing. Exits 0 either way; prints `update available` / `up to date`. |
| `--channel stable\|nightly` | Use that channel instead, which switches channels. This is required on a source build. |
| `--version <tag>` | Install exactly that release (`v0.3.0`, `v0.4.0-alpha.1793047587`), including older ones. Conflicts with `--channel`. |
| `--force` | Reinstall even when already on the newest release. |

Output is short and plain, for example:

```
cookie 0.4.0-alpha.1793047587 (nightly) → 0.4.0-alpha.1793133987 (nightly)
downloaded cookie_agent-x86_64-unknown-linux-gnu.tar.xz (31.2 MB), sha256 ok
replaced /home/me/.cargo/bin/cookie
the local daemon still runs 0.4.0-alpha.1793047587; restart it when convenient:
  cookie daemon restart
```

## 5. Finding the newest release

All requests go to the GitHub API (`api.github.com/repos/cookie-agent/cookie-agent`).
They send `COOKIE_AGENT_GITHUB_TOKEN` as a bearer token when it is set,
which is the same variable the installers read. Unauthenticated requests are
limited to 60 an hour, which is plenty for this command.

- **stable**: `GET /releases/latest`. Update when its tag is semver-newer
  than the installed tag. An equal or older tag means up to date. (An older
  one can happen right after a release is published, while `latest` lags.)
- **nightly**: `GET /releases?per_page=30`. Keep the prereleases whose tag
  parses as `v<semver>-alpha.<ts>` and pick the largest `<ts>`. Order by the
  timestamp, never by semver ([section 1](#1-problem)). Update when that
  `<ts>` is larger than the installed one's.
- **`--version`**: `GET /releases/tags/<tag>`.

A candidate is installable only when it has this target's archive and its
`.sha256`. If the newest release lacks them, its upload is still running.
For stable, cookie says so and stops ("v0.5.0 is still being uploaded; try
again in a few minutes"). For nightly, it falls back to the next newest
nightly that has the archive.

**A nightly user is not moved to a stable** that was published after their
nightly. Nightlies are built from `main`, which contains every stable
release commit, so staying on the nightly channel loses nothing.
(Decided: [Q1](#11-decisions).)

The lookup lives in `cookie_agent/src/update_command.rs`, which takes the
API's base URL as a parameter so tests can point it at a local server.

## 6. Downloading and verifying

1. Download `cookie_agent-<COOKIE_TARGET>.<tar.xz|zip>` and its `.sha256`
   into a fresh temporary directory **beside the binary**
   (`<bin dir>/.cookie-update-<random>/`). That puts it on the same
   filesystem, which keeps the final rename atomic. If that directory is not
   writable, stop with the error in [section 7](#7-replacing-the-binary)
   before downloading anything.
2. Compare the SHA-256 with the `.sha256` file. A mismatch aborts the
   update. (Like the installers, this checks integrity, not authenticity.
   Attestations are off in `dist-workspace.toml`.)
3. Extract only the `cookie` (`cookie.exe`) entry. Archives are read with
   `tar` and `lzma-rs` (pure Rust; the archive is decompressed to a `.tar`
   in the scratch directory, then read) on Unix, and with `zip` (deflate
   only, a Windows-only dependency) on Windows. `flate2` and `sha2` are
   already in the tree.
4. **Smoke test**: give the new file the old one's mode (executable in any
   case), run `<new binary> --version` with a 10 s timeout, and require
   that it prints `cookie ` and the release's major.minor.patch. (Releases
   from before this change print only that core version.) This catches a wrong
   architecture, a missing glibc, or a truncated file while the old binary is
   still in place.

The temporary directory is removed on every exit path.

## 7. Replacing the binary

- The target is `std::env::current_exe()` with symlinks resolved. Cookie
  replaces the file the link points at, never the symlink. The receipt and
  `CARGO_HOME` are not used, because the running binary is the one the user
  means.
- **Unix**: set the new file's mode to the old file's, `fsync` it, then
  `rename` it over the target. Running processes, such as the daemon,
  `cookie runtime serve`, and open TUIs, keep their old inode and carry on.
- **Windows**: a running `.exe` cannot be overwritten, but it can be
  renamed. The old binary moves aside as `cookie.exe.<uuid>.old`, the new
  one moves into place (if that fails, the old one moves back), and every
  `cookie` start deletes the `.old` files beside it that nothing runs any
  more.
- **Not writable** (`/usr/local/bin` owned by root, a read-only mount):
  stop with the path and the reason. Cookie never escalates privileges
  itself; the user can rerun it with `sudo`.
- If the installer's receipt exists and its `install_prefix` contains the
  binary, update its `version` field so the receipt matches the binary that
  is actually installed.

**Fixed alongside this.** On Linux, once the binary has been renamed
over, `current_exe()` in an already running process returns
`/path/cookie (deleted)`. `coding_runtime::server::default_bridge_command`
resolves it at every `serve()` call, so a runtime the daemon reconnects
after an update would start ACP agents' MCP bridges at a path that does not
exist. A plain `cargo install` over a running daemon has the same problem
today. The fix: `coding_runtime::server::pin_executable` resolves the
executable once at process start, the way `daemon_restart::Relaunch::capture`
already does, and bridge commands use that path.

## 8. Source builds

When the channel is `source`, cookie refuses unless `--channel` or
`--version` is given:

```
cookie 0.4.0 was built from source, so `cookie update` does not know which release to track.
Rebuild it the way you built it, or replace it with a release build:
  cookie update --channel stable
  cookie update --channel nightly
```

This keeps `cookie-reinstall` dev installs safe from an accidental update
(decided: [Q5](#11-decisions)).

## 9. The running daemon and other machines

- After a successful replace, cookie asks the local daemon for its version,
  using the existing local-daemon discovery. If the daemon is older, cookie
  prints the restart hint shown in [section 4](#4-command) and does nothing
  else. It never restarts the daemon, and it has no flag that does: the
  user runs `cookie daemon restart` when convenient ([Q2](#11-decisions)).
- `cookie daemon restart` re-executes the path captured at the daemon's
  start (`Relaunch::capture`), and that path now holds the new binary. A
  service-managed daemon (systemd, launchd) runs the same path from its unit
  file. No unit edits are needed.
- **Remote runtimes** (`[runtimes.<id>]` SSH templates, dial-in runtimes)
  run their own machine's `cookie`. `cookie update` does not touch them.
  The [templates page](../runtimes/templates.md) already asks for matching
  versions, and the update docs say to run `cookie update` on each machine.
  Updating them from the daemon is out of scope ([Q4](#11-decisions)).
- A downgrade, or a switch from nightly to an older stable, prints a
  warning: an older binary may report configuration keys or history it does
  not know as diagnostics. Cookie proceeds anyway, because the flag was
  explicit.

## 10. Docs, tests, and release process

- `install.md`: a new "Updating" section, and the Nightly builds section
  gains `cookie update --channel nightly`. CLI reference: the new command.
  `RELEASING.md`: the `COOKIE_RELEASE_TAG` workflow edit. Recommended
  addition there: bump `main` to the next version right after a stable
  release, so nightly tags sort sensibly for people reading them. The
  updater does not need it.
- Tests:
  - Channel parsing from tags.
  - Nightly selection by timestamp across a core-version boundary, for
    example `v0.4.0-alpha.20` after `v0.4.0` and `v0.4.0-alpha.10`.
  - Skipping releases without this target's archive.
  - A checksum mismatch aborting the update.
  - The smoke-test failure leaving the old binary untouched.
  - An end-to-end replace against a local HTTP server serving a fake release
    with a tiny "binary" script on Unix.
- No live GitHub calls in tests.

## 11. Decisions

The user decided all five on 2026-10-30.

| # | Question | Decision |
|---|---|---|
| Q1 | When a stable is published after a user's nightly, does `cookie update` on the nightly channel move them to it? | **No.** The channel sticks; the next nightly contains it. |
| Q2 | Daemon after the update: hint only (the printed restart message of [section 9](#9-the-running-daemon-and-other-machines)), hint plus a `--restart-daemon` opt-in, or restart by default? | **Hint only.** No `--restart-daemon` flag; the user restarts it. |
| Q3 | Should the TUI, the web client, or `cookie` startup say "update available"? | **No.** |
| Q4 | Should `cookie update` also update remote runtimes? | **No.** Run it on each machine. |
| Q5 | Source builds: refuse without `--channel`, or default to stable? | **Refuse** ([section 8](#8-source-builds)). |

## 12. Phases

1. `COOKIE_RELEASE_TAG`/`COOKIE_TARGET` in `build.rs` and `release.yml`,
   the channel helper, and the new `--version` output. Starter-kit lookup by
   own tag.
2. The shared `releases` module (moved out of onboarding) and
   `cookie update --check`.
3. Download, verify, smoke test, and atomic replace (Unix and Windows); the
   receipt update; the fix for `current_exe` resolved once at start.
4. The daemon version hint, docs, and the CLI reference.

The first nightly built after phase 1 is the first release that can update
itself. Binaries from before phase 1 have no `cookie update` command, so
their users update once with the installer.

## 13. Implementation notes

Implemented 2026-10-30 in one pass, on the plan of section 12.

- The starter kit and its release lookup in `onboarding.rs` were removed
  upstream (machines on demand) before this landed, so there was no
  release-list code to share; the lookup is new in `update_command.rs`.
- The build identity lives in the engine (`build.rs`, `release.rs`), which
  both the binary and the daemon's server depend on, instead of the
  published `cookie_agent_identity` crate. `cookie --version` takes
  `release::long_version()`; `crates/cookie_agent/build.rs` no longer makes
  the version.
- The new subcommand overflowed the 1 MB main-thread stack of Windows debug
  builds: clap's derived `augment_subcommands` builds every subcommand in
  one frame (418 KB in a Linux debug build). `crates/cookie_agent/build.rs`
  now links the Windows binary with `/STACK:8388608`, the 8 MB Linux and
  macOS give the main thread.
- No `self-replace` dependency: the Windows rename-aside is a few lines.
- `lzma-rs` instead of `lzma-rust2`: the latter's `xz` feature pulls a second
  `sha2`/`digest` stack.
- A nightly lookup reads the newest 30 releases. Nightlies are pruned after
  14 days, at most one a day, so the newest is always among them.
- `release_integrity.rs` requires the `COOKIE_RELEASE_TAG` line in
  `release.yml`.
