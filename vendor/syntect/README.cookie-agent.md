# syntect 5.3.0 provenance

This directory is the exact source payload published as `syntect` 5.3.0, with
the codec-only local patch declared below.

- Upstream repository: <https://github.com/trishume/syntect>
- Upstream tag: [`v5.3.0`](https://github.com/trishume/syntect/tree/v5.3.0)
- Upstream commit: `e4670846ecf16d8832db6c43d531bec466214e27`
- crates.io archive: <https://crates.io/api/v1/crates/syntect/5.3.0/download>
- Archive SHA-256: `656b45c05d95a5704399aeef6bd0ddec7b2b3531b7c9e900abbf7c4d2190c925`
- License: upstream `LICENSE.txt` (MIT), unchanged

## Local delta

The upstream payload has these declared graph-control changes:

- `Cargo.toml`: the optional dependency key remains `bincode`, but its package
  is changed from `bincode` 1.x to the exact local
  `syntect-bincode-compat` 0.1.0 package. That package's library crate remains
  named `bincode`, preserving `bincode::error::Result<T>` in public signatures.
- `Cargo.lock` is removed. Vendored Syntect is a library dependency, not an
  independently released application, so a separately resolved default/all-
  feature graph would be stale and unsupported.

This provenance README is the only added file. `Cargo.toml.orig`,
`src/dumps.rs`, all other source and test files, the public-API snapshot, and
all embedded assets are unchanged. The `.packdump` and `.themedump` files were
not regenerated. Codec implementation and error conversion live entirely in
the sibling `vendor/bincode-compat` crate.

The root workspace `Cargo.lock` is the only authoritative graph. Cookie builds
Syntect with exactly `default-features = false` and the requested features
`default-syntaxes`, `default-themes`, and `regex-fancy`. Root workspace tests
exercise the codec shim, compressed and uncompressed published dumps, themes,
syntaxes, rendering, and static upstream API/source integrity. The standalone
upstream public-API test source and snapshot remain unchanged for provenance,
but no standalone Syntect dependency universe is executed.

## Integrity values

- Upstream `Cargo.toml` SHA-256: `5e684bef35f74140214f024f4d781bb9a6abd896f73b8525ff59cf0a74417d73`
- Local `Cargo.toml` SHA-256: `abcbbb84b2d2e65b016cc18094ff7646f0dd84175d9fb41b674bf9df9e3ecc11`
- Removed upstream `Cargo.lock` SHA-256: `cc63f342a34d83c29b90a72644558c69b30e6bdd1ea2ecfd3ad6092d66085e48`
- Upstream `src/dumps.rs` SHA-256: `237719802be45db966a6e2e5de2f58baa970ac84f83fb375dbdd90592e704e91`
- Local `src/dumps.rs` SHA-256: `237719802be45db966a6e2e5de2f58baa970ac84f83fb375dbdd90592e704e91`
- Unchanged public-API test source SHA-256: `8e2454ad58226b2ecf01fd1a73f6ad2c04e98f04022379ed5e5f5f89828676d9`
- Upstream public-API snapshot SHA-256: `7a8b4cb34bd3bb01c507c9990faa93d902741a2f08050a60a2f76bacdcd545bd`
- Unchanged 57-file tree SHA-256: `15fd18cb2fb1441f3773b6ed900f656a89dc4a72c4ea6248d6a702574eac4e63`
- `assets/default.themedump`: `8b57a2118224993360b6fc5fc2fa2e9872a827f00f9c57d43da08fa42c892399`
- `assets/default_metadata.packdump`: `b1df0402dfdb84b9826b206bffafb35553c46530afcbb3c929147760056766f3`
- `assets/default_newlines.packdump`: `d740b20c12e40b678b9f1012401e1969aaa5cd55f1ab329ffeb94d746b06a5c0`
- `assets/default_nonewlines.packdump`: `b61623ff9b5c36e60666d637076697ad8234116b2d53ad2ee9e3908df1c2461d`

The unchanged-tree digest sorts relative file paths, excludes `Cargo.toml`, the
removed `Cargo.lock`, and this README, and hashes each `path`, NUL byte, file
contents, and NUL byte in sequence. Root workspace release-integrity tests
enforce these values and the declared patch shape.
