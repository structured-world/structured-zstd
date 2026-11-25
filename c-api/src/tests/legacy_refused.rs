//! Without `legacy` a v0.5-v0.7 frame is not a frame, as libzstd 1.6.0 builds
//! it by default (`ZSTD_LEGACY_SUPPORT=0`): `ZSTD_isFrame` says no, the size
//! query fails, and decoding refuses the magic number.

use std::path::Path;

use crate::error::{ZSTD_ErrorCode, ZSTD_getErrorCode, ZSTD_isError};
use crate::frame::ZSTD_isFrame;
use crate::simple::{ZSTD_decompress, ZSTD_getFrameContentSize};

#[test]
fn a_v07_frame_is_refused_as_an_unknown_format() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../zstd/legacy_frames/abc.txt.v07.l1.zst");
    let frame = std::fs::read(path).expect("v0.7 fixture");
    let mut out = [0u8; 256];
    // SAFETY: every buffer is valid for the length passed with it.
    unsafe {
        assert_eq!(ZSTD_isFrame(frame.as_ptr().cast(), frame.len()), 0);
        assert_eq!(
            ZSTD_getFrameContentSize(frame.as_ptr().cast(), frame.len()),
            // `ZSTD_CONTENTSIZE_ERROR`, `(0ULL - 2)`.
            u64::MAX - 1
        );
        let rc = ZSTD_decompress(
            out.as_mut_ptr().cast(),
            out.len(),
            frame.as_ptr().cast(),
            frame.len(),
        );
        assert_ne!(ZSTD_isError(rc), 0);
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_prefix_unknown
        );
    }
}
