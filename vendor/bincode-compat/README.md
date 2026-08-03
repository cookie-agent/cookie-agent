# syntect bincode compatibility shim

This private crate preserves the narrow bincode 1 API surface present in
syntect 5.3.0's public signatures without depending on a package named
`bincode`.

## Provenance

- Compatibility reference: `bincode` 1.3.3
- Reference source: <https://crates.io/api/v1/crates/bincode/1.3.3/download>
- Reference archive SHA-256: `b1f45e9417d87227c7a56d22e471c6206462cba514c7590c09aff4cf6d1ddcad`
- Maintained codec: `bincode_reloaded` 3.1.15
- Maintained codec source: <https://crates.io/api/v1/crates/bincode_reloaded/3.1.15/download>
- Maintained codec archive SHA-256: `2e4ac690d35463a65215a28cbc1a0de736a2ed299113874f1a8cdf5d5adc231e`

## Declared local implementation

The package name is `syntect-bincode-compat`, while its library crate name is
`bincode` so syntect's existing public type path remains
`bincode::error::Result<T>`. The shim exposes only:

- the bincode 1 `error::Result`, `Error`, and `ErrorKind` compatibility types;
- `serialize_into` and `deserialize_from`;
- conversions from bincode_reloaded encode, decode, and I/O errors.

`ErrorKind` preserves bincode 1.3.3's exact variants, `Display` strings,
deprecated `description`/`cause` behavior, default `source()` behavior, and
`serde::ser::Error` plus `serde::de::Error` implementations. Golden tests cover
that contract without adding the advisory bincode package to any dependency
graph or checked-in lockfile.

All encoding and decoding delegates to the bincode_reloaded serde API using
`config::legacy()`. No package named `bincode` is introduced into Cargo.lock.
This nonpublishable shim is a root workspace member so its tests use the same
authoritative root lock and feature resolution as Cookie's release build. It
intentionally carries no standalone `Cargo.lock`.

## License

The local shim is MIT licensed; see `LICENSE.txt`. Its compatibility surface
was modeled on bincode 1.3.3, also MIT licensed. bincode_reloaded 3.1.15 is MIT
licensed.

## Integrity values

- `Cargo.toml` SHA-256: `d916ab792cde8e38e48712f65bcc665070b91f7db1c7911ab8118b3da7739f48`
- `src/lib.rs` SHA-256: `236269aa509f3a2a563c322a97a594a683791ec9785570ee2841eb55cbc98365`
- `LICENSE.txt` SHA-256: `423ba4d0c1feb0e4c121e829e332214ba7ada0be312fc88c309ebd6d2026869c`
- Three-file implementation tree SHA-256: `3ee8e7fff21fc93b36eda6debba78c7c16ade35a5121c5dd1d79f7aee1de44fc`

The implementation-tree digest excludes this README, sorts relative paths,
and hashes each path, NUL byte, file contents, and NUL byte in sequence.
