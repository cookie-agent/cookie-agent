# bincode_reloaded 3.1.15 provenance

This directory is the exact crates.io source payload for
`bincode_reloaded` 3.1.15, excluding its standalone development lockfile.

- Upstream repository: <https://github.com/butlergroup/bincode_reloaded>
- Upstream commit: `222701399bfb6bd3c9569a2f4f02cc5aa2fcaeac`
- crates.io archive: <https://crates.io/api/v1/crates/bincode_reloaded/3.1.15/download>
- Archive SHA-256: `2e4ac690d35463a65215a28cbc1a0de736a2ed299113874f1a8cdf5d5adc231e`
- License: upstream `LICENSE.md` (MIT), unchanged

## Local delta

`Cargo.lock` is removed because this crate is consumed only as a dependency and
the upstream development lock contains its bincode-1 compatibility-test
dependency. `Cargo.toml.orig`, all source, tests, and license files are
unchanged. This provenance README is the only added file.

The root workspace `Cargo.lock` is the only authoritative dependency graph.
Codec behavior is exercised through the nonpublishable compatibility-shim
workspace member and Cookie's exact Syntect feature set; this vendored library
is never resolved or tested as a standalone dependency universe.

## Integrity values

- Upstream `Cargo.toml` SHA-256: `cece534ba1e7cd8edae14dcf3fc55afc76751e575fcadb6de1ffd87ac8296e76`
- Local `Cargo.toml` SHA-256: `cece534ba1e7cd8edae14dcf3fc55afc76751e575fcadb6de1ffd87ac8296e76`
- Removed upstream `Cargo.lock` SHA-256: `5a01e9dc199404c590d35df55a327f9b1da7dc03b3ffb83402350c2fd32149b2`
- Unchanged 73-file tree SHA-256: `a492cb0139a4fc0864da8cb93a94d545653086da4f6e9c6fe40d47a20d1e6bf6`
- Upstream `LICENSE.md` SHA-256: `90d7e062634054e6866d3c81e6a2b3058a840e6af733e98e80bdfe1a7dec6912`

The unchanged-tree digest sorts relative file paths, excludes `Cargo.toml`, the
removed `Cargo.lock`, and this README, and hashes each path, NUL byte, file
contents, and NUL byte in sequence.
