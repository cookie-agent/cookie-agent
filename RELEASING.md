# Releasing cookie agent

Stable releases are driven by manual `vX.Y.Z` tags. cargo-dist builds the five
supported targets, creates tar archives and SHA256 checksums, generates the
shell installer, and publishes those files to a GitHub release. The workflow
also publishes the three public Rust crates when `CARGO_REGISTRY_TOKEN` is
configured in the repository's Actions secrets.

## Stable release

1. Update every workspace package version in `crates/*/Cargo.toml`.
2. Update every exact internal path dependency pin (`version = "=X.Y.Z"`) to
   the same version.
3. Run the full local verification suite and merge the version bump to `main`.
4. Create and push an annotated `vX.Y.Z` tag at the release commit.
5. Monitor the Release workflow and verify the GitHub release artifacts.

The workflow publishes crates in dependency order:
`cookie_agent_identity`, `cookie_agent_protocol`, then
`cookie_agent_plugin_sdk`. It waits for each dependency version to appear in
the crates.io sparse index before publishing the dependent crate. Publication
finishes before GitHub release creation. Each crate is skipped when its exact
version is already indexed, so rerunning a partially completed release resumes
at the first missing crate.

For the first release, all workspace versions and exact pins are `0.2.0`, and
the tag is `v0.2.0`.

## Nightly release

Every push to `main` builds the same five targets and force-updates the rolling
`nightly` GitHub prerelease. Each binary includes the source commit in
`cookie --version`. A newer successful run replaces the previous nightly
assets; superseded in-progress runs are cancelled.
