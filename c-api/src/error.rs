//! Error-code surface mirroring `zstd_errors.h`.
//!
//! Upstream encodes errors in the `size_t` return of most API functions: a
//! result `r` is an error when `r > (size_t)-ZSTD_error_maxCode`, and the
//! error code is recovered as `(ZSTD_ErrorCode)(0 - r)`. We reproduce that
//! encoding exactly so a consumer's existing `ZSTD_isError` / `ZSTD_getErrorCode`
//! calls behave identically against this library.

use core::ffi::{CStr, c_char, c_uint};

use codec::decoding::errors::FrameDecoderError;

/// Error codes from `zstd_errors.h` (upstream v1.5.7), numeric values pinned
/// since zstd v1.3.1. Exposed `#[repr(u32)]` so the discriminants are the
/// exact integers a C consumer compares against the `ZSTD_ErrorCode` enum.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[allow(non_camel_case_types)]
pub enum ZSTD_ErrorCode {
    ZSTD_error_no_error = 0,
    ZSTD_error_GENERIC = 1,
    ZSTD_error_prefix_unknown = 10,
    ZSTD_error_version_unsupported = 12,
    ZSTD_error_frameParameter_unsupported = 14,
    ZSTD_error_frameParameter_windowTooLarge = 16,
    ZSTD_error_corruption_detected = 20,
    ZSTD_error_checksum_wrong = 22,
    ZSTD_error_literals_headerWrong = 24,
    ZSTD_error_dictionary_corrupted = 30,
    ZSTD_error_dictionary_wrong = 32,
    ZSTD_error_dictionaryCreation_failed = 34,
    ZSTD_error_parameter_unsupported = 40,
    ZSTD_error_parameter_combination_unsupported = 41,
    ZSTD_error_parameter_outOfBound = 42,
    ZSTD_error_tableLog_tooLarge = 44,
    ZSTD_error_maxSymbolValue_tooLarge = 46,
    ZSTD_error_maxSymbolValue_tooSmall = 48,
    ZSTD_error_cannotProduce_uncompressedBlock = 49,
    ZSTD_error_stabilityCondition_notRespected = 50,
    ZSTD_error_stage_wrong = 60,
    ZSTD_error_init_missing = 62,
    ZSTD_error_memory_allocation = 64,
    ZSTD_error_workSpace_tooSmall = 66,
    ZSTD_error_dstSize_tooSmall = 70,
    ZSTD_error_srcSize_wrong = 72,
    ZSTD_error_dstBuffer_null = 74,
    ZSTD_error_noForwardProgress_destFull = 80,
    ZSTD_error_noForwardProgress_inputEmpty = 82,
    ZSTD_error_frameIndex_tooLarge = 100,
    ZSTD_error_seekableIO = 102,
    ZSTD_error_dstBuffer_wrong = 104,
    ZSTD_error_srcBuffer_wrong = 105,
    ZSTD_error_sequenceProducer_failed = 106,
    ZSTD_error_externalSequences_invalid = 107,
    ZSTD_error_maxCode = 120,
}

/// `(size_t)-ZSTD_error_maxCode`. A `size_t` result strictly greater than
/// this sentinel is an error (matches upstream `ERR_isError`).
const MAXCODE_SENTINEL: usize = 0usize.wrapping_sub(ZSTD_ErrorCode::ZSTD_error_maxCode as usize);

/// Encode a `ZSTD_ErrorCode` as the `size_t` error value upstream returns:
/// `(size_t)(0 - code)`. Used as the return of the `size_t`-typed wrappers.
#[inline]
pub fn encode(code: ZSTD_ErrorCode) -> usize {
    0usize.wrapping_sub(code as usize)
}

/// Whether a `size_t` function result is an error code. Mirrors
/// `ERR_isError`: `result > (size_t)-ZSTD_error_maxCode`.
#[inline]
pub fn result_is_error(result: usize) -> bool {
    result > MAXCODE_SENTINEL
}

/// Recover the `ZSTD_ErrorCode` from a `size_t` result. Returns
/// `ZSTD_error_no_error` for non-error results and `ZSTD_error_GENERIC` for an
/// error value that does not decode to a known code (cannot happen for values
/// this library produces, but keeps the mapping total for foreign inputs).
pub fn code_from_result(result: usize) -> ZSTD_ErrorCode {
    if !result_is_error(result) {
        return ZSTD_ErrorCode::ZSTD_error_no_error;
    }
    code_from_u32((0usize.wrapping_sub(result)) as u32)
}

