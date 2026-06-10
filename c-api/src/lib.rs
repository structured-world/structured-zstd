//! C ABI front end for `structured-zstd` — a drop-in `libzstd` replacement.
//!
//! This crate exposes hand-written `extern "C"` wrappers whose signatures and
//! error semantics match upstream zstd v1.5.7 (the vendored headers under
//! `include/`), bottomed on the pure-Rust [`codec`] public API. It
//! builds as both a `cdylib` (SONAME `libzstd.so.1`) and a `staticlib`.
//!
//! Phase 6.1 scope: the synchronous slice of `zstd.h` — the simple one-shot
//! API, the synchronous context API, error-code mapping, frame content
//! inspection, and the stable dictionary-builder API (`ZDICT_*` from
//! `zdict.h`). Streaming, advanced parameters, the experimental
//! `ZDICT_STATIC_LINKING_ONLY` trainers, and the CLI land in later phases.
//!
//! Every wrapper here is `unsafe extern "C"`; the safety contracts mirror the
//! upstream documentation (valid `(ptr, len)` buffers, live context handles).
//!
//! This crate is intentionally std-only — it has no `no_std` prologue and no
//! `std` feature gate. It exists solely to emit a host shared object / static
//! archive (`libzstd.so.1` / `libzstd.a`) and every `extern "C"` wrapper guards
//! the boundary with [`std::panic::catch_unwind`], which is mandatory for
//! soundness: a Rust panic must not unwind into C. `catch_unwind` requires the
//! standard library, so the wrappers cannot build under `no_std`. The
//! `no_std + alloc` surface lives in the pure-Rust [`codec`] crate this depends
//! on; consumers wanting an embedded build link `codec` directly.

mod cdict;
mod context;
mod dict;
mod error;
mod ffi;
mod frame;
mod params;
mod simple;

#[cfg(test)]
mod tests;

// The `extern "C"` entry points are exported by their `#[no_mangle]` symbols;
// re-export the public types so rustdoc and in-crate tests can name them.
pub use cdict::{ZSTD_CDict, ZSTD_DDict};
pub use context::{ZSTD_CCtx, ZSTD_DCtx};
pub use dict::ZDICT_params_t;
pub use error::ZSTD_ErrorCode;
pub use frame::{ZSTD_FrameHeader, ZSTD_FrameType_e, ZSTD_format_e};
pub use params::ZSTD_bounds;
