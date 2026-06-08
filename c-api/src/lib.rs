//! C ABI front end for `structured-zstd` — a drop-in `libzstd` replacement.
//!
//! This crate exposes hand-written `extern "C"` wrappers whose signatures and
//! error semantics match upstream zstd v1.5.7 (the vendored headers under
//! `include/`), bottomed on the pure-Rust [`structured_zstd`] public API. It
//! builds as both a `cdylib` (SONAME `libzstd.so.1`) and a `staticlib`.
//!
//! Phase 6.1 scope: the synchronous slice of `zstd.h` — the simple one-shot
//! API, the synchronous context API, error-code mapping, and frame content
//! inspection. Streaming, advanced parameters, dictionaries, and the CLI land
//! in later phases.
//!
//! Every wrapper here is `unsafe extern "C"`; the safety contracts mirror the
//! upstream documentation (valid `(ptr, len)` buffers, live context handles).

mod context;
mod error;
mod ffi;
mod frame;
mod simple;

#[cfg(test)]
mod tests;

// The `extern "C"` entry points are exported by their `#[no_mangle]` symbols;
// re-export the public types so rustdoc and in-crate tests can name them.
pub use context::{ZSTD_CCtx, ZSTD_DCtx};
pub use error::ZSTD_ErrorCode;
pub use frame::{ZSTD_FrameHeader, ZSTD_FrameType_e, ZSTD_format_e};