fn code_from_u32(value: u32) -> ZSTD_ErrorCode {
    use ZSTD_ErrorCode::*;
    match value {
        0 => ZSTD_error_no_error,
        1 => ZSTD_error_GENERIC,
        10 => ZSTD_error_prefix_unknown,
        12 => ZSTD_error_version_unsupported,
        14 => ZSTD_error_frameParameter_unsupported,
        16 => ZSTD_error_frameParameter_windowTooLarge,
        20 => ZSTD_error_corruption_detected,
        22 => ZSTD_error_checksum_wrong,
        24 => ZSTD_error_literals_headerWrong,
        30 => ZSTD_error_dictionary_corrupted,
        32 => ZSTD_error_dictionary_wrong,
        34 => ZSTD_error_dictionaryCreation_failed,
        40 => ZSTD_error_parameter_unsupported,
        41 => ZSTD_error_parameter_combination_unsupported,
        42 => ZSTD_error_parameter_outOfBound,
        44 => ZSTD_error_tableLog_tooLarge,
        46 => ZSTD_error_maxSymbolValue_tooLarge,
        48 => ZSTD_error_maxSymbolValue_tooSmall,
        49 => ZSTD_error_cannotProduce_uncompressedBlock,
        50 => ZSTD_error_stabilityCondition_notRespected,
        60 => ZSTD_error_stage_wrong,
        62 => ZSTD_error_init_missing,
        64 => ZSTD_error_memory_allocation,
        66 => ZSTD_error_workSpace_tooSmall,
        70 => ZSTD_error_dstSize_tooSmall,
        72 => ZSTD_error_srcSize_wrong,
        74 => ZSTD_error_dstBuffer_null,
        80 => ZSTD_error_noForwardProgress_destFull,
        82 => ZSTD_error_noForwardProgress_inputEmpty,
        100 => ZSTD_error_frameIndex_tooLarge,
        102 => ZSTD_error_seekableIO,
        104 => ZSTD_error_dstBuffer_wrong,
        105 => ZSTD_error_srcBuffer_wrong,
        106 => ZSTD_error_sequenceProducer_failed,
        107 => ZSTD_error_externalSequences_invalid,
        _ => ZSTD_error_GENERIC,
    }
}

/// Map a decoder error to the closest stable `ZSTD_ErrorCode`. Conservative:
/// any variant without an exact upstream analogue (including the
/// feature-gated and future ones caught by the wildcard) collapses to
/// `corruption_detected`, the code upstream uses for a malformed frame.
pub fn code_for_decoder_error(err: &FrameDecoderError) -> ZSTD_ErrorCode {
    use ZSTD_ErrorCode::*;
    match err {
        FrameDecoderError::ReadFrameHeaderError(_) => ZSTD_error_prefix_unknown,
        FrameDecoderError::FrameHeaderError(_) | FrameDecoderError::FailedToInitialize(_) => {
            ZSTD_error_frameParameter_unsupported
        }
        FrameDecoderError::WindowSizeTooBig { .. } => ZSTD_error_frameParameter_windowTooLarge,
        FrameDecoderError::DictionaryDecodeError(_) => ZSTD_error_dictionary_corrupted,
        FrameDecoderError::DictNotProvided { .. }
        | FrameDecoderError::DictIdMismatch { .. }
        | FrameDecoderError::DictAlreadyRegistered { .. } => ZSTD_error_dictionary_wrong,
        FrameDecoderError::TargetTooSmall => ZSTD_error_dstSize_tooSmall,
        FrameDecoderError::FrameContentSizeMismatch { .. } => ZSTD_error_corruption_detected,
        FrameDecoderError::NotYetInitialized => ZSTD_error_init_missing,
        // Block-body / checksum / drain / skip failures and any
        // feature-gated or future variant: a malformed or unparseable frame.
        _ => ZSTD_error_corruption_detected,
    }
}

