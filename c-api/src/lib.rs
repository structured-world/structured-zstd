//! C ABI front end for `structured-zstd` — a drop-in `libzstd` replacement.
//!
//! This crate exposes hand-written `extern "C"` wrappers whose signatures and
//! error semantics match upstream zstd v1.5.7 (the vendored headers under
//! `include/`), bottomed on the pure-Rust [`codec`] public API. It
//! builds as both a `cdylib` (SONAME `libzstd.so.1`) and a `staticlib`.
//!
//! Scope: every function `zstd.h`, `zdict.h` and `zstd_errors.h` declare,
//! their `*_STATIC_LINKING_ONLY` sections included: the simple and context
//! APIs, streaming, dictionaries and the dictionary builders, the advanced
//! and experimental parameters, parameter sets, the buffer-less and block
//! APIs, sequences and sequence producers, frame inspection and the
//! `ZSTD_estimate*` budget queries. Thread pools are accepted without
//! effect, as in upstream's single-threaded build, and `ZSTD_initStatic*`
//! answer `NULL`: a context cannot live in a caller's workspace.
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

mod attach;
mod bufferless;
mod cctx_params;
mod cdict;
mod context;
mod dict;
mod error;
mod estimate;
mod ffi;
mod frame;
mod params;
mod provided;
mod sequences;
mod simple;
mod streaming;

#[cfg(test)]
mod tests;

// The `extern "C"` entry points are exported by their `#[no_mangle]` symbols;
// re-export the public types so rustdoc and in-crate tests can name them.
pub use cdict::{ZSTD_CDict, ZSTD_DDict};
pub use context::{ZSTD_CCtx, ZSTD_DCtx};
pub use dict::{ZDICT_legacy_params_t, ZDICT_params_t};
pub use error::ZSTD_ErrorCode;
pub use frame::{ZSTD_FrameHeader, ZSTD_FrameType_e, ZSTD_format_e};
pub use params::ZSTD_bounds;
pub use provided::ZSTD_threadPool;
pub use sequences::{ZSTD_Sequence, ZSTD_sequenceProducer_F};
pub use streaming::{ZSTD_inBuffer, ZSTD_outBuffer};
