//! Empty support crate for the C-binding benches, conformance tests, and FFI
//! diagnostic examples. All real content lives in the `[[bench]]` / `[[test]]`
//! / `[[example]]` targets declared in `Cargo.toml`, which reference the source
//! files under `../zstd` directly so their fixtures and the shared
//! `benches/support` module resolve in place.