/// Static, NUL-terminated message for a code (the strings upstream's
/// `ZSTD_getErrorString` returns for the stable codes).
fn message(code: ZSTD_ErrorCode) -> &'static CStr {
    use ZSTD_ErrorCode::*;
    match code {
        ZSTD_error_no_error => c"No error detected",
        ZSTD_error_GENERIC => c"Error (generic)",
        ZSTD_error_prefix_unknown => c"Unknown frame descriptor",
        ZSTD_error_version_unsupported => c"Version not supported",
        ZSTD_error_frameParameter_unsupported => c"Unsupported frame parameter",
        ZSTD_error_frameParameter_windowTooLarge => c"Frame requires too much memory for decoding",
        ZSTD_error_corruption_detected => c"Data corruption detected",
        ZSTD_error_checksum_wrong => c"Restored data doesn't match checksum",
        ZSTD_error_literals_headerWrong => {
            c"Header of Literals' block doesn't respect format specification"
        }
        ZSTD_error_dictionary_corrupted => c"Dictionary is corrupted",
        ZSTD_error_dictionary_wrong => c"Dictionary mismatch",
        ZSTD_error_dictionaryCreation_failed => c"Cannot create Dictionary from provided samples",
        ZSTD_error_parameter_unsupported => c"Unsupported parameter",
        ZSTD_error_parameter_combination_unsupported => c"Unsupported combination of parameters",
        ZSTD_error_parameter_outOfBound => c"Parameter is out of bound",
        ZSTD_error_tableLog_tooLarge => c"tableLog requires too much memory : unsupported",
        ZSTD_error_maxSymbolValue_tooLarge => c"Unsupported max Symbol Value : too large",
        ZSTD_error_maxSymbolValue_tooSmall => c"Specified maxSymbolValue is too small",
        ZSTD_error_cannotProduce_uncompressedBlock => {
            c"This mode cannot generate an uncompressed block"
        }
        ZSTD_error_stabilityCondition_notRespected => {
            c"pinned buffer stability condition is not respected"
        }
        ZSTD_error_stage_wrong => c"Operation not authorized at current processing stage",
        ZSTD_error_init_missing => c"Context should be init first",
        ZSTD_error_memory_allocation => c"Allocation error : not enough memory",
        ZSTD_error_workSpace_tooSmall => c"workSpace buffer is not large enough",
        ZSTD_error_dstSize_tooSmall => c"Destination buffer is too small",
        ZSTD_error_srcSize_wrong => c"Src size is incorrect",
        ZSTD_error_dstBuffer_null => c"Operation on NULL destination buffer",
        ZSTD_error_noForwardProgress_destFull => {
            c"Operation made no progress over multiple calls, due to output buffer being full"
        }
        ZSTD_error_noForwardProgress_inputEmpty => {
            c"Operation made no progress over multiple calls, due to input being empty"
        }
        ZSTD_error_frameIndex_tooLarge => c"Frame index is too large",
        ZSTD_error_seekableIO => c"An I/O error occurred when reading/seeking",
        ZSTD_error_dstBuffer_wrong => c"Destination buffer is wrong",
        ZSTD_error_srcBuffer_wrong => c"Source buffer is wrong",
        ZSTD_error_sequenceProducer_failed => {
            c"Block-level external sequence producer returned an error code"
        }
        ZSTD_error_externalSequences_invalid => c"External sequences are not valid",
        ZSTD_error_maxCode => c"Unspecified error code",
    }
}

// ===== extern "C" surface =====

/// `unsigned ZSTD_isError(size_t result)` — non-zero iff `result` is an error.
///
/// # Safety
/// FFI boundary; no pointers dereferenced.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_isError(result: usize) -> c_uint {
    result_is_error(result) as c_uint
}

/// `ZSTD_ErrorCode ZSTD_getErrorCode(size_t functionResult)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_getErrorCode(function_result: usize) -> ZSTD_ErrorCode {
    code_from_result(function_result)
}

/// `const char* ZSTD_getErrorName(size_t result)` — readable string for a result.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_getErrorName(result: usize) -> *const c_char {
    message(code_from_result(result)).as_ptr()
}

/// `const char* ZSTD_getErrorString(ZSTD_ErrorCode code)` — readable string
/// for a code (the `zstd_errors.h` entry point).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_getErrorString(code: ZSTD_ErrorCode) -> *const c_char {
    message(code).as_ptr()
}
