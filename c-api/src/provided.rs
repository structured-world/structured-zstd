//! Resources a caller hands in: thread pools (`ZSTD_createThreadPool`,
//! `ZSTD_CCtx_refThreadPool`) and workspaces to build a context inside
//! (`ZSTD_initStatic*`).
//!
//! Compression here runs on the calling thread, as in upstream's
//! single-threaded build, whose thread pool is a placeholder that every
//! context accepts and none uses. The contexts own heap allocations that a
//! caller's workspace cannot hold, so every `ZSTD_initStatic*` answers `NULL`,
//! the value upstream gives for a workspace it cannot use.

use core::ffi::c_void;

use crate::cdict::{ZSTD_CDict, ZSTD_DDict, ZSTD_compressionParameters};
use crate::context::{ZSTD_CCtx, ZSTD_DCtx};
use crate::error::{ZSTD_ErrorCode, encode};

/// Opaque `ZSTD_threadPool`.
#[allow(non_camel_case_types)]
pub struct ZSTD_threadPool {
    _placeholder: u8,
}

/// The one pool every [`ZSTD_createThreadPool`] hands out.
static POOL: ZSTD_threadPool = ZSTD_threadPool { _placeholder: 0 };

/// `ZSTD_threadPool* ZSTD_createThreadPool(size_t numThreads)` — a pool for
/// [`ZSTD_CCtx_refThreadPool`]. Never `NULL`; frames are compressed on the
/// calling thread whatever the pool, as upstream's single-threaded build does.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createThreadPool(_num_threads: usize) -> *mut ZSTD_threadPool {
    (&raw const POOL).cast_mut()
}

/// `void ZSTD_freeThreadPool(ZSTD_threadPool* pool)` — nothing to free.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_freeThreadPool(_pool: *mut ZSTD_threadPool) {}

/// `size_t ZSTD_CCtx_refThreadPool(ZSTD_CCtx* cctx, ZSTD_threadPool* pool)` —
/// accepted between frames (`stage_wrong` within one, as upstream), without
/// effect: there are no workers to run on it.
///
/// # Safety
/// `cctx` must be live or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_refThreadPool(
    cctx: *mut ZSTD_CCtx,
    _pool: *mut ZSTD_threadPool,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_ref() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    if cctx.stream_in_progress() {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    0
}

/// `ZSTD_CCtx* ZSTD_initStaticCCtx(void* workspace, size_t workspaceSize)` —
/// `NULL`: a context cannot be built inside a caller's workspace.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_initStaticCCtx(
    _workspace: *mut c_void,
    _workspace_size: usize,
) -> *mut ZSTD_CCtx {
    core::ptr::null_mut()
}

/// `ZSTD_CStream* ZSTD_initStaticCStream(void* workspace, size_t
/// workspaceSize)` — `NULL`, as [`ZSTD_initStaticCCtx`].
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_initStaticCStream(
    _workspace: *mut c_void,
    _workspace_size: usize,
) -> *mut ZSTD_CCtx {
    core::ptr::null_mut()
}

/// `ZSTD_DCtx* ZSTD_initStaticDCtx(void* workspace, size_t workspaceSize)` —
/// `NULL`, as [`ZSTD_initStaticCCtx`].
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_initStaticDCtx(
    _workspace: *mut c_void,
    _workspace_size: usize,
) -> *mut ZSTD_DCtx {
    core::ptr::null_mut()
}

/// `ZSTD_DStream* ZSTD_initStaticDStream(void* workspace, size_t
/// workspaceSize)` — `NULL`, as [`ZSTD_initStaticCCtx`].
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_initStaticDStream(
    _workspace: *mut c_void,
    _workspace_size: usize,
) -> *mut ZSTD_DCtx {
    core::ptr::null_mut()
}

/// `const ZSTD_CDict* ZSTD_initStaticCDict(void* workspace, size_t
/// workspaceSize, const void* dict, size_t dictSize, ZSTD_dictLoadMethod_e
/// dictLoadMethod, ZSTD_dictContentType_e dictContentType,
/// ZSTD_compressionParameters cParams)` — `NULL`, as [`ZSTD_initStaticCCtx`].
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_initStaticCDict(
    _workspace: *mut c_void,
    _workspace_size: usize,
    _dict: *const c_void,
    _dict_size: usize,
    _dict_load_method: core::ffi::c_int,
    _dict_content_type: core::ffi::c_int,
    _cparams: ZSTD_compressionParameters,
) -> *const ZSTD_CDict {
    core::ptr::null()
}

/// `const ZSTD_DDict* ZSTD_initStaticDDict(void* workspace, size_t
/// workspaceSize, const void* dict, size_t dictSize, ZSTD_dictLoadMethod_e
/// dictLoadMethod, ZSTD_dictContentType_e dictContentType)` — `NULL`, as
/// [`ZSTD_initStaticCCtx`].
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_initStaticDDict(
    _workspace: *mut c_void,
    _workspace_size: usize,
    _dict: *const c_void,
    _dict_size: usize,
    _dict_load_method: core::ffi::c_int,
    _dict_content_type: core::ffi::c_int,
) -> *const ZSTD_DDict {
    core::ptr::null()
}
