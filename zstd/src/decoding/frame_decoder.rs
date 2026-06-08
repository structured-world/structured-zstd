//! Framedecoder is the main low-level struct users interact with to decode zstd frames
//!
//! Zstandard compressed data is made of one or more frames. Each frame is independent and can be
//! decompressed independently of other frames. This module contains structures
//! and utilities that can be used to decode a frame.

use super::frame;
use crate::decoding;
use crate::decoding::block_decoder::BlockDecoder;
use crate::decoding::buffer_backend::BufferBackend;
use crate::decoding::dictionary::{Dictionary, DictionaryHandle};
use crate::decoding::errors::{DecodeBlockContentError, FrameDecoderError};
use crate::decoding::flat_buf::FlatBuf;
use crate::decoding::ringbuffer::RingBuffer;
use crate::decoding::scratch::DecoderScratch;
use crate::io::{Error, Read, Write};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::convert::TryInto;

use crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE;

/// Build the block-header decode error. With the `lsm` feature it captures
/// the failing block's index and frame offset (block-precise recovery);
/// without it, the legacy positionless variant — so the default build's
/// error surface stays byte-identical to the donor.
#[cfg(feature = "lsm")]
fn block_header_decode_error(
    source: crate::decoding::errors::BlockHeaderReadError,
    block_index: u32,
    frame_offset: u32,
) -> FrameDecoderError {
    FrameDecoderError::FailedToReadBlockHeaderAt {
        source,
        block_index,
        frame_offset,
    }
}
#[cfg(not(feature = "lsm"))]
fn block_header_decode_error(
    source: crate::decoding::errors::BlockHeaderReadError,
    _block_index: u32,
    _frame_offset: u32,
) -> FrameDecoderError {
    FrameDecoderError::FailedToReadBlockHeader(source)
}

/// Build the block-body decode error. With `lsm` it captures the block
/// index, frame offset, and the failing block's structural metadata
/// (reconstructed from its header); without it, the legacy variant.
#[cfg(feature = "lsm")]
fn block_body_decode_error(
    source: DecodeBlockContentError,
    block_index: u32,
    frame_offset: u32,
    header: &crate::blocks::block::BlockHeader,
    header_size: u8,
) -> FrameDecoderError {
    use crate::blocks::block::BlockType;
    // Physical wire body vs the raw `Block_Size` field: RLE writes a single
    // body byte while `Block_Size` carries the repeat count; Raw/Compressed
    // bodies match the field.
    let (body_size, block_size_field) = match header.block_type {
        BlockType::RLE => (1u32, header.decompressed_size),
        _ => (header.content_size, header.content_size),
    };
    FrameDecoderError::FailedToReadBlockBodyAt {
        source,
        block_index,
        frame_offset,
        block: crate::encoding::frame_emit_info::FrameBlock {
            offset_in_frame: frame_offset,
            header_size,
            body_size,
            block_size_field,
            block_type: header.block_type,
            last_block: header.last_block,
            // Raw/RLE carry their regenerated size in the header;
            // a Compressed block's is unknown until decoded, so
            // `read_block_header` leaves `decompressed_size` 0 here.
            decompressed_size: header.decompressed_size,
        },
    }
}
#[cfg(not(feature = "lsm"))]
fn block_body_decode_error(
    source: DecodeBlockContentError,
    _block_index: u32,
    _frame_offset: u32,
    _header: &crate::blocks::block::BlockHeader,
    _header_size: u8,
) -> FrameDecoderError {
    FrameDecoderError::FailedToReadBlockBody(source)
}

/// Low level Zstandard decoder that can be used to decompress frames with fine control over when and how many bytes are decoded.
///
/// This decoder is able to decode frames only partially and gives control
/// over how many bytes/blocks will be decoded at a time (so you don't have to decode a 10GB file into memory all at once).
/// It reads bytes as needed from a provided source and can be read from to collect partial results.
///
/// If you want to just read the whole frame with an `io::Read` without having to deal with manually calling [FrameDecoder::decode_blocks]
/// you can use the provided [crate::decoding::StreamingDecoder] wich wraps this FrameDecoder.
///
/// Workflow is as follows:
/// ```
/// use structured_zstd::decoding::BlockDecodingStrategy;
///
/// # #[cfg(feature = "std")]
/// use std::io::{Read, Write};
///
/// // no_std environments can use the crate's own Read traits
/// # #[cfg(not(feature = "std"))]
/// use structured_zstd::io::{Read, Write};
///
/// fn decode_this(mut file: impl Read) {
///     //Create a new decoder
///     let mut frame_dec = structured_zstd::decoding::FrameDecoder::new();
///     let mut result = Vec::new();
///
///     // Use reset or init to make the decoder ready to decode the frame from the io::Read
///     frame_dec.reset(&mut file).unwrap();
///
///     // Loop until the frame has been decoded completely
///     while !frame_dec.is_finished() {
///         // decode (roughly) batch_size many bytes
///         frame_dec.decode_blocks(&mut file, BlockDecodingStrategy::UptoBytes(1024)).unwrap();
///
///         // read from the decoder to collect bytes from the internal buffer
///         let bytes_read = frame_dec.read(result.as_mut_slice()).unwrap();
///
///         // then do something with it
///         do_something(&result[0..bytes_read]);
///     }
///
///     // handle the last chunk of data
///     while frame_dec.can_collect() > 0 {
///         let x = frame_dec.read(result.as_mut_slice()).unwrap();
///
///         do_something(&result[0..x]);
///     }
/// }
///
/// fn do_something(data: &[u8]) {
/// # #[cfg(feature = "std")]
///     std::io::stdout().write_all(data).unwrap();
/// }
/// ```
pub struct FrameDecoder {
    state: Option<FrameDecoderState>,
    // Registered dictionaries are stored by shared handle (Arc/Rc) so a
    // single content copy is referenced by every frame the decoder decodes
    // (donor `ZSTD_refDDict`), rather than re-copied into the decode buffer
    // per frame. `add_dict` wraps an owned `Dictionary` into a handle.
    owned_dicts: BTreeMap<u32, DictionaryHandle>,
    #[cfg(target_has_atomic = "ptr")]
    shared_dicts: BTreeMap<u32, DictionaryHandle>,
    #[cfg(not(target_has_atomic = "ptr"))]
    shared_dicts: (),
    /// `ZSTD_f_zstd1_magicless` — when true, [`init`] / [`reset`]
    /// expect frames without the 4-byte magic number prefix.
    /// Default false (standard zstd format).
    magicless: bool,
    /// How the optional content checksum is handled. Default
    /// [`ContentChecksum::EmitOnly`] (compute + expose, no error on
    /// mismatch). Set via [`Self::set_content_checksum`].
    content_checksum: ContentChecksum,
    /// Pinned `Dictionary_ID` expectation set via
    /// [`Self::expect_dict_id`]. `None` (default) disables the
    /// check; `Some(0)` matches frames whose header omits the
    /// optional dict_id (treated as "no dictionary"). Validated in
    /// [`Self::reset`] AFTER the frame header parses successfully
    /// and BEFORE any block decode work.
    #[cfg(feature = "lsm")]
    expect_dict_id: Option<u32>,
    /// Pinned `Window_Descriptor` byte expectation set via
    /// [`Self::expect_window_descriptor`]. `None` (default)
    /// disables the check. Validated in [`Self::reset`] AFTER the
    /// frame header parses successfully and BEFORE any block
    /// decode work. Single-segment frames (which omit the
    /// `Window_Descriptor` byte from the wire) surface as
    /// [`crate::decoding::errors::FrameDecoderError::UnexpectedWindowDescriptor`]
    /// with `found: None`.
    #[cfg(feature = "lsm")]
    expect_window_descriptor: Option<u8>,
    /// When `true`, the per-block decode loop XXH64-hashes each
    /// block's decompressed bytes and stores the low-32-bit digest in
    /// [`Self::computed_block_checksums`]. Default `false` (zero
    /// cost). Set via [`Self::enable_per_block_checksums`]. Gated on
    /// `all(lsm, hash)` because XXH64 lives behind the `hash`
    /// feature.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    per_block_checksums_enabled: bool,
    /// Per-block XXH64 (low 32 bits) digests captured during the
    /// current frame's decode when `per_block_checksums_enabled` is
    /// set. Reset at the start of every new frame. Gated on
    /// `all(lsm, hash)` (see `per_block_checksums_enabled`).
    #[cfg(all(feature = "lsm", feature = "hash"))]
    computed_block_checksums: alloc::vec::Vec<u32>,
}

/// How the decoder treats a frame's optional XXH64 content checksum
/// (RFC 8878 Content_Checksum_flag). The XXH64 pass over the decompressed
/// output is a measurable share of decode time, so it is made skippable.
///
/// ```
/// use structured_zstd::decoding::{ContentChecksum, FrameDecoder};
/// let mut decoder = FrameDecoder::new();
/// decoder.set_content_checksum(ContentChecksum::Verify);
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ContentChecksum {
    /// Skip the XXH64 pass entirely: no compute, no verify.
    /// `get_calculated_checksum()` returns `None`.
    None,
    /// Compute the checksum and expose it via the accessors, but do not
    /// error on a mismatch. This is the default and matches the historical
    /// behaviour (callers verify manually if they wish).
    #[default]
    EmitOnly,
    /// Compute the checksum and compare it against the frame's stored value;
    /// a disagreement fails the decode with
    /// [`FrameDecoderError::ChecksumMismatch`](crate::decoding::errors::FrameDecoderError::ChecksumMismatch).
    /// Without the `hash` feature there is no way to compute a digest, so
    /// `Verify` cannot detect a mismatch and behaves like `None`.
    Verify,
}

/// Decode-relevant identity of a frame, used to reject a [`ResumeState`]
/// captured from one frame being applied to a frame of a different shape. Covers
/// every header field that changes how blocks decode (buffer sizing, backend
/// kind, entropy/dictionary context, trailing-checksum handling, declared
/// content size, magicless framing).
///
/// This is a SHAPE guard, not a content-unique fingerprint: two distinct frames
/// that happen to share all these header fields produce the same key (no cheap
/// header field uniquely identifies frame content). It catches the realistic
/// accidental misuse — applying a snapshot to a frame with a different
/// window/dictionary/size — with a typed error instead of byte-wrong output.
/// Pairing a `ResumeState` with the correct frame's compressed source and
/// `window_prime` remains the caller's contract.
#[cfg(feature = "lsm")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FrameKey {
    window_size: u64,
    frame_content_size: u64,
    /// `Dictionary_ID` declared in the frame header (`None` when omitted).
    dictionary_id: Option<u32>,
    /// Dictionary actually applied to the decoder (`state.using_dict`). This is
    /// distinct from `dictionary_id`: a frame with a dictless header can still
    /// be decoded with an explicit dictionary via `reset_with_dict_handle` /
    /// `force_dict`, and two such decodes with different dictionaries must NOT
    /// compare equal — keying only on the header field would miss that.
    active_dictionary_id: Option<u32>,
    single_segment: bool,
    content_checksum: bool,
    magicless: bool,
}

#[cfg(feature = "lsm")]
impl FrameKey {
    fn from_state(state: &FrameDecoderState, magicless: bool) -> FrameKey {
        let header = &state.frame_header;
        FrameKey {
            window_size: header.window_size().unwrap_or(0),
            frame_content_size: header.frame_content_size(),
            dictionary_id: header.dictionary_id(),
            active_dictionary_id: state.using_dict,
            single_segment: header.descriptor.single_segment_flag(),
            content_checksum: header.descriptor.content_checksum_flag(),
            magicless,
        }
    }
}

/// XXH64 of a contiguous byte slice — the resume-side counterpart to
/// [`DecoderScratchKind::window_tail_hash`]. Streaming XXH64 is chunk-boundary
/// independent, so this single-slice hash equals the emit-side two-slice hash
/// over the same bytes.
#[cfg(all(feature = "lsm", feature = "hash"))]
fn xxh64_of(bytes: &[u8]) -> u64 {
    use core::hash::Hasher;
    let mut h = twox_hash::XxHash64::with_seed(0);
    h.write(bytes);
    h.finish()
}

/// Cross-block decode state needed to resume a cold partial decode at an inner
/// block boundary, emitted by [`FrameDecoder::decode_blocks_partial`] when its
/// `emit_resume` argument is `true` (returned in
/// [`PartialDecode::resume_state`]) and fed back via that same method's
/// [`resume`](FrameDecoder::decode_blocks_partial) argument
/// ([`ResumeInput`]).
///
/// A zstd block does not carry all the state required to decode it in
/// isolation: besides the shared match window (the decompressed output history),
/// a Compressed block may reuse the previous block's entropy tables via
/// `Repeat_Mode` (literals Huffman + the LL/OF/ML FSE distributions) and always
/// continues the running repeat-offset history. This snapshot carries exactly
/// that carry-over state plus the resume coordinates, so resuming is
/// byte-identical to a contiguous decode even across a dropped decoder. The
/// window itself is NOT stored here — the caller supplies it back through
/// [`ResumeInput::window_prime`] from the decompressed output it already
/// persists. Neither is the dictionary: for a dictionary frame the caller
/// re-attaches it to the resuming decoder via [`FrameDecoder::reset`] /
/// [`FrameDecoder::reset_with_dict_handle`] (it already holds the dictionary
/// from encode time), and the snapshot records only the dictionary's identity
/// so a resume under a different dictionary is rejected.
///
/// Behind the `lsm` Cargo feature.
#[cfg(feature = "lsm")]
#[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
pub struct ResumeState {
    /// Identity of the frame this state was captured from. Compared against the
    /// frame currently reset into the decoder before any state is restored, so a
    /// snapshot from a different frame shape is rejected with
    /// [`FrameDecoderError::ResumeFrameMismatch`] instead of silently producing
    /// byte-wrong output.
    frame_key: FrameKey,
    /// Index of the block to resume AT (the first block NOT yet decoded).
    block_index: u32,
    /// Cumulative decompressed byte count produced before `block_index`.
    output_offset: u64,
    /// FSE tables (LL/OF/ML) as of the last decoded block — the source for a
    /// `Repeat_Mode` resume block.
    fse: crate::decoding::scratch::FSEScratch,
    /// Huffman literals table as of the last decoded block — the source for a
    /// treeless (repeat) literals resume block.
    huf: crate::decoding::scratch::HuffmanScratch,
    /// Running repeat-offset history (`offset_hist`) as of the last decoded
    /// block.
    offset_hist: [u32; 3],
    /// XXH64 of the exact window-prime bytes (the last `min(window_size,
    /// output_offset)` decompressed bytes) captured at emit. Verified at resume
    /// against the caller-supplied [`ResumeInput::window_prime`]: a content
    /// mismatch (wrong frame, wrong or corrupted prime) is a near-unique
    /// (≈2⁻⁶⁴) signal and is rejected with
    /// [`FrameDecoderError::ResumeFrameMismatch`]. This is the content-exact
    /// guard; [`FrameKey`] is the cheap shape pre-check that works without the
    /// `hash` feature. Behind `all(lsm, hash)`.
    #[cfg(feature = "hash")]
    window_hash: u64,
}

#[cfg(feature = "lsm")]
impl ResumeState {
    /// Inner block index this state resumes at (the first block not yet
    /// decoded). Pass it as the `end_block` lower bound (and as `start_block`)
    /// of the resuming
    /// [`decode_blocks_partial`](FrameDecoder::decode_blocks_partial) call.
    pub fn block_index(&self) -> u32 {
        self.block_index
    }

    /// Cumulative decompressed byte count produced before
    /// [`block_index`](Self::block_index) — i.e. the decompressed offset at
    /// which the resumed output begins. Equals
    /// `FrameEmitInfo::decompressed_byte_range(block_index).start`. Use it to
    /// slice the `window_prime` tail the resumed call needs.
    pub fn output_offset(&self) -> u64 {
        self.output_offset
    }
}

// Manual Debug: the entropy tables are large internal scratch with no useful
// Debug surface; only the resume coordinates are worth printing (and this lets
// `PartialDecode` keep its derived Debug).
#[cfg(feature = "lsm")]
impl core::fmt::Debug for ResumeState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ResumeState")
            .field("block_index", &self.block_index)
            .field("output_offset", &self.output_offset)
            .finish_non_exhaustive()
    }
}

/// Resume input fed to [`FrameDecoder::decode_blocks_partial`]'s `resume`
/// argument to continue a cold partial decode without re-decompressing the
/// preceding blocks.
///
/// Behind the `lsm` Cargo feature.
#[cfg(feature = "lsm")]
#[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
pub struct ResumeInput<'a> {
    /// The caller's already-decompressed output ending just before
    /// [`ResumeState::block_index`]. Must contain at least the last
    /// `min(window_size, output_offset)` bytes (a full match window, or the
    /// whole prefix when it is shorter than one window); anything beyond the
    /// last `window_size` bytes is ignored, so passing the entire prefix is
    /// also valid (capped internally, bounding resume memory to one window).
    pub window_prime: &'a [u8],
    /// Cross-block entropy/repcode state emitted by the prior
    /// [`decode_blocks_partial`](FrameDecoder::decode_blocks_partial) call.
    pub state: &'a ResumeState,
}

/// Backend-tagged decode scratch — chosen at frame-reset time based
/// on the parsed `FrameHeader.descriptor.single_segment_flag()` and
/// kept stable through the lifetime of the frame. The match in each
/// helper below dispatches **once per call** (e.g. once per block in
/// `decode_block_content`, once per drain in `drain_to_writer`) —
/// never inside the hot push/repeat loop, which is fully
/// monomorphised through the `DecoderScratch<B>` generic.
enum DecoderScratchKind {
    Ring(DecoderScratch<RingBuffer>),
    Flat(DecoderScratch<FlatBuf>),
}

impl DecoderScratchKind {
    fn new_ring(window_size: usize) -> Self {
        // Lazy ring-buffer allocation: do NOT `reserve(window_size)` here.
        // The direct-decode path (`run_direct_decode`) writes through
        // `UserSliceBackend` and never touches the ring; allocating it
        // eagerly wastes one full window of peak memory on the common
        // direct-eligible frame. On the non-direct path the window is
        // pre-reserved once at frame entry (`decode_all_impl` and
        // `decode_blocks` both call `DecoderScratchKind::reserve_buffer`
        // before any block writes), so multi-block frames pay one
        // amortised grow instead of repeated `reserve_amortized` steps
        // per block. Issue #279 round 2.
        let s = DecoderScratch::<RingBuffer>::new(window_size);
        Self::Ring(s)
    }

    /// Construct a flat-backed scratch for a single-segment frame.
    /// `frame_content_size` is the upcoming output size in bytes
    /// (== `window_size` when the flag is set).
    ///
    /// Lazy buffer allocation (mirrors [`Self::new_ring`]): do NOT
    /// pre-size the `FlatBuf`. The direct-decode path
    /// (`run_direct_decode`) writes through `UserSliceBackend` and never
    /// touches this buffer, so eagerly allocating a full FCS wastes one
    /// whole content-size of peak memory on the common direct-eligible
    /// single-segment frame. The non-direct fallback reserves it once via
    /// `reserve_buffer(window_size)` at frame entry before any block
    /// write (`FlatBuf::reserve` adds the `WILDCOPY_OVERLENGTH` slack),
    /// and every inline-exec site (trait method and per-kernel macros)
    /// now carries a tight-tail bounded copy, so a tight buffer can never
    /// overshoot regardless of construction-time slack.
    fn new_flat(frame_content_size: usize) -> Self {
        let s = DecoderScratch::<FlatBuf>::new(frame_content_size);
        Self::Flat(s)
    }

    /// Reset (or transition between) backends for a new frame.
    /// Reuses the existing `DecoderScratch` allocations (FSE / HUF
    /// tables, sequence vec, etc.) when the backend kind is unchanged
    /// — only the underlying buffer is re-sized for the new frame.
    /// Building a fresh `DecoderScratch` on every frame would
    /// re-allocate everything and was measured at +255 % vs ring on
    /// small frames; reusing it keeps the small-frame cost flat.
    fn reset(&mut self, frame: &frame::FrameHeader, window_size: usize) {
        if frame.descriptor.single_segment_flag() {
            match self {
                Self::Flat(s) => {
                    s.reset(window_size);
                    // `DecoderScratch::reset` clears the backing buffer and
                    // updates `window_size` WITHOUT reserving it (it may still
                    // resize the per-block scratch Vecs up to
                    // `min(window_size, MAX_BLOCK_SIZE)`). Backing-buffer
                    // capacity is decided one layer up: direct-eligible frames
                    // never touch it, and the non-direct path pre-reserves once
                    // via `reserve_buffer(window_size)` at frame entry.
                }
                Self::Ring(_) => *self = Self::new_flat(window_size),
            }
        } else {
            match self {
                Self::Ring(s) => s.reset(window_size),
                Self::Flat(_) => *self = Self::new_ring(window_size),
            }
        }
    }

    fn init_from_dict(&mut self, dict: &DictionaryHandle) {
        match self {
            Self::Ring(s) => s.init_from_dict(dict),
            Self::Flat(s) => s.init_from_dict(dict),
        }
    }

    #[inline]
    fn buffer_len(&self) -> usize {
        match self {
            Self::Ring(s) => s.buffer.len(),
            Self::Flat(s) => s.buffer.len(),
        }
    }

    /// Pre-reserve the backing buffer to `window_size` in a single
    /// allocation. Called once on the non-direct (`decode_blocks`) path
    /// after direct-eligibility is ruled out, so multi-segment fallback
    /// decodes don't pay repeated `reserve_amortized` grow steps
    /// (128 KiB → 256 KiB → ... → window) as blocks accumulate.
    ///
    /// Direct-eligible frames never call this and pay zero backing-buffer
    /// allocation for the window, on BOTH backends: `new_ring` and
    /// `new_flat` are each lazy (no pre-reserve), so a direct-eligible
    /// frame writes only through `UserSliceBackend` and leaves this
    /// buffer empty.
    /// `window_size` is the ADDITIONAL-bytes argument the backend `reserve`
    /// contract expects (Vec-style `reserve(additional)` → `capacity >= len +
    /// additional`, plus the backend's wildcopy slack). This is always called at
    /// frame entry on a freshly-reset buffer (`len == 0`), so the additional
    /// request equals the desired total window capacity. Do NOT pass
    /// `window_size - buffer.len()`: when `len == 0` it's identical, and the
    /// backend `reserve` impls deliberately reject that form because it
    /// under-reserves on reused buffers (see `FlatBuf::reserve`).
    #[inline]
    fn reserve_buffer(&mut self, window_size: usize) {
        match self {
            Self::Ring(s) => s.buffer.reserve(window_size),
            Self::Flat(s) => s.buffer.reserve(window_size),
        }
    }

    /// Last `n` bytes of the visible buffer as `(s1, s2)` (wrap-aware).
    /// Routes through whichever backend the current scratch holds.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    fn last_n_as_slices(&self, n: usize) -> (&[u8], &[u8]) {
        match self {
            Self::Ring(s) => s.buffer.last_n_as_slices(n),
            Self::Flat(s) => s.buffer.last_n_as_slices(n),
        }
    }

    fn buffer_drain(&mut self) -> Vec<u8> {
        match self {
            Self::Ring(s) => s.buffer.drain(),
            Self::Flat(s) => s.buffer.drain(),
        }
    }

    fn buffer_drain_to_window_size(&mut self) -> Option<Vec<u8>> {
        match self {
            Self::Ring(s) => s.buffer.drain_to_window_size(),
            Self::Flat(s) => s.buffer.drain_to_window_size(),
        }
    }

    fn buffer_drain_to_writer(&mut self, sink: impl Write) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.drain_to_writer(sink),
            Self::Flat(s) => s.buffer.drain_to_writer(sink),
        }
    }

    fn buffer_drain_to_window_size_writer(&mut self, sink: impl Write) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.drain_to_window_size_writer(sink),
            Self::Flat(s) => s.buffer.drain_to_window_size_writer(sink),
        }
    }

    fn buffer_can_drain(&self) -> usize {
        match self {
            Self::Ring(s) => s.buffer.can_drain(),
            Self::Flat(s) => s.buffer.can_drain(),
        }
    }

    fn buffer_can_drain_to_window_size(&self) -> Option<usize> {
        match self {
            Self::Ring(s) => s.buffer.can_drain_to_window_size(),
            Self::Flat(s) => s.buffer.can_drain_to_window_size(),
        }
    }

    fn buffer_read(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.read(target),
            Self::Flat(s) => s.buffer.read(target),
        }
    }

    fn buffer_read_all(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.read_all(target),
            Self::Flat(s) => s.buffer.read_all(target),
        }
    }

    /// Drop visible output beyond `window_size` without producing it,
    /// keeping the most recent `window_size` bytes available to back
    /// future match copies. Used by `decode_blocks_partial` to bound
    /// memory while decoding the leading (skipped) blocks into the window.
    #[cfg(feature = "lsm")]
    fn buffer_drop_to_window_size(&mut self) -> usize {
        match self {
            Self::Ring(s) => s.buffer.drop_to_window_size(),
            Self::Flat(s) => s.buffer.drop_to_window_size(),
        }
    }

    /// Drop exactly `n` bytes from the front of the visible output without
    /// producing them. Used by `decode_blocks_partial` to discard the
    /// leading blocks' window-context bytes once the in-range blocks are
    /// decoded (match resolution complete), leaving only the in-range output.
    #[cfg(feature = "lsm")]
    fn buffer_discard_front(&mut self, n: usize) {
        match self {
            Self::Ring(s) => s.buffer.discard_front(n),
            Self::Flat(s) => s.buffer.discard_front(n),
        }
    }

    /// Prime the match window with the caller's already-decompressed tail for
    /// a resumed partial decode. Routes through whichever backend the current
    /// scratch holds. See [`DecodeBuffer::prime_window`].
    #[cfg(feature = "lsm")]
    fn prime_window(&mut self, prefix: &[u8], total_output: u64) {
        match self {
            Self::Ring(s) => s.buffer.prime_window(prefix, total_output),
            Self::Flat(s) => s.buffer.prime_window(prefix, total_output),
        }
    }

    /// Total decompressed bytes produced so far (the buffer's running output
    /// counter, unaffected by window drops / drains). Used to stamp a captured
    /// [`ResumeState`]'s `output_offset`.
    #[cfg(feature = "lsm")]
    fn total_output(&self) -> u64 {
        match self {
            Self::Ring(s) => s.buffer.total_output(),
            Self::Flat(s) => s.buffer.total_output(),
        }
    }

    /// Clone the cross-block entropy/repcode state (FSE + Huffman tables +
    /// `offset_hist`) out of the live scratch for a [`ResumeState`] snapshot.
    #[cfg(feature = "lsm")]
    fn export_entropy(
        &self,
    ) -> (
        crate::decoding::scratch::FSEScratch,
        crate::decoding::scratch::HuffmanScratch,
        [u32; 3],
    ) {
        let (fse_src, huf_src, offset_hist) = match self {
            Self::Ring(s) => (&s.fse, &s.huf, s.offset_hist),
            Self::Flat(s) => (&s.fse, &s.huf, s.offset_hist),
        };
        let mut fse = crate::decoding::scratch::FSEScratch::new();
        fse.reinit_from(fse_src);
        let mut huf = crate::decoding::scratch::HuffmanScratch::new();
        huf.reinit_resolved_from(huf_src);
        (fse, huf, offset_hist)
    }

    /// Install entropy/repcode state from a [`ResumeState`] into the live
    /// scratch so a `Repeat_Mode` / treeless resume block resolves against the
    /// same tables a contiguous decode would have carried over.
    #[cfg(feature = "lsm")]
    fn restore_entropy(&mut self, state: &ResumeState) {
        match self {
            Self::Ring(s) => {
                s.fse.reinit_from(&state.fse);
                s.huf.reinit_resolved_from(&state.huf);
                s.offset_hist = state.offset_hist;
            }
            Self::Flat(s) => {
                s.fse.reinit_from(&state.fse);
                s.huf.reinit_resolved_from(&state.huf);
                s.offset_hist = state.offset_hist;
            }
        }
    }

    /// XXH64 of the window-prime bytes for a [`ResumeState`]: the last
    /// `min(window_size, buffer_len)` bytes of the current buffer, which at emit
    /// time are exactly the match-window context the resume block will see.
    /// Wrap-aware via `last_n_as_slices` — streaming XXH64 over the two slices
    /// equals a single hash over the contiguous `window_prime` at resume.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    fn window_tail_hash(&self, window_size: usize) -> u64 {
        use core::hash::Hasher;
        let n = core::cmp::min(window_size, self.buffer_len());
        let (s1, s2) = self.last_n_as_slices(n);
        let mut h = twox_hash::XxHash64::with_seed(0);
        h.write(s1);
        h.write(s2);
        h.finish()
    }

    fn decode_block_content<R: Read>(
        &mut self,
        decoder: &mut BlockDecoder,
        header: &crate::blocks::block::BlockHeader,
        source: R,
    ) -> Result<u64, DecodeBlockContentError> {
        match self {
            Self::Ring(s) => decoder.decode_block_content(header, s, source),
            Self::Flat(s) => decoder.decode_block_content(header, s, source),
        }
    }

    #[cfg(feature = "hash")]
    fn hash_finish(&self) -> u64 {
        use core::hash::Hasher;
        match self {
            Self::Ring(s) => s.buffer.hash.finish(),
            Self::Flat(s) => s.buffer.hash.finish(),
        }
    }

    /// Forward the drain-time hash toggle to the inner `DecodeBuffer`
    /// (streaming path). Called by the frame layer from the decoder's
    /// `ContentChecksum` mode before each decode.
    #[cfg(feature = "hash")]
    fn set_compute_hash(&mut self, compute: bool) {
        match self {
            Self::Ring(s) => s.buffer.set_compute_hash(compute),
            Self::Flat(s) => s.buffer.set_compute_hash(compute),
        }
    }
}

struct FrameDecoderState {
    pub frame_header: frame::FrameHeader,
    decoder_scratch: DecoderScratchKind,
    frame_finished: bool,
    block_counter: usize,
    bytes_read_counter: u64,
    check_sum: Option<u32>,
    using_dict: Option<u32>,
}

pub enum BlockDecodingStrategy {
    All,
    UptoBlocks(usize),
    UptoBytes(usize),
}

/// Outcome of [`FrameDecoder::decode_blocks_partial`]: the decompressed
/// bytes of the requested inner-block range plus where (if anywhere)
/// decoding stopped early.
///
/// Behind the `lsm` Cargo feature.
#[cfg(feature = "lsm")]
#[derive(Debug)]
pub struct PartialDecode {
    /// Decompressed bytes of the in-range blocks actually decoded, in
    /// frame order, as one contiguous buffer. `data.len()` equals the sum
    /// of the decompressed sizes of blocks `start_block .. start_block +
    /// blocks_decoded`.
    pub data: alloc::vec::Vec<u8>,
    /// First block whose output is in [`data`](Self::data): the requested
    /// `start_block` on a fresh decode, or [`ResumeState::block_index`] when
    /// resuming (the caller-supplied `start_block` is ignored in resume mode).
    pub start_block: u32,
    /// Number of in-range blocks successfully decoded into
    /// [`data`](Self::data).
    pub blocks_decoded: u32,
    /// `Some((block_index, error))` if decoding stopped on a failing block
    /// before reaching `end_block` (a corrupt block inside the range, or a
    /// leading block needed for window context). `None` if the requested
    /// range decoded cleanly or the frame's last block was reached first.
    ///
    /// When the failing block is a leading context block
    /// (`block_index < start_block`), the in-range window could not be
    /// built so [`data`](Self::data) is empty and `blocks_decoded` is 0.
    pub stopped_at: Option<(u32, FrameDecoderError)>,
    /// `true` if the frame's last block was reached during this decode.
    pub frame_finished: bool,
    /// Cross-block carry-over state for resuming the next extent. Feed it back
    /// (with the matching `window_prime`) via the `resume` argument of a
    /// later [`FrameDecoder::decode_blocks_partial`] to continue from
    /// [`ResumeState::block_index`] without re-decompressing the prefix.
    ///
    /// `None` in two cases: emission was not requested (`emit_resume = false`),
    /// OR this decode reached the frame's last block ([`frame_finished`] is
    /// `true`) — there is no following block to resume from, so no snapshot is
    /// emitted even with `emit_resume = true`. Callers walking a frame
    /// incrementally should therefore stop when `frame_finished` is set rather
    /// than treat a `None` here as "emission disabled".
    ///
    /// [`frame_finished`]: Self::frame_finished
    pub resume_state: Option<ResumeState>,
}

impl FrameDecoderState {
    /// Construct a new frame decoder state, reading the frame header
    /// from `source`. When `magicless` is `true`, the 4-byte magic
    /// number prefix is NOT consumed (donor `ZSTD_f_zstd1_magicless`).
    /// Crate-internal — reached only via `FrameDecoder::init` /
    /// `FrameDecoder::init_with_dict_handle`. The decode buffer is
    /// allocated lazily on BOTH backends (`new_ring` and `new_flat`):
    /// direct-eligible frames pay zero buffer allocation, and the
    /// non-direct fallback reserves `window_size` once in
    /// `decode_all_impl` / `decode_blocks` via `reserve_buffer` before
    /// any block write.
    pub(crate) fn new_with_format(
        source: impl Read,
        magicless: bool,
    ) -> Result<FrameDecoderState, FrameDecoderError> {
        let (frame, header_size) = frame::read_frame_header_with_format(source, magicless)?;
        let window_size = frame.window_size()?;

        if window_size > MAXIMUM_ALLOWED_WINDOW_SIZE {
            return Err(FrameDecoderError::WindowSizeTooBig {
                requested: window_size,
            });
        }

        let decoder_scratch = if frame.descriptor.single_segment_flag() {
            DecoderScratchKind::new_flat(window_size as usize)
        } else {
            DecoderScratchKind::new_ring(window_size as usize)
        };
        Ok(FrameDecoderState {
            frame_header: frame,
            frame_finished: false,
            block_counter: 0,
            decoder_scratch,
            bytes_read_counter: u64::from(header_size),
            check_sum: None,
            using_dict: None,
        })
    }

    /// Reset this state for a new frame read from `source`, reusing
    /// existing allocations. When `magicless` is `true`, the frame
    /// header is read WITHOUT expecting a magic-number prefix
    /// (donor `ZSTD_f_zstd1_magicless`). Crate-internal — reached
    /// only via `FrameDecoder::reset`.
    ///
    /// `DecodeBuffer::reset` no longer reserves window_size for either
    /// backend — capacity decisions live one layer up. Both backends are
    /// lazy: direct-eligible frames pay zero backing-buffer allocation
    /// here (they write through `UserSliceBackend`), and the non-direct
    /// path is pre-reserved by `decode_all_impl` / `decode_blocks` via
    /// `DecoderScratchKind::reserve_buffer(window_size)` before any block
    /// write. A reused scratch whose new frame fits within prior capacity
    /// reuses it; a larger one grows on that same `reserve_buffer` call.
    pub(crate) fn reset_with_format(
        &mut self,
        source: impl Read,
        magicless: bool,
    ) -> Result<(), FrameDecoderError> {
        let (frame_header, header_size) = frame::read_frame_header_with_format(source, magicless)?;
        let window_size = frame_header.window_size()?;

        if window_size > MAXIMUM_ALLOWED_WINDOW_SIZE {
            return Err(FrameDecoderError::WindowSizeTooBig {
                requested: window_size,
            });
        }

        self.decoder_scratch
            .reset(&frame_header, window_size as usize);
        self.frame_header = frame_header;
        self.frame_finished = false;
        self.block_counter = 0;
        self.bytes_read_counter = u64::from(header_size);
        self.check_sum = None;
        self.using_dict = None;
        Ok(())
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    /// This will create a new decoder without allocating anything yet.
    /// init()/reset() will allocate all needed buffers if it is the first time this decoder is used
    /// else they just reset these buffers with not further allocations
    pub fn new() -> FrameDecoder {
        FrameDecoder {
            state: None,
            owned_dicts: BTreeMap::new(),
            #[cfg(target_has_atomic = "ptr")]
            shared_dicts: BTreeMap::new(),
            #[cfg(not(target_has_atomic = "ptr"))]
            shared_dicts: (),
            magicless: false,
            content_checksum: ContentChecksum::EmitOnly,
            #[cfg(feature = "lsm")]
            expect_dict_id: None,
            #[cfg(feature = "lsm")]
            expect_window_descriptor: None,
            #[cfg(all(feature = "lsm", feature = "hash"))]
            per_block_checksums_enabled: false,
            #[cfg(all(feature = "lsm", feature = "hash"))]
            computed_block_checksums: alloc::vec::Vec::new(),
        }
    }

    /// Select how the frame's optional content checksum is handled
    /// (compute, expose, verify, or skip). See [`ContentChecksum`].
    /// Default [`ContentChecksum::EmitOnly`]. Takes effect on the next
    /// decode; safe to call between frames on a reused decoder.
    pub fn set_content_checksum(&mut self, mode: ContentChecksum) {
        self.content_checksum = mode;
    }

    /// Opt in to per-block XXH64 verification during decode.
    /// Default off; zero cost when disabled. Each block's decompressed
    /// bytes are XXH64-hashed (low 32 bits) and appended to
    /// [`Self::computed_block_checksums`] as the decode progresses.
    /// Callers compare the captured digests against externally-stored
    /// expected values (e.g. from a per-block sidecar in the
    /// containing application protocol).
    ///
    /// Behind `all(feature = "lsm", feature = "hash")` — the XXH64
    /// primitive lives behind the `hash` feature, so this method
    /// only compiles when both are enabled.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    pub fn enable_per_block_checksums(&mut self) {
        self.per_block_checksums_enabled = true;
    }

    /// Per-block XXH64 (low 32 bits) digests captured during the
    /// current frame's decode. Empty unless
    /// [`Self::enable_per_block_checksums`] was called before
    /// [`Self::decode_all`] / [`Self::reset`].
    ///
    /// Reset at the start of every new frame.
    ///
    /// Behind `all(feature = "lsm", feature = "hash")`.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    pub fn computed_block_checksums(&self) -> &[u32] {
        &self.computed_block_checksums
    }

    /// Pin the expected `Dictionary_ID` for the next frame.
    ///
    /// When `expected` is set, [`Self::init`] / [`Self::reset`]
    /// validate it against the parsed frame header BEFORE any
    /// block decode work runs. A mismatch returns
    /// [`crate::decoding::errors::FrameDecoderError::UnexpectedDictId`]
    /// before any block decode and before any output is produced.
    /// Scratch buffer allocation / reservation for the decode
    /// pipeline happens during frame-header parsing, which is
    /// already complete when this validation fires — the cost of
    /// scratch sizing is paid even on a mismatched header. The
    /// guarantee is "no block decode, no XXH64 init, no partial
    /// output", not "zero allocation".
    ///
    /// `Some(0)` is treated as "no dictionary expected": a frame
    /// whose header omits the optional `Dictionary_ID` field
    /// (flag value 0) passes the check; a frame that carries an
    /// explicit non-zero id fails.
    ///
    /// `None` (default) disables the check.
    ///
    /// Primary use case: post-AEAD-decrypt sanity check in
    /// wire-format consumers (e.g. lsm-tree's encrypted block
    /// format pins the `dict_id` baked into the AAD against the
    /// inner zstd frame's `dict_id` to defeat dict-substitution
    /// attacks).
    ///
    /// NOT a replacement for AEAD authentication. NOT the same
    /// semantic as donor `ZSTD_d_windowLogMax` (which is a
    /// ceiling-style limit, separate concern).
    #[cfg(feature = "lsm")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
    pub fn expect_dict_id(&mut self, expected: Option<u32>) {
        self.expect_dict_id = expected;
    }

    /// Pin the expected raw `Window_Descriptor` byte (RFC 8878
    /// §3.1.1.1.2 layout: `(exp << 3) | mantissa`) for the next
    /// frame.
    ///
    /// When `expected` is set, [`Self::init`] / [`Self::reset`]
    /// validate it against the parsed frame header BEFORE any
    /// block decode work runs. A mismatch returns
    /// [`crate::decoding::errors::FrameDecoderError::UnexpectedWindowDescriptor`].
    ///
    /// Single-segment frames omit the `Window_Descriptor` byte
    /// from the wire entirely. Setting an expectation while
    /// receiving a single-segment frame fails the check with
    /// `found: None` — there is no on-wire byte to match against,
    /// which is reported explicitly rather than silently passing.
    ///
    /// `None` (default) disables the check.
    ///
    /// Byte-exact equality, NOT a ceiling. Donor
    /// `ZSTD_d_windowLogMax` is a separate ceiling-style limit
    /// available through the C FFI surface; this method is for
    /// strict equality validation against a pinned expectation
    /// (e.g. lsm-tree's wire format pins the window descriptor
    /// from the AAD to defeat decompression-bomb-swap attacks).
    #[cfg(feature = "lsm")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
    pub fn expect_window_descriptor(&mut self, expected: Option<u8>) {
        self.expect_window_descriptor = expected;
    }

    /// Validate the just-parsed frame header against any pinned
    /// expectations set via [`Self::expect_dict_id`] /
    /// [`Self::expect_window_descriptor`].
    ///
    /// Returns the typed error variant on mismatch and leaves
    /// `self.state` in a re-resettable shape — a subsequent
    /// `reset()` will overwrite `frame_header` from the new source
    /// without needing intermediate cleanup.
    #[cfg(feature = "lsm")]
    fn validate_expectations(
        &self,
        frame_header: &frame::FrameHeader,
    ) -> Result<(), FrameDecoderError> {
        if let Some(expected) = self.expect_dict_id {
            let found = frame_header.dictionary_id();
            // `Some(0)` is the "no dictionary expected" sentinel —
            // matches a frame whose header omits the optional
            // dict_id field (which is reported as `None` by the
            // parser). All other values must match exactly.
            let matches = match (expected, found) {
                (0, None) => true,
                (e, Some(f)) => e == f,
                _ => false,
            };
            if !matches {
                return Err(FrameDecoderError::UnexpectedDictId {
                    expected: Some(expected),
                    found,
                });
            }
        }
        if let Some(expected) = self.expect_window_descriptor {
            let found = frame_header.window_descriptor();
            if found != Some(expected) {
                return Err(FrameDecoderError::UnexpectedWindowDescriptor { expected, found });
            }
        }
        Ok(())
    }

    /// Enable or disable magicless frame format
    /// (`ZSTD_f_zstd1_magicless`). When set to `true`, subsequent
    /// [`init`] / [`reset`] calls expect the frame header to begin
    /// directly with the frame-header descriptor — no 4-byte magic
    /// number prefix. Default false. Must match the encoder's
    /// magicless setting; the format is unambiguous only when the
    /// caller knows it out-of-band.
    ///
    /// Note: magicless mode also disables skippable-frame detection.
    /// The `0x184D2A50..=0x184D2A5F` skippable-frame magic range is
    /// only recognised when the 4-byte magic prefix is consumed, so
    /// `decode_all` / `init` / `reset` will treat a skippable frame
    /// at the head of a magicless stream as a malformed frame header
    /// (bad descriptor / window-size error) instead of skipping it.
    /// Mixed-format streams that interleave skippable frames must be
    /// pre-split by the caller; `set_magicless(true)` is only safe
    /// when the entire stream is known to be magicless zstd frames.
    pub fn set_magicless(&mut self, magicless: bool) {
        self.magicless = magicless;
    }

    #[cfg(target_has_atomic = "ptr")]
    fn shared_dict_exists(&self, dict_id: u32) -> bool {
        self.shared_dicts.contains_key(&dict_id)
    }

    #[cfg(not(target_has_atomic = "ptr"))]
    fn shared_dict_exists(&self, _dict_id: u32) -> bool {
        false
    }

    fn validate_registered_dictionary(dict: &Dictionary) -> Result<(), FrameDecoderError> {
        use crate::decoding::errors::DictionaryDecodeError as dict_err;

        if dict.id == 0 {
            return Err(FrameDecoderError::from(dict_err::ZeroDictionaryId));
        }
        if let Some(index) = dict.offset_hist.iter().position(|&rep| rep == 0) {
            return Err(FrameDecoderError::from(
                dict_err::ZeroRepeatOffsetInDictionary { index: index as u8 },
            ));
        }
        Ok(())
    }

    /// init() will allocate all needed buffers if it is the first time this decoder is used
    /// else they just reset these buffers with not further allocations
    ///
    /// Note that all bytes currently in the decodebuffer from any previous frame will be lost. Collect them with collect()/collect_to_writer()
    ///
    /// equivalent to reset()
    pub fn init(&mut self, source: impl Read) -> Result<(), FrameDecoderError> {
        self.reset(source)
    }

    /// Initialize the decoder for a new frame using a pre-parsed dictionary handle.
    ///
    /// If the frame header has a dictionary ID, this validates it against
    /// `dict.id()` and returns [`FrameDecoderError::DictIdMismatch`] on mismatch.
    ///
    /// If the header omits the optional dictionary ID, this still applies the
    /// provided dictionary handle.
    ///
    /// # Warning
    ///
    /// This method always applies `dict` unless the frame header contains a
    /// non-matching dictionary ID. Callers must only use this API when they
    /// already know the frame was encoded with the provided dictionary, even if
    /// the frame header omits the dictionary ID or encodes an explicit
    /// dictionary ID of `0`.
    ///
    /// Passing a dictionary for a frame that was not encoded with it can
    /// silently corrupt the decoded output.
    pub fn init_with_dict_handle(
        &mut self,
        source: impl Read,
        dict: &DictionaryHandle,
    ) -> Result<(), FrameDecoderError> {
        self.reset_with_dict_handle(source, dict)
    }

    /// reset() will allocate all needed buffers if it is the first time this decoder is used
    /// else they just reset these buffers with not further allocations
    ///
    /// Note that all bytes currently in the decodebuffer from any previous frame will be lost. Collect them with collect()/collect_to_writer()
    ///
    /// equivalent to init()
    pub fn reset(&mut self, source: impl Read) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        // Fresh frame → start with an empty per-block checksum vec so
        // the values for the next frame don't carry over from the
        // previous one.
        #[cfg(all(feature = "lsm", feature = "hash"))]
        self.computed_block_checksums.clear();
        let magicless = self.magicless;
        let dict_id = match &mut self.state {
            Some(s) => {
                s.reset_with_format(source, magicless)?;
                s.frame_header.dictionary_id()
            }
            None => {
                self.state = Some(FrameDecoderState::new_with_format(source, magicless)?);
                self.state
                    .as_ref()
                    .and_then(|state| state.frame_header.dictionary_id())
            }
        };
        // Validate any pinned expectations BEFORE block decode work
        // runs. Catches dict_id substitution / window-descriptor
        // tampering on inputs already authenticated by an outer
        // layer (e.g. AEAD). Returning here leaves `self.state` in
        // a re-resettable shape — next `reset()` re-parses the
        // frame header without intermediate cleanup.
        #[cfg(feature = "lsm")]
        if let Some(state) = self.state.as_ref() {
            self.validate_expectations(&state.frame_header)?;
        }
        if let Some(dict_id) = dict_id {
            let state = self.state.as_mut().expect("state initialized");
            let owned_dicts = &self.owned_dicts;
            #[cfg(target_has_atomic = "ptr")]
            let shared_dicts = &self.shared_dicts;
            let dict = owned_dicts
                .get(&dict_id)
                .or_else(|| {
                    #[cfg(target_has_atomic = "ptr")]
                    {
                        shared_dicts.get(&dict_id)
                    }
                    #[cfg(not(target_has_atomic = "ptr"))]
                    {
                        None
                    }
                })
                .ok_or(err::DictNotProvided { dict_id })?;
            state.decoder_scratch.init_from_dict(dict);
            state.using_dict = Some(dict_id);
        }
        Ok(())
    }

    /// Reset this decoder for a new frame using a pre-parsed dictionary handle.
    ///
    /// If the frame header has a dictionary ID, this validates it against
    /// `dict.id()` and returns [`FrameDecoderError::DictIdMismatch`] on mismatch.
    ///
    /// If the header omits the optional dictionary ID, this still applies the
    /// provided dictionary handle.
    ///
    /// # Warning
    ///
    /// This method always applies `dict` unless the frame header contains a
    /// non-matching dictionary ID. Callers must only use this API when they
    /// already know the frame was encoded with the provided dictionary, even if
    /// the frame header omits the dictionary ID or encodes an explicit
    /// dictionary ID of `0`.
    ///
    /// Passing a dictionary for a frame that was not encoded with it can
    /// silently corrupt the decoded output.
    pub fn reset_with_dict_handle(
        &mut self,
        source: impl Read,
        dict: &DictionaryHandle,
    ) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        // Fresh frame → drop the previous frame's per-block checksum
        // digests so the next decode starts with an empty vec.
        // Mirrors the same clear in `reset()`; reset_with_dict_handle
        // is a parallel entry point so it needs its own call.
        #[cfg(all(feature = "lsm", feature = "hash"))]
        self.computed_block_checksums.clear();
        Self::validate_registered_dictionary(dict.as_dict())?;
        let magicless = self.magicless;
        // Scope the &mut borrow of `self.state` to the header parse
        // alone, so the subsequent `validate_expectations(&self, ...)`
        // call below can take a fresh shared borrow of self without
        // tripping the borrow checker.
        match &mut self.state {
            Some(s) => s.reset_with_format(source, magicless)?,
            None => {
                self.state = Some(FrameDecoderState::new_with_format(source, magicless)?);
            }
        }
        // Single source of truth: route through the same
        // `validate_expectations` used by `reset()`. Routing through
        // the helper keeps the two code paths from drifting (e.g.,
        // if expect-semantics or error wiring changes later).
        #[cfg(feature = "lsm")]
        {
            let header = &self
                .state
                .as_ref()
                .expect("state populated by reset_with_format/new_with_format")
                .frame_header;
            self.validate_expectations(header)?;
        }
        let state = self
            .state
            .as_mut()
            .expect("state populated by reset_with_format/new_with_format");
        if let Some(dict_id) = state.frame_header.dictionary_id()
            && dict_id != dict.id()
        {
            return Err(err::DictIdMismatch {
                expected: dict_id,
                provided: dict.id(),
            });
        }
        state.decoder_scratch.init_from_dict(dict);
        state.using_dict = Some(dict.id());
        Ok(())
    }

    /// Add a dictionary that can be selected dynamically by frame dictionary ID.
    ///
    /// Returns [`FrameDecoderError::DictAlreadyRegistered`] if the ID is already
    /// registered (either as owned or shared).
    pub fn add_dict(&mut self, dict: Dictionary) -> Result<(), FrameDecoderError> {
        Self::validate_registered_dictionary(&dict)?;
        let dict_id = dict.id;
        if self.owned_dicts.contains_key(&dict_id) || self.shared_dict_exists(dict_id) {
            return Err(FrameDecoderError::DictAlreadyRegistered { dict_id });
        }
        self.owned_dicts
            .insert(dict_id, DictionaryHandle::from_dictionary(dict));
        Ok(())
    }

    /// Parse and add a serialized dictionary blob.
    pub fn add_dict_from_bytes(&mut self, raw_dictionary: &[u8]) -> Result<(), FrameDecoderError> {
        let dict = Dictionary::decode_dict(raw_dictionary)?;
        self.add_dict(dict)
    }

    /// Add a pre-parsed dictionary handle for reuse across decoders.
    ///
    /// This API is available on targets with pointer-width atomics
    /// (`target_has_atomic = "ptr"`).
    ///
    /// Returns [`FrameDecoderError::DictAlreadyRegistered`] if the ID is already
    /// registered (either as owned or shared).
    #[cfg(target_has_atomic = "ptr")]
    pub fn add_dict_handle(&mut self, dict: DictionaryHandle) -> Result<(), FrameDecoderError> {
        Self::validate_registered_dictionary(dict.as_dict())?;
        let dict_id = dict.id();
        if self.owned_dicts.contains_key(&dict_id) || self.shared_dicts.contains_key(&dict_id) {
            return Err(FrameDecoderError::DictAlreadyRegistered { dict_id });
        }
        self.shared_dicts.insert(dict_id, dict);
        Ok(())
    }

    pub fn force_dict(&mut self, dict_id: u32) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;
        let owned_dicts = &self.owned_dicts;
        #[cfg(target_has_atomic = "ptr")]
        let shared_dicts = &self.shared_dicts;

        let dict = owned_dicts
            .get(&dict_id)
            .or_else(|| {
                #[cfg(target_has_atomic = "ptr")]
                {
                    shared_dicts.get(&dict_id)
                }
                #[cfg(not(target_has_atomic = "ptr"))]
                {
                    None
                }
            })
            .ok_or(err::DictNotProvided { dict_id })?;
        state.decoder_scratch.init_from_dict(dict);
        state.using_dict = Some(dict_id);

        Ok(())
    }

    /// Returns how many bytes the frame contains after decompression
    pub fn content_size(&self) -> u64 {
        match &self.state {
            None => 0,
            Some(s) => s.frame_header.frame_content_size(),
        }
    }

    /// Returns the checksum that was read from the data. Only available after all bytes have been read. It is the last 4 bytes of a zstd-frame
    pub fn get_checksum_from_data(&self) -> Option<u32> {
        let state = self.state.as_ref()?;

        state.check_sum
    }

    /// Returns the checksum that was calculated while decoding.
    /// Only a sensible value after all decoded bytes have been collected/read from the FrameDecoder.
    /// Returns `None` when the frame header has `content_checksum_flag = 0`:
    /// no hash is computed for such frames (the post-decode XXH64 pass was a
    /// 63 % decode-wall hotspot on flag-off frames; skipping it when the
    /// frame format declares no trailing digest avoids that wasted work).
    #[cfg(feature = "hash")]
    pub fn get_calculated_checksum(&self) -> Option<u32> {
        let state = self.state.as_ref()?;
        // `ContentChecksum::None` skips the XXH64 pass entirely, so there is
        // no calculated digest to report.
        if self.content_checksum == ContentChecksum::None {
            return None;
        }
        if !state.frame_header.descriptor.content_checksum_flag() {
            return None;
        }
        let cksum_64bit = state.decoder_scratch.hash_finish();
        //truncate to lower 32bit because reasons...
        Some(cksum_64bit as u32)
    }

    /// Compare the frame's stored content checksum against the digest the
    /// decoder computed, returning [`FrameDecoderError::ChecksumMismatch`] on
    /// disagreement. No-op unless the mode is [`ContentChecksum::Verify`] and
    /// the frame carries a trailing checksum.
    ///
    /// [`decode_all`](Self::decode_all) and the streaming reader call this
    /// automatically. Callers driving [`decode_blocks`](Self::decode_blocks)
    /// directly invoke it themselves once per frame, after the frame is fully
    /// decoded AND fully drained (e.g. via [`collect`](Self::collect)), so both
    /// the stored value and the running digest are final.
    #[cfg(feature = "hash")]
    pub fn verify_content_checksum(&self) -> Result<(), FrameDecoderError> {
        if self.content_checksum != ContentChecksum::Verify {
            return Ok(());
        }
        let Some(state) = self.state.as_ref() else {
            return Ok(());
        };
        if !state.frame_header.descriptor.content_checksum_flag() {
            return Ok(());
        }
        let Some(expected) = state.check_sum else {
            return Ok(());
        };
        let calculated = state.decoder_scratch.hash_finish() as u32;
        if expected != calculated {
            return Err(FrameDecoderError::ChecksumMismatch {
                expected,
                calculated,
            });
        }
        Ok(())
    }

    /// Counter for how many bytes have been consumed while decoding the frame
    pub fn bytes_read_from_source(&self) -> u64 {
        let state = match &self.state {
            None => return 0,
            Some(s) => s,
        };
        state.bytes_read_counter
    }

    /// Whether the current frames last block has been decoded yet
    /// If this returns true you can call the drain* functions to get all content
    /// (the read() function will drain automatically if this returns true)
    pub fn is_finished(&self) -> bool {
        let state = match &self.state {
            None => return true,
            Some(s) => s,
        };
        if state.frame_header.descriptor.content_checksum_flag() {
            state.frame_finished && state.check_sum.is_some()
        } else {
            state.frame_finished
        }
    }

    /// Counter for how many blocks have already been decoded
    pub fn blocks_decoded(&self) -> usize {
        let state = match &self.state {
            None => return 0,
            Some(s) => s,
        };
        state.block_counter
    }

    /// Decodes blocks from a reader. It requires that the framedecoder has been initialized first.
    /// The Strategy influences how many blocks will be decoded before the function returns
    /// This is important if you want to manage memory consumption carefully. If you don't care
    /// about that you can just choose the strategy "All" and have all blocks of the frame decoded into the buffer
    pub fn decode_blocks(
        &mut self,
        mut source: impl Read,
        strat: BlockDecodingStrategy,
    ) -> Result<bool, FrameDecoderError> {
        use FrameDecoderError as err;
        // Apply the content-checksum mode to the streaming drain hash before
        // any block decodes into the ring. `None` skips the XXH64 pass.
        #[cfg(feature = "hash")]
        let compute_hash = self.content_checksum != ContentChecksum::None;
        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;
        #[cfg(feature = "hash")]
        state.decoder_scratch.set_compute_hash(compute_hash);

        // Streaming entry point: pre-reserve the backing buffer to
        // `window_size` so multi-block frames don't pay repeated
        // `reserve_amortized` grow steps (128 KiB → 256 KiB → ... →
        // window) as blocks accumulate. `decode_all` does the same up
        // front in `decode_all_impl`; this mirrors it for callers
        // driving `decode_blocks` directly. Idempotent — the
        // backend's `reserve` early-returns when capacity is already
        // sufficient.
        let window_size = state.frame_header.window_size().unwrap_or(0) as usize;
        state.decoder_scratch.reserve_buffer(window_size);

        let mut block_dec = decoding::block_decoder::new();

        let buffer_size_before = state.decoder_scratch.buffer_len();
        let block_counter_before = state.block_counter;
        loop {
            vprintln!("################");
            vprintln!("Next Block: {}", state.block_counter);
            vprintln!("################");
            // Capture the failing-block coordinates BEFORE the header read so
            // the error carries where it happened: `bytes_read_counter` is the
            // frame-absolute offset of this block's header (not yet advanced),
            // `block_counter` its 0-based index. Used by both the header- and
            // body-error builders below (block-precise recovery under `lsm`).
            let block_index = state.block_counter as u32;
            let block_frame_offset = state.bytes_read_counter as u32;
            let (block_header, block_header_size) =
                block_dec.read_block_header(&mut source).map_err(|source| {
                    block_header_decode_error(source, block_index, block_frame_offset)
                })?;
            state.bytes_read_counter += u64::from(block_header_size);

            vprintln!();
            vprintln!(
                "Found {} block with size: {}, which will be of size: {}",
                block_header.block_type,
                block_header.content_size,
                block_header.decompressed_size
            );

            #[cfg(all(feature = "lsm", feature = "hash"))]
            let len_before_block: Option<usize> = if self.per_block_checksums_enabled {
                Some(state.decoder_scratch.buffer_len())
            } else {
                None
            };
            let bytes_read_in_block_body = state
                .decoder_scratch
                .decode_block_content(&mut block_dec, &block_header, &mut source)
                .map_err(|source| {
                    block_body_decode_error(
                        source,
                        block_index,
                        block_frame_offset,
                        &block_header,
                        block_header_size,
                    )
                })?;
            state.bytes_read_counter += bytes_read_in_block_body;

            // Per-block XXH64 (low 32 bits) of the just-decompressed
            // bytes. Hashed from `last_n_as_slices` so RingBuffer wrap
            // is handled in-place, no extra copy.
            #[cfg(all(feature = "lsm", feature = "hash"))]
            if let Some(len_before_block) = len_before_block {
                let added = state.decoder_scratch.buffer_len() - len_before_block;
                let (s1, s2) = state.decoder_scratch.last_n_as_slices(added);
                let mut h = twox_hash::XxHash64::with_seed(0);
                use core::hash::Hasher;
                h.write(s1);
                h.write(s2);
                self.computed_block_checksums.push(h.finish() as u32);
            }

            state.block_counter += 1;

            vprintln!("Output: {}", state.decoder_scratch.buffer_len());

            if block_header.last_block {
                state.frame_finished = true;
                if state.frame_header.descriptor.content_checksum_flag() {
                    let mut chksum = [0u8; 4];
                    source
                        .read_exact(&mut chksum)
                        .map_err(err::FailedToReadChecksum)?;
                    state.bytes_read_counter += 4;
                    let chksum = u32::from_le_bytes(chksum);
                    state.check_sum = Some(chksum);
                }
                break;
            }

            match strat {
                BlockDecodingStrategy::All => { /* keep going */ }
                BlockDecodingStrategy::UptoBlocks(n) => {
                    if state.block_counter - block_counter_before >= n {
                        break;
                    }
                }
                BlockDecodingStrategy::UptoBytes(n) => {
                    if state.decoder_scratch.buffer_len() - buffer_size_before >= n {
                        break;
                    }
                }
            }
        }

        Ok(state.frame_finished)
    }

    /// Decode the inner blocks `[start_block, end_block)` of the current
    /// frame and return their decompressed bytes as one contiguous buffer.
    ///
    /// Serves two consumer needs with one call:
    ///
    /// - **Range-query performance:** decode only the inner zstd blocks that
    ///   cover a key range instead of the whole frame. Blocks before
    ///   `start_block` are decoded into the window (zstd blocks share one
    ///   window, so a leading block's bytes may be the match source for an
    ///   in-range block and cannot simply be skipped) but their output is not
    ///   returned; blocks at or after `end_block` are not decoded at all,
    ///   which is the trailing-block work saving. Map a decompressed byte
    ///   offset to a block index with
    ///   [`FrameEmitInfo::decompressed_byte_range`].
    /// - **Best-effort recovery:** if a block decode fails, decoding stops,
    ///   the clean prefix of in-range output is preserved in
    ///   [`PartialDecode::data`], and the failure is reported via
    ///   [`PartialDecode::stopped_at`]. Passing `(0, u32::MAX)` decodes the
    ///   whole frame, stopping at the first corrupt block (pure recovery).
    ///
    /// `end_block` is exclusive; pass `u32::MAX` to decode to the end of the
    /// frame. Call on a freshly [`reset`](Self::reset) decoder (it decodes
    /// from the frame's first block).
    ///
    /// # Resume (cold incremental / top-up)
    ///
    /// A plain call drains its in-range output from the match window on return,
    /// so two consecutive calls cannot resume one another and growing a decoded
    /// extent would mean re-decoding the covering prefix from block 0
    /// (`O(extent)` per growth, `O(N²)` for a forward walk). The `resume` /
    /// `emit_resume` arguments make a symmetric one-call grow-loop possible:
    ///
    /// - `emit_resume = true` captures the cross-block carry-over state (entropy
    ///   tables + repcode history + the next block index / output offset) into
    ///   [`PartialDecode::resume_state`]. The entropy-table snapshot clone is
    ///   only paid when this is set. The snapshot is `None` when the decode
    ///   reaches the frame's last block ([`PartialDecode::frame_finished`]):
    ///   there is no following block to resume from, so an incremental walk
    ///   stops on `frame_finished` rather than on a `None` snapshot.
    /// - `resume = Some(`[`ResumeInput`]`)` continues from a previously emitted
    ///   [`ResumeState`] WITHOUT re-decompressing the preceding blocks: the
    ///   match window is primed from [`ResumeInput::window_prime`] and the
    ///   entropy/repcode tables are restored from the state, so a `Repeat_Mode`
    ///   resume block resolves byte-identically to a contiguous decode — even
    ///   across a dropped (cold) decoder.
    ///
    /// When `resume` is `Some`, decoding resumes at
    /// [`ResumeState::block_index`] and the `start_block` argument is ignored
    /// (pass `resume.state.block_index()`); position `source` at that block's
    /// compressed frame offset
    /// ([`FrameEmitInfo::blocks`]`[block_index].offset_in_frame`). After a
    /// resumed call, [`bytes_read_from_source`](Self::bytes_read_from_source)
    /// and any `stopped_at` offsets are relative to the repositioned `source`.
    ///
    /// **Dictionaries:** [`ResumeState`] does NOT carry the dictionary content.
    /// For a dictionary frame, attach the dictionary to the resuming decoder the
    /// same way as for a fresh decode — [`reset`](Self::reset) with the
    /// dictionary registered (or
    /// [`reset_with_dict_handle`](Self::reset_with_dict_handle)) BEFORE this
    /// call — so dict-sourced matches near the frame start resolve. The caller
    /// already holds the dictionary (it supplied it at encode time), so
    /// re-supplying it on resume is free; storing it in the snapshot would only
    /// duplicate it. The resume guard records the applied dictionary's identity
    /// and rejects ([`FrameDecoderError::ResumeFrameMismatch`]) a resume whose
    /// active dictionary differs from the one the snapshot was captured under.
    ///
    /// # Errors
    ///
    /// Returns [`FrameDecoderError::NotYetInitialized`] if the decoder has not
    /// been reset, [`FrameDecoderError::InvalidBlockRange`] if the effective
    /// start exceeds `end_block`, [`FrameDecoderError::ResumeWindowTooShort`]
    /// if `resume`'s `window_prime` is shorter than the match window the resume
    /// block can reach back into (`min(window_size, output_offset)`), and
    /// [`FrameDecoderError::ResumeFrameMismatch`] if the snapshot was captured
    /// from a frame with a different decode shape / dictionary, or (with the
    /// `hash` feature) a `window_prime` whose content does not match what was
    /// captured — all rejected up front rather than silently mis-resolving
    /// matches. A corrupt block is NOT an `Err` here: it is reported via
    /// [`PartialDecode::stopped_at`] so the clean prefix survives.
    ///
    /// [`FrameEmitInfo::decompressed_byte_range`]: crate::encoding::frame_emit_info::FrameEmitInfo::decompressed_byte_range
    /// [`FrameEmitInfo::blocks`]: crate::encoding::frame_emit_info::FrameEmitInfo::blocks
    #[cfg(feature = "lsm")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
    pub fn decode_blocks_partial(
        &mut self,
        mut source: impl Read,
        start_block: u32,
        end_block: u32,
        resume: Option<ResumeInput<'_>>,
        emit_resume: bool,
    ) -> Result<PartialDecode, FrameDecoderError> {
        use FrameDecoderError as err;
        let magicless = self.magicless;
        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;

        // Mirror `decode_blocks`: pre-reserve the backing buffer to
        // `window_size` so multi-block frames don't pay repeated grow steps.
        let window_size = state.frame_header.window_size().unwrap_or(0) as usize;
        state.decoder_scratch.reserve_buffer(window_size);

        // Cold resume: prime the match window + restore entropy/repcode state +
        // advance the block cursor BEFORE the loop, so the first in-range block
        // resolves its matches and `Repeat_Mode` tables against the caller's
        // persisted state instead of re-decoded prefix blocks. The effective
        // start is the resume state's block index (the passed `start_block` is
        // ignored in resume mode, per the doc).
        let effective_start = if let Some(r) = resume {
            // Reject a snapshot captured from a different frame shape BEFORE
            // touching any decoder state: restoring entropy/repcode tables that
            // belong to another frame would silently produce byte-wrong output.
            let current_key = FrameKey::from_state(state, magicless);
            if current_key != r.state.frame_key {
                return Err(err::ResumeFrameMismatch);
            }
            let output_offset = r.state.output_offset;
            // The window the resume block can reach back into is bounded by the
            // smaller of the frame's window_size and the bytes produced so far.
            let required = core::cmp::min(window_size as u64, output_offset) as usize;
            if r.window_prime.len() < required {
                return Err(err::ResumeWindowTooShort {
                    got: r.window_prime.len(),
                    need: required,
                });
            }
            // Only the most recent `window_size` bytes can ever back a match
            // (offset <= window_size by the frame invariant); load just those
            // even if the caller handed us a longer prefix, bounding resume
            // memory to one window regardless of the skipped prefix's size.
            let prime = if r.window_prime.len() > window_size {
                &r.window_prime[r.window_prime.len() - window_size..]
            } else {
                r.window_prime
            };
            // Content-exact identity: the primed window must hash to what was
            // captured at emit. Catches a same-shape-but-different-frame
            // snapshot and a wrong/corrupted window_prime (which FrameKey alone
            // cannot), before any state is restored. O(window) one-time per
            // resume — negligible next to the decode it guards.
            #[cfg(feature = "hash")]
            if xxh64_of(prime) != r.state.window_hash {
                return Err(err::ResumeFrameMismatch);
            }
            // Validate the effective range (resume mode begins at the resume
            // block, ignoring the caller's `start_block`) BEFORE mutating the
            // decoder: an inverted `end_block` must fail without priming the
            // window / entropy or advancing the cursor, leaving the decoder
            // re-resettable rather than in a half-resumed state.
            let effective_start = r.state.block_index;
            if effective_start > end_block {
                return Err(err::InvalidBlockRange {
                    start_block: effective_start,
                    end_block,
                });
            }
            state.decoder_scratch.restore_entropy(r.state);
            state.decoder_scratch.prime_window(prime, output_offset);
            state.block_counter = effective_start as usize;
            // The caller repositions `source` to the resume block; report
            // consumed bytes relative to that point (reset left this at the
            // frame-header size).
            state.bytes_read_counter = 0;
            effective_start
        } else {
            // Fresh decode: validate the caller's range (no state to mutate).
            if start_block > end_block {
                return Err(err::InvalidBlockRange {
                    start_block,
                    end_block,
                });
            }
            start_block
        };

        let mut block_dec = decoding::block_decoder::new();

        // Bytes of prefix-window output that physically precede the first
        // in-range block in the buffer. Captured at the prefix → in-range
        // transition (after leading blocks were dropped to the window) so we
        // can discard exactly those bytes once decoding is done. `None` until
        // the first in-range block is reached.
        let mut prefix_window_len: Option<usize> = None;
        // Exact count of clean in-range decompressed bytes (sum of per-block
        // length deltas of the in-range blocks that succeeded). Any partial
        // bytes of a failing in-range block are excluded — the fused executor
        // rolls the buffer back to the pre-block checkpoint on a sequence
        // error, and anything left over is never counted here, so it is not
        // drained into `data`.
        let mut subset_bytes: u64 = 0;
        let mut blocks_decoded: u32 = 0;
        let mut stopped_at: Option<(u32, FrameDecoderError)> = None;

        loop {
            let block_index = state.block_counter as u32;
            // Stop before decoding `end_block`: the trailing blocks are never
            // touched (the perf win), and the frame's tail is left unread.
            if block_index >= end_block || state.frame_finished {
                break;
            }
            let in_range = block_index >= effective_start;
            // Snapshot the window length at the prefix → in-range boundary.
            if in_range && prefix_window_len.is_none() {
                prefix_window_len = Some(state.decoder_scratch.buffer_len());
            }

            let block_frame_offset = state.bytes_read_counter as u32;
            let (block_header, block_header_size) = match block_dec.read_block_header(&mut source) {
                Ok(v) => v,
                Err(e) => {
                    stopped_at = Some((
                        block_index,
                        block_header_decode_error(e, block_index, block_frame_offset),
                    ));
                    break;
                }
            };
            state.bytes_read_counter += u64::from(block_header_size);

            let len_before = state.decoder_scratch.buffer_len();
            match state.decoder_scratch.decode_block_content(
                &mut block_dec,
                &block_header,
                &mut source,
            ) {
                Ok(body_read) => state.bytes_read_counter += body_read,
                Err(e) => {
                    stopped_at = Some((
                        block_index,
                        block_body_decode_error(
                            e,
                            block_index,
                            block_frame_offset,
                            &block_header,
                            block_header_size,
                        ),
                    ));
                    break;
                }
            }
            let produced = state.decoder_scratch.buffer_len() - len_before;
            // Per-block XXH64 capture, mirroring `decode_blocks`: hash this
            // block's just-decoded bytes BEFORE any window drop so the digest
            // count stays 1:1 with the blocks decoded on this path too. Covers
            // context (out-of-range) blocks as well, matching `decode_blocks`
            // which hashes every block it decodes.
            #[cfg(all(feature = "lsm", feature = "hash"))]
            if self.per_block_checksums_enabled {
                use core::hash::Hasher;
                let (s1, s2) = state.decoder_scratch.last_n_as_slices(produced);
                let mut h = twox_hash::XxHash64::with_seed(0);
                h.write(s1);
                h.write(s2);
                self.computed_block_checksums.push(h.finish() as u32);
            }
            state.block_counter += 1;
            if in_range {
                subset_bytes += produced as u64;
                blocks_decoded += 1;
            }

            if block_header.last_block {
                state.frame_finished = true;
                if state.frame_header.descriptor.content_checksum_flag() {
                    let mut chksum = [0u8; 4];
                    match source.read_exact(&mut chksum) {
                        Ok(()) => {
                            state.bytes_read_counter += 4;
                            state.check_sum = Some(u32::from_le_bytes(chksum));
                        }
                        // A trailing-checksum read failure does not invalidate
                        // the decoded bytes; surface it so the caller knows the
                        // frame tail was truncated, but keep `data`.
                        Err(e) => {
                            stopped_at = Some((block_index, err::FailedToReadChecksum(e)));
                        }
                    }
                }
                break;
            }

            // Leading (out-of-range) block: bound memory to the window. We
            // must NOT drop once in-range, or the in-range output we are about
            // to return would be discarded.
            if !in_range {
                state.decoder_scratch.buffer_drop_to_window_size();
            }
        }

        // Emit cross-block carry-over state for a later resume, if requested.
        // Captured AFTER the loop (entropy tables / repcode history are final)
        // but BEFORE the drain — the drain only touches the visible output, not
        // the entropy state or `total_output_counter`. `block_counter` /
        // `total_output()` give the resume coordinates: the next block to decode
        // and the cumulative decompressed offset before it (clean even after an
        // early stop, since a failed block rolls both back to its checkpoint).
        // Suppress the snapshot on the terminal block: `block_counter` is then
        // one past the last block (EOF), for which there is no next-block source
        // position to resume from. A resume needs a real following block.
        let resume_state = if emit_resume && !state.frame_finished {
            let (fse, huf, offset_hist) = state.decoder_scratch.export_entropy();
            Some(ResumeState {
                frame_key: FrameKey::from_state(state, magicless),
                block_index: state.block_counter as u32,
                output_offset: state.decoder_scratch.total_output(),
                fse,
                huf,
                offset_hist,
                #[cfg(feature = "hash")]
                window_hash: state.decoder_scratch.window_tail_hash(window_size),
            })
        } else {
            None
        };

        // The visible buffer is now `[prefix window][in-range clean][maybe
        // trailing garbage from a failed in-range block]`. Drop the prefix
        // window from the front (match resolution is complete, so it is no
        // longer needed), then drain exactly the clean in-range byte count.
        let w = prefix_window_len.unwrap_or(0);
        state.decoder_scratch.buffer_discard_front(w);
        let mut data = alloc::vec![0u8; subset_bytes as usize];
        state
            .decoder_scratch
            .buffer_read_all(&mut data)
            .map_err(err::FailedToDrainDecodebuffer)?;

        // Clear anything still buffered so a later `read()`/`collect()` on this
        // decoder cannot surface out-of-range bytes: the leading-block window
        // when no in-range block was reached (`prefix_window_len` stayed
        // `None`, so `w` was 0), or trailing garbage from a failed in-range
        // block. Only the returned `data` is the partial decode's output.
        let residual = state.decoder_scratch.buffer_len();
        state.decoder_scratch.buffer_discard_front(residual);

        Ok(PartialDecode {
            data,
            start_block: effective_start,
            blocks_decoded,
            stopped_at,
            frame_finished: state.frame_finished,
            resume_state,
        })
    }

    /// Collect bytes and retain window_size bytes while decoding is still going on.
    /// After decoding of the frame (is_finished() == true) has finished it will collect all remaining bytes
    pub fn collect(&mut self) -> Option<Vec<u8>> {
        let finished = self.is_finished();
        let state = self.state.as_mut()?;
        if finished {
            Some(state.decoder_scratch.buffer_drain())
        } else {
            state.decoder_scratch.buffer_drain_to_window_size()
        }
    }

    /// Collect bytes and retain window_size bytes while decoding is still going on.
    /// After decoding of the frame (is_finished() == true) has finished it will collect all remaining bytes
    pub fn collect_to_writer(&mut self, w: impl Write) -> Result<usize, Error> {
        let finished = self.is_finished();
        let state = match &mut self.state {
            None => return Ok(0),
            Some(s) => s,
        };
        if finished {
            state.decoder_scratch.buffer_drain_to_writer(w)
        } else {
            state.decoder_scratch.buffer_drain_to_window_size_writer(w)
        }
    }

    /// How many bytes can currently be collected from the decodebuffer, while decoding is going on this will be lower than the actual decodbuffer size
    /// because window_size bytes need to be retained for decoding.
    /// After decoding of the frame (is_finished() == true) has finished it will report all remaining bytes
    pub fn can_collect(&self) -> usize {
        let finished = self.is_finished();
        let state = match &self.state {
            None => return 0,
            Some(s) => s,
        };
        if finished {
            state.decoder_scratch.buffer_can_drain()
        } else {
            state
                .decoder_scratch
                .buffer_can_drain_to_window_size()
                .unwrap_or(0)
        }
    }

    /// Decodes as many blocks as possible from the source slice and reads from the decodebuffer into the target slice
    /// The source slice may contain only parts of a frame but must contain at least one full block to make progress
    ///
    /// By all means use decode_blocks if you have a io.Reader available. This is just for compatibility with other decompressors
    /// which try to serve an old-style c api
    ///
    /// Returns (read, written), if read == 0 then the source did not contain a full block and further calls with the same
    /// input will not make any progress!
    ///
    /// Note that no kind of block can be bigger than 128kb.
    /// So to be safe use at least 128*1024 (max block content size) + 3 (block_header size) + 18 (max frame_header size) bytes as your source buffer
    ///
    /// You may call this function with an empty source after all bytes have been decoded. This is equivalent to just call decoder.read(&mut target)
    pub fn decode_from_to(
        &mut self,
        source: &[u8],
        target: &mut [u8],
    ) -> Result<(usize, usize), FrameDecoderError> {
        use FrameDecoderError as err;
        let bytes_read_at_start = match &self.state {
            Some(s) => s.bytes_read_counter,
            None => 0,
        };

        if !self.is_finished() || self.state.is_none() {
            let mut mt_source = source;

            if self.state.is_none() {
                self.init(&mut mt_source)?;
            }

            //pseudo block to scope "state" so we can borrow self again after the block
            {
                let state = match &mut self.state {
                    Some(s) => s,
                    None => panic!("Bug in library"),
                };
                let mut block_dec = decoding::block_decoder::new();

                if state.frame_header.descriptor.content_checksum_flag()
                    && state.frame_finished
                    && state.check_sum.is_none()
                {
                    //this block is needed if the checksum were the only 4 bytes that were not included in the last decode_from_to call for a frame
                    if mt_source.len() >= 4 {
                        let chksum = mt_source[..4].try_into().expect("optimized away");
                        state.bytes_read_counter += 4;
                        let chksum = u32::from_le_bytes(chksum);
                        state.check_sum = Some(chksum);
                    }
                    return Ok((4, 0));
                }

                loop {
                    //check if there are enough bytes for the next header
                    if mt_source.len() < 3 {
                        break;
                    }
                    let block_index = state.block_counter as u32;
                    let block_frame_offset = state.bytes_read_counter as u32;
                    let (block_header, block_header_size) = block_dec
                        .read_block_header(&mut mt_source)
                        .map_err(|source| {
                            block_header_decode_error(source, block_index, block_frame_offset)
                        })?;

                    // check the needed size for the block before updating counters.
                    // If not enough bytes are in the source, the header will have to be read again, so act like we never read it in the first place
                    if mt_source.len() < block_header.content_size as usize {
                        break;
                    }
                    state.bytes_read_counter += u64::from(block_header_size);

                    let bytes_read_in_block_body = state
                        .decoder_scratch
                        .decode_block_content(&mut block_dec, &block_header, &mut mt_source)
                        .map_err(|source| {
                            block_body_decode_error(
                                source,
                                block_index,
                                block_frame_offset,
                                &block_header,
                                block_header_size,
                            )
                        })?;
                    state.bytes_read_counter += bytes_read_in_block_body;
                    state.block_counter += 1;

                    if block_header.last_block {
                        state.frame_finished = true;
                        if state.frame_header.descriptor.content_checksum_flag() {
                            //if there are enough bytes handle this here. Else the block at the start of this function will handle it at the next call
                            if mt_source.len() >= 4 {
                                let chksum = mt_source[..4].try_into().expect("optimized away");
                                state.bytes_read_counter += 4;
                                let chksum = u32::from_le_bytes(chksum);
                                state.check_sum = Some(chksum);
                            }
                        }
                        break;
                    }
                }
            }
        }

        let result_len = self.read(target).map_err(err::FailedToDrainDecodebuffer)?;
        let bytes_read_at_end = match &mut self.state {
            Some(s) => s.bytes_read_counter,
            None => panic!("Bug in library"),
        };
        let read_len = bytes_read_at_end - bytes_read_at_start;
        Ok((read_len as usize, result_len))
    }

    /// Decode multiple frames into the output slice.
    ///
    /// `input` must contain an exact number of frames. Skippable frames are allowed and will be
    /// skipped during decode.
    ///
    /// `output` must be large enough to hold the decompressed data. If you don't know
    /// how large the output will be, use [`FrameDecoder::decode_blocks`] instead.
    ///
    /// This calls [`FrameDecoder::init`], and all bytes currently in the decoder will be lost.
    ///
    /// Returns the number of bytes written to `output`.
    pub fn decode_all(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, FrameDecoderError> {
        #[cfg(not(feature = "lsm"))]
        {
            self.decode_all_impl(input, output, |this, src| this.init(src))
        }
        #[cfg(feature = "lsm")]
        {
            self.decode_all_impl(input, output, |this, src| this.init(src), None)
        }
    }

    /// Decode multiple frames into the output slice, invoking `visitor`
    /// for every skippable frame encountered before advancing past it.
    ///
    /// `input` must contain an exact number of frames. Skippable frames
    /// (RFC 8878 §3.1.2 magic numbers `0x184D2A50..=0x184D2A5F`) are
    /// allowed and will be both visited AND skipped: the visitor gets
    /// `(magic_variant, payload)` where `magic_variant` is the low
    /// nibble of the magic (`magic - 0x184D2A50`, range `0..=15`) and
    /// `payload` is a borrowed slice of the on-wire payload bytes (the
    /// skippable frame's `Frame_Size` field worth of data) into
    /// `input` — no allocation.
    ///
    /// The visitor sees skippable frames in stream order; interleaved
    /// regular zstd frames continue to decompress into `output` exactly
    /// as `decode_all` does.
    ///
    /// `output` must be large enough to hold the decompressed data.
    /// Returns the number of bytes written to `output`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use structured_zstd::decoding::FrameDecoder;
    ///
    /// let mut decoder = FrameDecoder::new();
    /// let mut output = vec![0u8; 1024];
    /// let mut collected: Vec<(u8, Vec<u8>)> = Vec::new();
    /// let n = decoder.decode_all_with_skippable_visitor(
    ///     input,
    ///     &mut output,
    ///     |variant, payload| collected.push((variant, payload.to_vec())),
    /// )?;
    /// ```
    #[cfg(feature = "lsm")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
    pub fn decode_all_with_skippable_visitor<F>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        mut visitor: F,
    ) -> Result<usize, FrameDecoderError>
    where
        F: FnMut(u8, &[u8]),
    {
        self.decode_all_impl(
            input,
            output,
            |this, src| this.init(src),
            Some(&mut visitor),
        )
    }

    /// Decode multiple frames into the output slice using a pre-parsed dictionary handle.
    ///
    /// `input` must contain an exact number of frames. Skippable frames are allowed and will be
    /// skipped during decode.
    ///
    /// `output` must be large enough to hold the decompressed data. If you don't know
    /// how large the output will be, use [`FrameDecoder::decode_blocks`] instead.
    ///
    /// This calls [`FrameDecoder::init_with_dict_handle`], and all bytes currently in the
    /// decoder will be lost.
    ///
    /// # Warning
    ///
    /// Each decoded frame is initialized with `dict`, even when a frame header
    /// omits the optional dictionary ID. Callers must only use this API when
    /// they already know the input frames were encoded with the provided
    /// dictionary; otherwise decoded output can be silently corrupted.
    pub fn decode_all_with_dict_handle(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        dict: &DictionaryHandle,
    ) -> Result<usize, FrameDecoderError> {
        #[cfg(not(feature = "lsm"))]
        {
            self.decode_all_impl(input, output, |this, src| {
                this.init_with_dict_handle(src, dict)
            })
        }
        #[cfg(feature = "lsm")]
        {
            self.decode_all_impl(
                input,
                output,
                |this, src| this.init_with_dict_handle(src, dict),
                None,
            )
        }
    }

    /// Default-feature decode_all_impl: no visitor parameter so the
    /// no-lsm build's call surface and codegen are byte-identical to
    /// the pre-#172 implementation. Compiles only when `lsm` is OFF.
    #[cfg(not(feature = "lsm"))]
    fn decode_all_impl(
        &mut self,
        mut input: &[u8],
        mut output: &mut [u8],
        mut init_frame: impl FnMut(&mut Self, &mut &[u8]) -> Result<(), FrameDecoderError>,
    ) -> Result<usize, FrameDecoderError> {
        let mut total_bytes_written = 0;
        while !input.is_empty() {
            match init_frame(self, &mut input) {
                Ok(_) => {}
                Err(FrameDecoderError::ReadFrameHeaderError(
                    crate::decoding::errors::ReadFrameHeaderError::SkipFrame { length, .. },
                )) => {
                    input = input
                        .get(length as usize..)
                        .ok_or(FrameDecoderError::FailedToSkipFrame)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            // Per-frame direct-path dispatch. Now safe to route the
            // public `decode_all` here because
            // `UserSliceBackend::exec_sequence_inline` returns
            // `Result<(), ExecuteSequencesError>` instead of
            // panicking on capacity overflow; the error propagates
            // up as `FrameDecoderError`. Eligibility (FCS > 0, no
            // active dict, remaining `output` slice has WILDCOPY
            // slack) puts the frame on the fast path that bypasses
            // the FlatBuf/Ring -> `read()` drain copy. Ineligible
            // frames (no FCS, active dict, output too small for
            // slack) fall through to the legacy `decode_blocks` +
            // `read` drain loop below.
            let (content_size, fcs_declared, dict_active, window_size) = {
                let state_ref = self.state.as_ref().expect("init populated state");
                (
                    state_ref.frame_header.frame_content_size(),
                    state_ref.frame_header.fcs_declared(),
                    state_ref.using_dict.is_some(),
                    state_ref.frame_header.window_size().unwrap_or(0),
                )
            };
            // Direct decode requires only that the caller slice holds the
            // declared content; the inline sequence-exec path no longer
            // needs `WILDCOPY_OVERLENGTH` trailing slack because the
            // trailing sequence(s) take the bounded (non-overshooting)
            // copy in `UserSliceBackend::exec_sequence_bounded`. This is
            // the universal "decode into an FCS-sized buffer" case (a
            // caller sizing `output` to exactly `frame_content_size`),
            // so dropping the slack requirement halves its peak alloc.
            //
            // Per-block checksums collected inside `run_direct_decode`
            // post-loop (over recorded (start, end) ranges of `output`)
            // so the direct path stays eligible AND keeps the
            // window-size cap (`drop_to_window_size`) between blocks
            // that the spec relies on for `offset <= window_size`
            // validation. Path choice no longer alters checksum
            // semantics.
            let direct_eligible =
                content_size > 0 && !dict_active && (output.len() as u64) >= content_size;
            if direct_eligible {
                let written = self.run_direct_decode(&mut input, output, content_size)?;
                output = &mut output[written..];
                total_bytes_written += written;
                // Per-frame content-checksum verification (no-op unless the
                // mode is `Verify` and the frame carries a checksum).
                #[cfg(feature = "hash")]
                self.verify_content_checksum()?;
                continue;
            }
            // Non-direct fallback: pre-reserve the backing buffer to
            // `window_size` in a single allocation before block decode
            // starts, so multi-segment frames don't pay repeated
            // `reserve_amortized` grow steps as blocks accumulate (each
            // block only reserves MAX_BLOCK_SIZE = 128 KiB, so a window
            // > 128 KiB otherwise grows through several intermediate
            // sizes with `alloc_zeroed + memcpy` each time).
            if let Some(state) = self.state.as_mut() {
                state.decoder_scratch.reserve_buffer(window_size as usize);
            }
            let frame_start_total = total_bytes_written;
            loop {
                self.decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1024 * 1024))?;
                let bytes_written = self
                    .read(output)
                    .map_err(FrameDecoderError::FailedToDrainDecodebuffer)?;
                output = &mut output[bytes_written..];
                total_bytes_written += bytes_written;
                if self.can_collect() != 0 {
                    return Err(FrameDecoderError::TargetTooSmall);
                }
                if self.is_finished() {
                    break;
                }
            }
            // Per-frame FCS validation on the legacy fallback path.
            // Use `fcs_declared()` (NOT `content_size > 0`) so an
            // empty frame with explicit FCS=0 on the wire still gets
            // validated.
            if fcs_declared {
                let produced = (total_bytes_written - frame_start_total) as u64;
                if produced != content_size {
                    return Err(FrameDecoderError::FrameContentSizeMismatch {
                        declared: content_size,
                        produced,
                    });
                }
            }
            // Per-frame content-checksum verification on the drain path: the
            // frame is fully decoded and drained here (is_finished + nothing
            // left to collect), so the running digest and stored value are
            // final. No-op unless the mode is `Verify`.
            #[cfg(feature = "hash")]
            self.verify_content_checksum()?;
        }

        Ok(total_bytes_written)
    }

    /// `lsm`-feature decode_all_impl: adds the optional skippable
    /// visitor parameter consumed by
    /// [`Self::decode_all_with_skippable_visitor`]. Mirrors the no-lsm
    /// variant including the direct-path dispatch + FCS-validation
    /// rationale comments, so the two functions stay in sync; the only
    /// behavioral difference is the SkipFrame arm, which uses
    /// `split_at(length)` (single bounds check) instead of two
    /// separate `get(..length)` / `get(length..)` slices and invokes
    /// the visitor (when `Some`) on the borrowed payload before
    /// advancing past it.
    #[cfg(feature = "lsm")]
    #[allow(clippy::type_complexity)]
    fn decode_all_impl(
        &mut self,
        mut input: &[u8],
        mut output: &mut [u8],
        mut init_frame: impl FnMut(&mut Self, &mut &[u8]) -> Result<(), FrameDecoderError>,
        mut skippable_visitor: Option<&mut dyn FnMut(u8, &[u8])>,
    ) -> Result<usize, FrameDecoderError> {
        let mut total_bytes_written = 0;
        while !input.is_empty() {
            match init_frame(self, &mut input) {
                Ok(_) => {}
                Err(FrameDecoderError::ReadFrameHeaderError(
                    crate::decoding::errors::ReadFrameHeaderError::SkipFrame {
                        magic_number,
                        length,
                    },
                )) => {
                    let length = length as usize;
                    // Visitor sees the payload slice BEFORE we advance
                    // past it. Borrowed slice — no allocation. The
                    // variant is the low nibble of the magic number
                    // (RFC 8878 §3.1.2). `read_frame_header` only emits
                    // SkipFrame for magic in 0x184D2A50..=0x184D2A5F, so
                    // the subtraction fits in 0..=15.
                    if input.len() < length {
                        return Err(FrameDecoderError::FailedToSkipFrame);
                    }
                    let (payload, rest) = input.split_at(length);
                    if let Some(visitor) = skippable_visitor.as_mut() {
                        let variant = (magic_number - 0x184D2A50) as u8;
                        visitor(variant, payload);
                    }
                    input = rest;
                    continue;
                }
                Err(e) => return Err(e),
            };
            // Per-frame direct-path dispatch. Now safe to route the
            // public `decode_all` here because
            // `UserSliceBackend::exec_sequence_inline` returns
            // `Result<(), ExecuteSequencesError>` instead of
            // panicking on capacity overflow; the error propagates
            // up as `FrameDecoderError`. Eligibility (FCS > 0, no
            // active dict, remaining `output` slice has WILDCOPY
            // slack) puts the frame on the fast path that bypasses
            // the FlatBuf/Ring -> `read()` drain copy. Ineligible
            // frames (no FCS, active dict, output too small for
            // slack) fall through to the legacy `decode_blocks` +
            // `read` drain loop below.
            let (content_size, fcs_declared, dict_active, window_size) = {
                let state_ref = self.state.as_ref().expect("init populated state");
                (
                    state_ref.frame_header.frame_content_size(),
                    state_ref.frame_header.fcs_declared(),
                    state_ref.using_dict.is_some(),
                    state_ref.frame_header.window_size().unwrap_or(0),
                )
            };
            // Only `cap >= frame_content_size` needed; the trailing
            // sequence(s) take the bounded copy in
            // `UserSliceBackend::exec_sequence_bounded`, so no
            // `WILDCOPY_OVERLENGTH` trailing slack is required (see the
            // no-lsm path above).
            let direct_eligible =
                content_size > 0 && !dict_active && (output.len() as u64) >= content_size;
            if direct_eligible {
                let written = self.run_direct_decode(&mut input, output, content_size)?;
                output = &mut output[written..];
                total_bytes_written += written;
                // Per-frame content-checksum verification (no-op unless the
                // mode is `Verify` and the frame carries a checksum).
                #[cfg(feature = "hash")]
                self.verify_content_checksum()?;
                continue;
            }
            // Non-direct fallback: pre-reserve the backing buffer to
            // `window_size` once so the per-block growth cycle is
            // skipped (see same comment on the no-lsm path above).
            if let Some(state) = self.state.as_mut() {
                state.decoder_scratch.reserve_buffer(window_size as usize);
            }
            let frame_start_total = total_bytes_written;
            loop {
                self.decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1024 * 1024))?;
                let bytes_written = self
                    .read(output)
                    .map_err(FrameDecoderError::FailedToDrainDecodebuffer)?;
                output = &mut output[bytes_written..];
                total_bytes_written += bytes_written;
                if self.can_collect() != 0 {
                    return Err(FrameDecoderError::TargetTooSmall);
                }
                if self.is_finished() {
                    break;
                }
            }
            // Per-frame FCS validation on the legacy fallback path.
            // Use `fcs_declared()` (NOT `content_size > 0`) so an
            // empty frame with explicit FCS=0 on the wire still gets
            // validated.
            if fcs_declared {
                let produced = (total_bytes_written - frame_start_total) as u64;
                if produced != content_size {
                    return Err(FrameDecoderError::FrameContentSizeMismatch {
                        declared: content_size,
                        produced,
                    });
                }
            }
            // Per-frame content-checksum verification on the drain path: the
            // frame is fully decoded and drained here (is_finished + nothing
            // left to collect), so the running digest and stored value are
            // final. No-op unless the mode is `Verify`.
            #[cfg(feature = "hash")]
            self.verify_content_checksum()?;
        }

        Ok(total_bytes_written)
    }

    /// Decode multiple frames into the output slice using a serialized dictionary.
    ///
    /// # Warning
    ///
    /// Each decoded frame is initialized with the parsed dictionary, even when a
    /// frame header omits the optional dictionary ID. Callers must only use this
    /// API when they already know the input frames were encoded with that
    /// dictionary; otherwise decoded output can be silently corrupted.
    pub fn decode_all_with_dict_bytes(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        raw_dictionary: &[u8],
    ) -> Result<usize, FrameDecoderError> {
        let dict = DictionaryHandle::decode_dict(raw_dictionary)?;
        self.decode_all_with_dict_handle(input, output, &dict)
    }

    /// Decode multiple frames into the extra capacity of the output vector.
    ///
    /// `input` must contain an exact number of frames.
    ///
    /// `output` must have enough spare capacity to hold the decompressed
    /// data. This adds no extra slack: exact-fit output is now eligible
    /// for the direct decode path, so a `Vec::with_capacity(fcs)` is
    /// decoded straight into without a growth/reallocation. It will NOT
    /// grow the vector to fit the decompressed payload itself; the
    /// caller's pre-allocated capacity must already cover the data. If
    /// you don't know how large the output will be, use
    /// [`FrameDecoder::decode_blocks`] instead.
    ///
    /// This calls [`FrameDecoder::init`], and all bytes currently in the decoder will be lost.
    ///
    /// The length of the output vector is updated to include the
    /// decompressed data. The length is not changed if an error occurs.
    pub fn decode_all_to_vec(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<(), FrameDecoderError> {
        let len = output.len();
        let cap = output.capacity();
        output.resize(cap, 0);
        match self.decode_all(input, &mut output[len..]) {
            Ok(bytes_written) => {
                let new_len = core::cmp::min(len + bytes_written, cap); // Sanitizes `bytes_written`.
                output.resize(new_len, 0);
                Ok(())
            }
            Err(e) => {
                output.resize(len, 0);
                Err(e)
            }
        }
    }

    /// Single-frame direct-decode path. Decodes one zstd frame into
    /// `output[..content_size]` via a stack-local
    /// `DecodeBuffer<UserSliceBackend>`, bypassing the per-block
    /// FlatBuf/Ring -> `read()` drain copy.
    ///
    /// # Preconditions (caller-enforced)
    ///
    /// - `self.init` (or `init_with_dict_handle`) was called for
    ///   this frame so `self.state` is populated.
    /// - `content_size` matches `self.state.frame_header
    ///   .frame_content_size()` and is `> 0` (caller already passed
    ///   the eligibility gate).
    /// - `output.len() >= content_size`. No `WILDCOPY_OVERLENGTH`
    ///   trailing slack is required: the trailing sequence(s) take the
    ///   bounded (non-overshooting) copy in
    ///   [`UserSliceBackend::exec_sequence_bounded`].
    /// - No active dictionary
    ///   (`self.state.using_dict.is_none()`).
    ///
    /// On return, `input` points at the byte immediately after the
    /// frame's checksum (or after the last block, when the frame
    /// has `content_checksum_flag = 0`). `self.state.frame_finished`
    /// is set so [`Self::is_finished`] reports `true`.
    fn run_direct_decode(
        &mut self,
        input: &mut &[u8],
        output: &mut [u8],
        content_size: u64,
    ) -> Result<usize, FrameDecoderError> {
        use super::block_decoder;
        use super::decode_buffer::DecodeBuffer;
        use super::scratch::DirectScratch;
        use super::user_slice_buf::UserSliceBackend;
        use crate::io::Read;
        use FrameDecoderError as err;

        let state = self
            .state
            .as_mut()
            .expect("caller ensures init populated state");

        // Borrow persistent fields out of whichever scratch variant
        // `init` produced (Flat for single_segment, Ring for
        // multi-segment) — both expose the same HUF/FSE/Vec
        // fields; only `buffer` differs and we don't use that here.
        // Macro-style binding avoids the closure / generic
        // gymnastics of returning multiple `&mut` from a match arm.
        let (huf, fse, offset_hist, literals_buffer, block_content_buffer, window_size) =
            match &mut state.decoder_scratch {
                DecoderScratchKind::Flat(s) => (
                    &mut s.huf,
                    &mut s.fse,
                    &mut s.offset_hist,
                    &mut s.literals_buffer,
                    &mut s.block_content_buffer,
                    s.buffer.window_size,
                ),
                DecoderScratchKind::Ring(s) => (
                    &mut s.huf,
                    &mut s.fse,
                    &mut s.offset_hist,
                    &mut s.literals_buffer,
                    &mut s.block_content_buffer,
                    s.buffer.window_size,
                ),
            };
        let backend = UserSliceBackend::from_slice(output);
        let buffer = DecodeBuffer::from_backend(backend, window_size);
        let mut direct = DirectScratch {
            huf,
            fse,
            offset_hist,
            literals_buffer,
            block_content_buffer,
            buffer,
        };

        // Block loop. Mirrors `decode_blocks` (without the
        // strategy-bounded early exit — we always decode the whole
        // frame in one shot for the direct path). Keeps
        // `state.bytes_read_counter` / `state.block_counter` in
        // sync with `decode_blocks` so post-call accessors
        // (`bytes_read_from_source`, `blocks_decoded`) return
        // accurate values.
        let mut block_dec = block_decoder::new();
        // Track total output bytes against the declared
        // `frame_content_size` via the buffer's actual write
        // counter — `BlockHeader.decompressed_size` is 0 for
        // Compressed blocks (the header parser can't know the
        // expanded size before decoding the body), so per-header
        // tracking would always count 0 for those blocks and
        // miscount frames that aren't pure Raw/RLE.
        let mut produced: u64 = 0;
        // Per-block output ranges captured during the direct-path
        // loop. After the loop we re-borrow `output` (post-drop of
        // `direct`) and XXH64 each range into
        // `self.computed_block_checksums`, so the digests vector
        // stays consistent with the legacy `decode_blocks` path
        // regardless of which dispatch the frame took.
        // `Vec::new()` does not allocate, so this stays free when
        // `per_block_checksums_enabled` is false: the `push` and the
        // post-loop hashing loop are both gated by the same flag.
        #[cfg(all(feature = "lsm", feature = "hash"))]
        let mut block_ranges: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        loop {
            #[cfg(all(feature = "lsm", feature = "hash"))]
            let produced_before: Option<usize> = if self.per_block_checksums_enabled {
                Some(produced as usize)
            } else {
                None
            };
            // Failing-block coordinates captured before the header read (see
            // the `decode_blocks` loop for the rationale).
            let block_index = state.block_counter as u32;
            let block_frame_offset = state.bytes_read_counter as u32;
            let (block_header, hsize) =
                block_dec.read_block_header(&mut *input).map_err(|source| {
                    block_header_decode_error(source, block_index, block_frame_offset)
                })?;
            state.bytes_read_counter += u64::from(hsize);
            // Pre-flight FCS check ONLY for Raw / RLE blocks where
            // `decompressed_size` is the actual block output size.
            // For Compressed blocks the header field is 0; the
            // post-decode check below catches overflow via the
            // backend's actual write counter delta.
            let block_upper = u64::from(block_header.decompressed_size);
            if block_upper > 0 && produced + block_upper > content_size {
                // Frame is corrupt — Raw/RLE block headers claim
                // more output than the FCS allows.
                return Err(err::FrameContentSizeMismatch {
                    declared: content_size,
                    produced: produced + block_upper,
                });
            }
            // Slice-source fast path: consume the block body
            // straight from `input` without copying into the
            // persistent `block_content_buffer`.
            let body_consumed = match block_dec.decode_block_content_from_slice(
                &block_header,
                &mut direct,
                &mut *input,
            ) {
                Ok(n) => n,
                // Defense-in-depth: RLE / Raw block whose declared
                // `decompressed_size` slipped past the per-block
                // pre-flight above and tripped the backend's
                // fallible write surface.
                Err(crate::decoding::errors::DecodeBlockContentError::BackendOverflow {
                    ..
                }) => {
                    // Use saturating_add on the
                    // `produced + decompressed_size` sum. Each block
                    // is bounded by 128 KiB (MAX_BLOCK_SIZE), but
                    // accumulated `produced` can grow toward
                    // u64::MAX across adversarial frames. Saturating
                    // avoids a panic on the error path itself.
                    return Err(err::FrameContentSizeMismatch {
                        declared: content_size,
                        produced: produced
                            .saturating_add(u64::from(block_header.decompressed_size)),
                    });
                }
                // Compressed-block in-block overshoot: the sequence
                // executor (donor-inline path) or the match-repeat
                // fallback tripped the fixed-capacity backend's per-write
                // check. Unlike Raw/RLE, a Compressed block carries no
                // header-declared output size, so `produced` is computed
                // from the partial fill: `tail` bytes were written before
                // the failing op, and `requested` is what overflowed —
                // their sum is a strict lower bound on the frame's true
                // expanded size and is always > `content_size` (the
                // direct path is only entered when the slice is sized to
                // `content_size + WILDCOPY_OVERLENGTH`, so any overflow
                // means the frame exceeded the declared FCS, never a
                // caller-undersized buffer). Folds into the same
                // `FrameContentSizeMismatch` contract as Raw/RLE.
                Err(crate::decoding::errors::DecodeBlockContentError::DecompressBlockError(
                    crate::decoding::errors::DecompressBlockError::ExecuteSequencesError(ref e),
                )) if e.output_overflow_requested().is_some() => {
                    let requested = e
                        .output_overflow_requested()
                        .expect("guard guarantees Some") as u64;
                    let tail = direct.buffer.buffer_ref().tail() as u64;
                    return Err(err::FrameContentSizeMismatch {
                        declared: content_size,
                        produced: tail.saturating_add(requested),
                    });
                }
                Err(e) => {
                    return Err(block_body_decode_error(
                        e,
                        block_index,
                        block_frame_offset,
                        &block_header,
                        hsize,
                    ));
                }
            };
            produced = direct.buffer.buffer_ref().tail() as u64;
            // Post-decode FCS overflow check.
            if produced > content_size {
                return Err(err::FrameContentSizeMismatch {
                    declared: content_size,
                    produced,
                });
            }
            state.bytes_read_counter += body_consumed;
            state.block_counter += 1;
            #[cfg(all(feature = "lsm", feature = "hash"))]
            if let Some(produced_before) = produced_before {
                block_ranges.push((produced_before, produced as usize));
            }
            // Cap the visible buffer at window_size between blocks
            // so the next block's match-offset validation matches
            // the spec's `offset <= window_size` rule.
            direct.buffer.drop_to_window_size();
            if block_header.last_block {
                if state.frame_header.descriptor.content_checksum_flag() {
                    let mut chksum = [0u8; 4];
                    input
                        .read_exact(&mut chksum)
                        .map_err(err::FailedToReadChecksum)?;
                    state.bytes_read_counter += 4;
                    state.check_sum = Some(u32::from_le_bytes(chksum));
                }
                break;
            }
        }
        // Final sanity: blocks summed to exactly `content_size`.
        if produced != content_size {
            return Err(err::FrameContentSizeMismatch {
                declared: content_size,
                produced,
            });
        }

        let written = content_size as usize;
        state.frame_finished = true;
        // Drop the stack-local DirectScratch (and its DecodeBuffer
        // borrow on `output`) so we can re-borrow `output` for the
        // hash pass below.
        drop(direct);
        // Per-block XXH64 (low 32 bits) over the captured ranges.
        // Mirrors `decode_blocks`' per-block hashing so the digests
        // vector stays identical regardless of which dispatch path
        // the frame took. Ranges were recorded inside the loop while
        // `direct` held a mutable borrow on `output`; now that the
        // borrow is dropped we can read the slices directly.
        #[cfg(all(feature = "lsm", feature = "hash"))]
        if self.per_block_checksums_enabled {
            use core::hash::Hasher;
            for (start, end) in &block_ranges {
                let mut h = twox_hash::XxHash64::with_seed(0);
                h.write(&output[*start..*end]);
                self.computed_block_checksums.push(h.finish() as u32);
            }
        }
        #[cfg(feature = "hash")]
        if state.frame_header.descriptor.content_checksum_flag()
            && self.content_checksum != ContentChecksum::None
        {
            // Direct path bypasses the per-write hash accounting
            // (DecodeBuffer hashes during drain; the direct path
            // never drains because the user slice IS the buffer).
            // Walk the decoded output once and propagate the
            // resulting hasher state so the frame-tail XXH64 check
            // (in the block loop above) can verify the digest.
            //
            // Gated on `content_checksum_flag`: frames without a
            // trailing checksum byte have nothing to verify, and the
            // 1 MiB+ second-pass scan dominated decode wall time
            // (63 % of total on z000033 L-3 in the standalone perf
            // loop). `get_calculated_checksum()` now returns `None`
            // for flag-off frames, matching this skip.
            use core::hash::Hasher;
            let mut hasher = twox_hash::XxHash64::with_seed(0);
            hasher.write(&output[..written]);
            match &mut state.decoder_scratch {
                DecoderScratchKind::Flat(s) => s.buffer.hash = hasher,
                DecoderScratchKind::Ring(s) => s.buffer.hash = hasher,
            }
        }
        Ok(written)
    }
}

/// Read bytes from the decode_buffer that are no longer needed. While the frame is not yet finished
/// this will retain window_size bytes, else it will drain it completely
impl Read for FrameDecoder {
    fn read(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        let state = match &mut self.state {
            None => return Ok(0),
            Some(s) => s,
        };
        if state.frame_finished {
            state.decoder_scratch.buffer_read_all(target)
        } else {
            state.decoder_scratch.buffer_read(target)
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{DictionaryHandle, FrameDecoder};
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec::Vec;

    #[test]
    fn decode_all_tight_and_slack_outputs_match_on_single_segment_frame() {
        // Roundtrip a small payload through the encoder, then decode
        // it via `decode_all` on two output shapes that select
        // different internal sequence-exec paths within the direct
        // decode:
        //   1. Tight output (exactly `frame_content_size`, no
        //      WILDCOPY_OVERLENGTH slack) → direct path whose trailing
        //      sequence(s) take the bounded (non-overshooting) copy in
        //      `UserSliceBackend::exec_sequence_bounded`.
        //   2. Output with WILDCOPY slack → direct path whose
        //      sequences all take the SIMD wildcopy fast path.
        // Both must produce identical output bytes — the bounded tail
        // copy must reconstruct the same data as the overshooting fast
        // path. This is the regression gate for the relaxed
        // direct-decode gate (`cap >= content_size`).
        let payload: Vec<u8> = (0..4096u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        // Baseline: tight output → legacy drain path.
        let mut dec_a = FrameDecoder::new();
        let mut out_a = alloc::vec![0u8; payload.len()];
        let n_a = dec_a
            .decode_all(compressed.as_slice(), &mut out_a)
            .expect("decode_all (legacy drain) should succeed");
        assert_eq!(n_a, payload.len());
        assert_eq!(&out_a[..n_a], payload.as_slice());

        // Direct: output with WILDCOPY slack → direct path.
        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec_b = FrameDecoder::new();
        let mut out_b = alloc::vec![0u8; payload.len() + slack];
        let n_b = dec_b
            .decode_all(compressed.as_slice(), &mut out_b)
            .expect("decode_all (direct path) should succeed");
        assert_eq!(
            n_b,
            payload.len(),
            "direct decode produced wrong byte count"
        );
        assert_eq!(&out_b[..n_b], payload.as_slice());
    }

    #[test]
    fn decode_all_tight_output_overlapping_tail_match_roundtrips() {
        // The bounded tail copy must handle an OVERLAPPING match
        // (offset < match_length) as the trailing sequence when the
        // output slice is sized to exactly `frame_content_size`. A long
        // run of a single byte at the end of the payload encodes as an
        // offset-1 match whose length far exceeds the offset, so the
        // bounded copy's overlapping (forward byte-by-byte) branch is
        // exercised at the buffer tail where the SIMD overshoot would
        // otherwise run past `cap`. Decoding into a tight buffer and
        // matching the original payload byte-for-byte is the regression
        // gate for the overlap branch of `exec_sequence_bounded`.
        let mut payload: Vec<u8> = (0..256u32).map(|i| (i & 0xFF) as u8).collect();
        payload.extend(core::iter::repeat_n(0xABu8, 8192));
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        // Anti-vacuous precondition: the 8 KiB trailing run of a single
        // byte must compress to a Compressed block dominated by ONE long
        // offset-1 (overlapping, offset < match_length) match — not a Raw
        // block. If the encoder ever stopped emitting that overlapping
        // tail match the test would pass without exercising
        // `exec_sequence_bounded`'s overlapping forward-copy branch, so
        // gate on the output being a tiny fraction of the input (a raw
        // block would be ~`payload.len()`; an offset-1 run match is tens
        // of bytes).
        assert!(
            compressed.len() < payload.len() / 8,
            "expected an overlapping-tail match to dominate the frame \
             (compressed={} payload={}); the bounded overlap branch would \
             not be exercised otherwise",
            compressed.len(),
            payload.len(),
        );

        // Tight output: exactly content_size, no WILDCOPY slack.
        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; payload.len()];
        let n = dec
            .decode_all(compressed.as_slice(), &mut out)
            .expect("tight-output decode with overlapping tail match should succeed");
        assert_eq!(n, payload.len());
        assert_eq!(out, payload, "bounded overlap tail copy corrupted output");
    }

    #[test]
    fn decode_all_multi_segment_frame_decodes_correctly() {
        // Multi-segment frame: payload large enough that the
        // encoder's default frame layout has `single_segment_flag =
        // false` and `window_size < frame_content_size`. The direct
        // path must cap the visible buffer at window_size after each
        // block (drop_to_window_size) so match-offset validation
        // matches the spec rule `offset <= window_size`, and still
        // produce the same bytes as decode_all on the
        // FlatBuf/Ring-backed path.
        //
        // Make the payload structured so multi-segment behavior
        // actually kicks in: 2 MiB of repeating + random-ish bytes
        // forces window_size lower than content_size at the encoder.
        let mut payload: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024);
        for i in 0..payload.capacity() {
            payload.push((i.wrapping_mul(2_654_435_761) & 0xFF) as u8);
        }
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        // Baseline: decode_all through the FlatBuf+drain path.
        let mut dec_a = FrameDecoder::new();
        let mut out_a = alloc::vec![0u8; payload.len()];
        let n_a = dec_a
            .decode_all(compressed.as_slice(), &mut out_a)
            .expect("decode_all should succeed");
        assert_eq!(n_a, payload.len());
        assert_eq!(&out_a[..n_a], payload.as_slice());

        // Direct path: must give identical bytes via UserSliceBackend
        // + per-block drop_to_window_size.
        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec_b = FrameDecoder::new();
        let mut out_b = alloc::vec![0u8; payload.len() + slack];
        let n_b = dec_b
            .decode_all(compressed.as_slice(), &mut out_b)
            .expect("decode_all should succeed on multi-segment frame");
        assert_eq!(n_b, payload.len(), "wrong byte count on direct path");
        assert_eq!(&out_b[..n_b], payload.as_slice());

        // Sanity-check: confirm the encoded frame really IS
        // multi-segment. If a future encoder default changes,
        // catching the assumption here is better than silently
        // testing single_segment on this name.
        let mut sanity = FrameDecoder::new();
        sanity.init(&mut compressed.as_slice()).unwrap();
        assert!(
            !sanity
                .state
                .as_ref()
                .unwrap()
                .frame_header
                .descriptor
                .single_segment_flag(),
            "test precondition violated: frame is single-segment, rename or resize"
        );
    }

    #[cfg(feature = "hash")]
    #[test]
    fn decode_all_propagates_checksum_into_persistent_scratch() {
        // Direct path on a checksum-flagged frame: the FrameCompressor
        // under `feature = "hash"` sets content_checksum_flag, so the
        // decoded frame has a recorded checksum. After
        // decode_all we must be able to verify it matches via
        // the public get_calculated_checksum() accessor — the digest
        // is computed by walking output at end of decode and stored
        // into the persistent scratch's hasher.
        let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; payload.len() + slack];
        let n = dec
            .decode_all(compressed.as_slice(), &mut out)
            .expect("decode_all with checksum must succeed");
        assert_eq!(n, payload.len());
        assert_eq!(&out[..n], payload.as_slice());

        // Both sides must report the same checksum: the frame header
        // carries the stored u32, and get_calculated_checksum reads
        // the running digest the direct path just propagated.
        let stored = dec.get_checksum_from_data();
        let calculated = dec.get_calculated_checksum();
        assert!(stored.is_some(), "frame must carry stored checksum");
        assert!(
            calculated.is_some(),
            "direct path must propagate calculated checksum"
        );
        assert_eq!(
            stored, calculated,
            "stored vs calculated checksum mismatch on direct path"
        );
    }

    #[cfg(feature = "hash")]
    #[test]
    fn verify_mode_accepts_a_valid_frame() {
        use crate::decoding::ContentChecksum;
        let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        dec.set_content_checksum(ContentChecksum::Verify);
        let mut out = alloc::vec![0u8; payload.len() + slack];
        let n = dec
            .decode_all(compressed.as_slice(), &mut out)
            .expect("Verify mode must accept a frame with a correct checksum");
        assert_eq!(&out[..n], payload.as_slice());
    }

    #[cfg(feature = "hash")]
    #[test]
    fn verify_mode_rejects_a_corrupted_checksum() {
        use crate::decoding::ContentChecksum;
        use crate::decoding::errors::FrameDecoderError;
        let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        // Flip a bit in the trailing 4-byte content checksum: the frame body
        // still decodes to the correct bytes, but the stored digest no longer
        // matches the one the decoder computes.
        let last = compressed.len() - 1;
        compressed[last] ^= 0xFF;

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        dec.set_content_checksum(ContentChecksum::Verify);
        let mut out = alloc::vec![0u8; payload.len() + slack];
        let err = dec
            .decode_all(compressed.as_slice(), &mut out)
            .expect_err("Verify mode must reject a corrupted checksum");
        assert!(
            matches!(err, FrameDecoderError::ChecksumMismatch { .. }),
            "expected ChecksumMismatch, got {err:?}"
        );
    }

    #[cfg(feature = "hash")]
    #[test]
    fn none_mode_skips_the_checksum_pass() {
        use crate::decoding::ContentChecksum;
        let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        dec.set_content_checksum(ContentChecksum::None);
        let mut out = alloc::vec![0u8; payload.len() + slack];
        let n = dec
            .decode_all(compressed.as_slice(), &mut out)
            .expect("None mode must still decode correctly");
        assert_eq!(&out[..n], payload.as_slice());
        // No digest is computed in None mode, even though the frame carries one.
        assert!(dec.get_checksum_from_data().is_some());
        assert!(dec.get_calculated_checksum().is_none());
    }

    #[cfg(feature = "hash")]
    #[test]
    fn encoder_without_checksum_emits_no_trailing_digest() {
        let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();

        let mut with = Vec::new();
        let mut c_with = FrameCompressor::new(CompressionLevel::Default);
        c_with.set_source(payload.as_slice());
        c_with.set_drain(&mut with);
        c_with.compress();

        let mut without = Vec::new();
        let mut c_without = FrameCompressor::new(CompressionLevel::Default);
        c_without.set_content_checksum(false);
        c_without.set_source(payload.as_slice());
        c_without.set_drain(&mut without);
        c_without.compress();

        // The checksum-off frame is exactly the 4-byte trailing digest shorter.
        assert_eq!(with.len(), without.len() + 4);

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; payload.len() + slack];
        let n = dec
            .decode_all(without.as_slice(), &mut out)
            .expect("a frame without a content checksum must decode");
        assert_eq!(&out[..n], payload.as_slice());
        assert!(
            dec.get_checksum_from_data().is_none(),
            "no trailing checksum should be reported"
        );
    }

    #[test]
    fn decode_all_fcs_overflow_via_corrupt_frame_returns_structured_error() {
        // Hand-build a corrupt frame that declares
        // frame_content_size = 4 but the (last) block carries a
        // larger Raw payload. The pre-flight FCS check inside the
        // direct path's block loop catches this and returns the
        // structured FrameContentSizeMismatch variant — not a
        // panic, not a generic TargetTooSmall.
        //
        // Frame layout (single_segment, FCS=4):
        //   magic            4 bytes  0xFD2FB528
        //   FHD              1 byte   single_segment=1, no checksum,
        //                              FCS field size = 0 (-> 1-byte FCS)
        //   FCS              1 byte   0x04
        //   block_header     3 bytes  last=1, type=Raw, block_size=10
        //   block_payload    10 bytes 0xAA repeated
        let mut frame = alloc::vec::Vec::new();
        // magic
        frame.extend_from_slice(&0xFD2FB528u32.to_le_bytes());
        // FHD: single_segment=1, fcs_flag=0 (1-byte FCS), no checksum,
        // no dict. Bit layout: FCS(7-6)=0, single_segment(5)=1,
        // reserved/uncs(4)=0, content_checksum(2)=0, dict(0-1)=00.
        frame.push(0b0010_0000);
        // FCS: 1 byte
        frame.push(4);
        // Block header: cBlockSize=10, type=Raw (0), last=1
        // 3-byte LE: bit0=last, bits1-2=type(2 bits), bits3-23=size
        let cblock_size: u32 = 10;
        let bh: u32 = 1 | (cblock_size << 3); // last=1, type=Raw=0
        frame.push((bh & 0xFF) as u8);
        frame.push((bh >> 8) as u8);
        frame.push((bh >> 16) as u8);
        // Payload — 10 bytes that, if decoded, would exceed FCS=4.
        frame.extend(core::iter::repeat_n(0xAAu8, 10));

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; 4 + slack];
        let err = dec
            .decode_all(&frame, &mut out)
            .expect_err("FCS-overflow frame must fail decode");
        assert!(
            matches!(
                err,
                super::FrameDecoderError::FrameContentSizeMismatch { .. }
            ),
            "expected FrameContentSizeMismatch, got {:?}",
            err
        );
    }

    #[test]
    fn decode_all_compressed_block_fcs_overflow_returns_structured_error() {
        // Acceptance test for #246: a malformed frame whose *Compressed*
        // block expands past the declared `frame_content_size` must
        // surface `FrameContentSizeMismatch` from the direct-decode path
        // (UserSliceBackend sequence executor), NOT panic and NOT a
        // generic FailedToReadBlockBody. The Raw-block sibling above
        // covers the `BackendOverflow` arm; this covers the Compressed
        // sequence-executor overflow arm (`ExecuteSequencesError::
        // OutputBufferOverflow` folded into FrameContentSizeMismatch in
        // `run_direct_decode`).
        //
        // Construction: compress a compressible payload to get a genuine
        // Compressed block + a header-declared FCS, then surgically patch
        // the FCS field down to a tiny value. The block body still
        // decodes (literals/sequences are independent of FCS) and the
        // sequence executor overflows the small output slice.
        // Highly compressible payload (repeated phrase) → Compressed
        // block whose sequence executor produces ~4 KiB of output.
        let unit = b"The quick brown fox jumps over the lazy dog. ";
        let mut payload = Vec::with_capacity(4 * 1024);
        while payload.len() < 4 * 1024 {
            payload.extend_from_slice(unit);
        }
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut frame = Vec::new();
        compressor.set_drain(&mut frame);
        compressor.compress();
        // Sanity: the encoder actually compressed (=> a Compressed block,
        // not a raw-stored fallback) so we exercise the sequence path.
        assert!(frame.len() < payload.len());

        // Locate the FCS field: it is the last `fcs_len` bytes of the
        // frame header, whose total size `header_size` includes the magic.
        // A ~4 KiB single-segment frame declares FCS = 4096, which lands in
        // the 2-byte field range [256, 65791] (RFC 8878 §3.1.1.1.4) — assert
        // that so the patch logic below stays a single deterministic branch.
        let (header, header_size) =
            super::super::frame::read_frame_header(frame.as_slice()).expect("valid header");
        let fcs_len = header
            .descriptor
            .frame_content_size_bytes()
            .expect("fcs present") as usize;
        assert_eq!(
            fcs_len, 2,
            "4 KiB single-segment frame must use a 2-byte FCS"
        );
        let fcs_off = header_size as usize - fcs_len;

        // Patch the 2-byte FCS to its floor: stored bytes 0 decode to 256
        // (the field's `+256` bias), far below the 4 KiB the block actually
        // produces, so the sequence executor overflows the output slice.
        let patched_declared: u64 = 256;
        frame[fcs_off] = 0;
        frame[fcs_off + 1] = 0;

        // Size the output to declared + WILDCOPY slack so the direct path
        // is eligible (output.len() >= content_size + slack) — the
        // overflow then comes from the frame, not an undersized buffer.
        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut out = alloc::vec![0u8; patched_declared as usize + slack];
        let mut dec = FrameDecoder::new();
        let err = dec
            .decode_all(frame.as_slice(), &mut out)
            .expect_err("Compressed block exceeding FCS must fail decode");
        match err {
            super::FrameDecoderError::FrameContentSizeMismatch { declared, produced } => {
                assert_eq!(declared, patched_declared, "declared echoes patched FCS");
                assert!(produced > declared, "produced must exceed declared");
            }
            other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
        }
    }

    /// Block-precise error positions (#174): a failing block header / body
    /// reports its 0-based index and frame-absolute offset, consistent with
    /// the encoder's `FrameEmitInfo.blocks[index].offset_in_frame`.
    #[cfg(feature = "lsm")]
    #[test]
    fn block_precise_errors_carry_index_and_offset() {
        use crate::encoding::{CompressionLevel, FrameCompressor};
        // ~1.3 MiB of incompressible (xorshift) bytes → many 128 KiB raw
        // blocks, so blocks 3 and 7 both exist and are not the last block.
        let mut data = alloc::vec::Vec::with_capacity(1_300_000);
        let mut s: u64 = 0x2545_F491_4F6C_DD1D;
        while data.len() < 1_300_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            data.push((s >> 33) as u8);
        }

        let mut frame = alloc::vec::Vec::new();
        let blocks = {
            let mut fc = FrameCompressor::new(CompressionLevel::Level(1));
            fc.set_source(data.as_slice());
            fc.set_drain(&mut frame);
            fc.compress();
            fc.last_frame_emit_info()
                .expect("emit info present under lsm")
                .blocks
                .clone()
        };
        assert!(blocks.len() > 7, "need >7 blocks, got {}", blocks.len());

        let mut out = alloc::vec![0u8; data.len() + 4096];

        // (1) Corrupt block 7's header: force its Block_Type to Reserved (3)
        // by setting both type bits — fails the header read at block 7.
        let off7 = blocks[7].offset_in_frame as usize;
        let mut corrupt = frame.clone();
        corrupt[off7] |= 0b0000_0110;
        let mut dec = FrameDecoder::new();
        let err = dec
            .decode_all(&corrupt, &mut out)
            .expect_err("reserved block-7 header must fail");
        match err {
            super::FrameDecoderError::FailedToReadBlockHeaderAt {
                block_index,
                frame_offset,
                ..
            } => {
                assert_eq!(block_index, 7);
                assert_eq!(frame_offset, blocks[7].offset_in_frame);
            }
            other => panic!("expected FailedToReadBlockHeaderAt, got {other:?}"),
        }

        // (2) Truncate at block 3's body start: header intact, body missing
        // → the body decode fails at block 3 with its FrameBlock metadata.
        let body3 = blocks[3].offset_in_frame as usize + blocks[3].header_size as usize;
        let mut dec = FrameDecoder::new();
        let err = dec
            .decode_all(&frame[..body3], &mut out)
            .expect_err("truncated block-3 body must fail");
        match err {
            super::FrameDecoderError::FailedToReadBlockBodyAt {
                block_index,
                frame_offset,
                block,
                ..
            } => {
                assert_eq!(block_index, 3);
                assert_eq!(frame_offset, blocks[3].offset_in_frame);
                assert_eq!(block.offset_in_frame, blocks[3].offset_in_frame);
            }
            other => panic!("expected FailedToReadBlockBodyAt, got {other:?}"),
        }
    }

    #[test]
    fn decode_all_exact_fit_output_decodes_correctly() {
        // Output sized exactly to frame_content_size (no
        // WILDCOPY_OVERLENGTH slack) is now eligible for the direct
        // path: every output-write site is exact-fit-safe (sequence
        // exec falls back to the bounded, non-overshooting copy on the
        // trailing sequence(s), Raw/RLE blocks copy exactly). This must
        // produce the same bytes as a slack-padded buffer. Exercised on
        // x86 through the per-kernel AVX2/SSE2 inline-exec macros, which
        // carry the same tight-tail branch.
        let payload: Vec<u8> = (0..2048u32)
            .map(|i| (i.wrapping_mul(31) & 0xFF) as u8)
            .collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let mut dec = FrameDecoder::new();
        // Exactly payload.len(), no slack.
        let mut out = alloc::vec![0u8; payload.len()];
        let n = dec
            .decode_all(compressed.as_slice(), &mut out)
            .expect("exact-fit decode_all should succeed");
        assert_eq!(n, payload.len());
        assert_eq!(&out[..n], payload.as_slice());
    }

    #[test]
    fn decode_all_fallback_validates_fcs_against_total_output() {
        // Synthetic single-segment frame: FCS = 20 bytes, but the
        // last-block flag fires after only 4 bytes of raw payload.
        // On the direct path this would trip the post-block
        // `produced > content_size` check; the fallback path
        // (eligible=false because output is sized exactly to FCS,
        // no WILDCOPY slack) used to silently return Ok(4). With
        // the fix it now surfaces `FrameContentSizeMismatch`
        // matching the direct path.
        //
        // Frame layout: 4 B magic | 1 B FHD (single_segment=1,
        // FCS_flag=3 → 8-byte FCS) | 8 B FCS=20 | block header
        // (Raw, last, size=4) | 4 raw bytes.
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes()); // magic
        // FHD: FCS_flag=3 (8-byte FCS) <<6 | single_segment=1 <<5.
        wire.push(0b1110_0000);
        wire.extend_from_slice(&20u64.to_le_bytes()); // declared FCS
        // Block header: (size << 3) | (block_type << 1) | last_block.
        // Raw block (block_type=0), last_block=1, size=4 → 0b00100001 = 0x21.
        wire.push(0x21);
        wire.push(0x00);
        wire.push(0x00);
        wire.extend_from_slice(&[1u8, 2, 3, 4]);

        let mut dec = FrameDecoder::new();
        // Size output SMALLER than the declared FCS so direct-decode is
        // gated out (`output.len() >= content_size` is false) and the
        // frame takes the legacy fallback drain loop — the path this test
        // guards. The corrupt frame only produces 4 bytes, so 19 is ample
        // room; the point is `19 != declared FCS (20)`.
        const DECLARED_FCS: usize = 20;
        let mut out = alloc::vec![0u8; DECLARED_FCS - 1];
        assert_ne!(
            out.len(),
            DECLARED_FCS,
            "output must be smaller than FCS to exercise the fallback path",
        );
        let err = dec
            .decode_all(wire.as_slice(), &mut out)
            .expect_err("fallback must reject corrupt FCS underflow");
        match err {
            crate::decoding::errors::FrameDecoderError::FrameContentSizeMismatch {
                declared,
                produced,
            } => {
                assert_eq!(declared, 20);
                assert_eq!(produced, 4);
            }
            other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_all_fallback_treats_explicit_fcs_zero_as_declared() {
        // Synthetic multi-segment frame with FCS_flag=2 (4-byte
        // FCS) explicitly set to 0. The header DECLARES zero
        // content, but the body carries a 5-byte raw last-block.
        // `fcs_declared()` must return true (the field is on the
        // wire) so the fallback's post-decode size check sees the
        // mismatch — even though `frame_content_size == 0`. This
        // is exactly the FCS=0 edge case where the previous
        // `content_size > 0` proxy would have silently accepted
        // the corrupt frame.
        //
        // Frame layout:
        //   4 B magic            — 28 B5 2F FD
        //   1 B FHD              — FCS_flag=2 (bits 7-6), no
        //                          single_segment, content_checksum=0,
        //                          dict_id_flag=0 → 0b1000_0000
        //   1 B window_descriptor — exp=10, mantissa=0 → window=1 MiB
        //   4 B FCS              — 0 LE
        //   3 B block header     — raw, last, size=5 → 0x29 0x00 0x00
        //   5 B raw payload      — anything non-empty
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        wire.push(0b1000_0000); // FHD: FCS_flag=2, others 0.
        wire.push(0x50); // window_descriptor: exp=10, mantissa=0.
        wire.extend_from_slice(&0u32.to_le_bytes()); // FCS = 0.
        // Block header (24-bit LE): (size << 3) | (block_type << 1) | last_block
        // = (5 << 3) | (0 << 1) | 1 = 0x29.
        wire.push(0x29);
        wire.push(0x00);
        wire.push(0x00);
        wire.extend_from_slice(&[1u8, 2, 3, 4, 5]);

        let mut dec = FrameDecoder::new();
        // FCS=0 declared, so eligibility (`content_size > 0`)
        // false — falls through to the drain loop. Output buffer
        // size doesn't matter for the eligibility check here;
        // give it some room so `read()` can drain the block.
        let mut out = alloc::vec![0u8; 16];
        let err = dec
            .decode_all(wire.as_slice(), &mut out)
            .expect_err("corrupt FCS=0 + 5-byte block must error");
        match err {
            crate::decoding::errors::FrameDecoderError::FrameContentSizeMismatch {
                declared,
                produced,
            } => {
                assert_eq!(declared, 0);
                assert_eq!(produced, 5);
            }
            other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_all_fallback_accepts_honest_explicit_fcs_zero() {
        // Companion to the corrupt-FCS=0 test above: an HONEST
        // empty frame with FCS_flag=2 (4-byte FCS) explicitly set
        // to 0 AND a 0-byte raw last-block. `fcs_declared()`
        // returns true and `content_size == 0 == total_written`,
        // so the fallback validation accepts the frame instead of
        // misreporting a mismatch.
        //
        // (Single-segment FCS=0 would test a similar invariant
        // but trips header-stage validation: `window_size =
        // frame_content_size = 0 < MIN_WINDOW_SIZE` fails the
        // window-size sanity check before decode runs. Use the
        // multi-segment shape where `window_size` comes from
        // `window_descriptor` independently of FCS.)
        //
        // Frame layout:
        //   4 B magic
        //   1 B FHD              — FCS_flag=2, others 0 → 0x80
        //   1 B window_descriptor — exp=10 → 1 MiB window
        //   4 B FCS              — 0 LE
        //   3 B block header     — raw, last, size=0 → 0x01 0x00 0x00
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        wire.push(0b1000_0000);
        wire.push(0x50);
        wire.extend_from_slice(&0u32.to_le_bytes());
        // Block header: (0 << 3) | (0 << 1) | 1 = 0x01.
        wire.push(0x01);
        wire.push(0x00);
        wire.push(0x00);

        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; 16];
        let n = dec
            .decode_all(wire.as_slice(), &mut out)
            .expect("honest FCS=0 + empty block must succeed");
        assert_eq!(n, 0);
    }

    #[test]
    fn reset_with_dict_handle_applies_dict_when_no_dict_id() {
        let payload = b"reset-without-dict-id";
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let handle = DictionaryHandle::decode_dict(dict_raw).expect("dictionary should parse");

        let mut decoder = FrameDecoder::new();
        decoder
            .reset_with_dict_handle(compressed.as_slice(), &handle)
            .expect("reset should succeed");
        let state = decoder.state.as_ref().expect("state should be initialized");
        assert!(state.frame_header.dictionary_id().is_none());
        assert_eq!(state.using_dict, Some(handle.id()));
    }

    #[cfg(feature = "lsm")]
    mod expect_validation {
        use super::*;
        use crate::decoding::errors::FrameDecoderError;

        fn compress(payload: &[u8]) -> Vec<u8> {
            let mut compressor = FrameCompressor::new(CompressionLevel::Default);
            compressor.set_source(payload);
            let mut compressed = Vec::new();
            compressor.set_drain(&mut compressed);
            compressor.compress();
            compressed
        }

        fn compress_with_dict(payload: &[u8], dict_raw: &[u8]) -> Vec<u8> {
            let mut compressor = FrameCompressor::new(CompressionLevel::Default);
            compressor
                .set_dictionary_from_bytes(dict_raw)
                .expect("dict load");
            compressor.set_source(payload);
            let mut compressed = Vec::new();
            compressor.set_drain(&mut compressed);
            compressor.compress();
            compressed
        }

        #[test]
        fn expect_dict_id_none_default_allows_anything() {
            let compressed = compress(b"hello-no-expect");
            let mut decoder = FrameDecoder::new();
            decoder
                .reset(compressed.as_slice())
                .expect("default None passes");
        }

        #[test]
        fn expect_dict_id_zero_matches_frame_without_dict_id() {
            // Default-encoded frame has no dict_id; pinning Some(0)
            // ("no dictionary expected") must accept it.
            let compressed = compress(b"payload");
            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(0));
            decoder
                .reset(compressed.as_slice())
                .expect("Some(0) ~ None");
        }

        #[test]
        fn expect_dict_id_matching_value_passes() {
            let dict_raw = include_bytes!("../../dict_tests/dictionary");
            let handle = DictionaryHandle::decode_dict(dict_raw).expect("dict parse");
            let actual_id = handle.id();

            let compressed = compress_with_dict(b"payload-with-dict", dict_raw);

            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(actual_id));
            // Decode requires the dict to be registered; using
            // reset_with_dict_handle for that.
            decoder
                .reset_with_dict_handle(compressed.as_slice(), &handle)
                .expect("matching dict_id passes");
        }

        #[test]
        fn expect_dict_id_mismatching_value_fails_before_decode() {
            let dict_raw = include_bytes!("../../dict_tests/dictionary");
            let handle = DictionaryHandle::decode_dict(dict_raw).expect("dict parse");
            let actual_id = handle.id();
            let wrong_id = actual_id.wrapping_add(1);

            let compressed = compress_with_dict(b"payload-with-dict", dict_raw);

            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(wrong_id));
            let err = decoder
                .reset_with_dict_handle(compressed.as_slice(), &handle)
                .expect_err("mismatch must fail");
            match err {
                FrameDecoderError::UnexpectedDictId { expected, found } => {
                    assert_eq!(expected, Some(wrong_id));
                    assert_eq!(found, Some(actual_id));
                }
                other => panic!("expected UnexpectedDictId, got {other:?}"),
            }
        }

        #[test]
        fn expect_dict_id_nonzero_fails_on_frame_without_dict_id() {
            // Frame has no dict_id; expecting Some(42) (non-zero)
            // must fail with found = None.
            let compressed = compress(b"no-dict-frame");
            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(42));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("nonzero expectation on dictless frame must fail");
            match err {
                FrameDecoderError::UnexpectedDictId { expected, found } => {
                    assert_eq!(expected, Some(42));
                    assert_eq!(found, None);
                }
                other => panic!("expected UnexpectedDictId, got {other:?}"),
            }
        }

        #[test]
        fn expect_window_descriptor_none_default_allows_anything() {
            let compressed = compress(b"hello-no-wd-expect");
            let mut decoder = FrameDecoder::new();
            decoder
                .reset(compressed.as_slice())
                .expect("default None passes");
        }

        #[test]
        fn expect_window_descriptor_mismatch_fails_before_decode() {
            // Compress a payload large enough to force a
            // multi-segment frame (window_descriptor on wire).
            // Default compression at >256 KiB produces multi-
            // segment frames with a real window_descriptor byte.
            let payload = alloc::vec![0xABu8; 512 * 1024];
            let compressed = compress(&payload);

            // Read the actual window_descriptor by decoding once
            // without expectations, then pin a wrong value.
            let mut probe_decoder = FrameDecoder::new();
            probe_decoder.reset(compressed.as_slice()).unwrap();
            let probe_state = probe_decoder.state.as_ref().unwrap();
            let actual_wd = probe_state
                .frame_header
                .window_descriptor()
                .expect("multi-segment frame should expose window_descriptor");
            let wrong_wd = actual_wd.wrapping_add(0x10); // bump exponent

            let mut decoder = FrameDecoder::new();
            decoder.expect_window_descriptor(Some(wrong_wd));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("wrong window_descriptor must fail");
            match err {
                FrameDecoderError::UnexpectedWindowDescriptor { expected, found } => {
                    assert_eq!(expected, wrong_wd);
                    assert_eq!(found, Some(actual_wd));
                }
                other => panic!("expected UnexpectedWindowDescriptor, got {other:?}"),
            }
        }

        /// Build a minimal synthetic single-segment zstd frame
        /// carrying a 4-byte raw payload. RFC 8878 §3.1.1.1
        /// layout, hand-rolled because our default
        /// `FrameCompressor` settings don't emit
        /// `single_segment_flag` for tiny inputs.
        ///
        /// Wire bytes (13 total for 4-byte payload):
        /// ```text
        /// 28 B5 2F FD       magic
        /// 20                FHD: single_segment=1, FCS_flag=0
        /// 04                FCS (single byte, value = payload.len())
        /// 21 00 00          block header: raw, last, size=4
        /// .. .. .. ..       payload bytes
        /// ```
        fn synth_single_segment_frame(payload: &[u8]) -> Vec<u8> {
            assert!(payload.len() <= 255, "1-byte FCS field caps at 255");
            assert!(payload.len() < (1usize << 21), "block size 21-bit max");
            let mut out = Vec::new();
            // Magic 0xFD2FB528 LE.
            out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
            // FHD: single_segment_flag (bit 5) set, everything
            // else zero. With single_segment + FCS_flag=0 the FCS
            // field is 1 byte. No window_descriptor on wire.
            out.push(0b0010_0000);
            // 1-byte FCS = payload length.
            out.push(payload.len() as u8);
            // Block header (3 bytes LE):
            // last_block=1, block_type=0 (Raw), block_size=payload.len().
            // Encoded: (size << 3) | (block_type << 1) | last_block.
            // Block header: last_block flag in bit 0, block_type
            // (0 = Raw) in bits 1-2, block size in bits 3+.
            let bh: u32 = ((payload.len() as u32) << 3) | 1;
            out.push((bh & 0xFF) as u8);
            out.push(((bh >> 8) & 0xFF) as u8);
            out.push(((bh >> 16) & 0xFF) as u8);
            // Raw payload.
            out.extend_from_slice(payload);
            out
        }

        #[test]
        fn expect_window_descriptor_on_single_segment_frame_fails_with_found_none() {
            // Single-segment frames omit the window_descriptor
            // byte from the wire entirely. Setting an expectation
            // here must surface `found: None` so callers
            // distinguish "wrong descriptor" from "no descriptor
            // on the wire" — never silently pass.
            let compressed = synth_single_segment_frame(b"tiny");

            // First sanity-check: the synthetic frame decodes
            // cleanly without any expectation.
            {
                let mut probe = FrameDecoder::new();
                probe
                    .reset(compressed.as_slice())
                    .expect("synth frame parses");
                let probe_state = probe.state.as_ref().unwrap();
                assert!(
                    probe_state.frame_header.window_descriptor().is_none(),
                    "synth frame must be single-segment"
                );
            }

            let mut decoder = FrameDecoder::new();
            decoder.expect_window_descriptor(Some(0x40));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("single-segment + expectation must fail");
            match err {
                FrameDecoderError::UnexpectedWindowDescriptor { expected, found } => {
                    assert_eq!(expected, 0x40);
                    assert_eq!(found, None);
                }
                other => panic!("expected UnexpectedWindowDescriptor, got {other:?}"),
            }
        }

        #[test]
        fn validation_failure_leaves_decoder_re_resettable() {
            // After UnexpectedDictId on a wrong-expectation reset,
            // clearing the expectation and re-calling reset must
            // succeed on the same source — no lingering failed
            // state.
            let compressed = compress(b"re-resettable");

            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(42));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("first reset fails");
            assert!(matches!(err, FrameDecoderError::UnexpectedDictId { .. }));

            // Clear expectation and retry on a fresh source.
            decoder.expect_dict_id(None);
            decoder
                .reset(compressed.as_slice())
                .expect("retry after clearing expectation should succeed");
        }
    }

    /// Build a skippable frame on the wire: 4-byte LE magic + 4-byte LE
    /// length + payload bytes. RFC 8878 §3.1.2 restricts the magic
    /// variant to `0..=15`; assert here so accidental misuse of the
    /// helper can't smuggle a non-skippable magic past the tests.
    #[cfg(feature = "lsm")]
    fn build_skippable_frame(variant: u8, payload: &[u8]) -> Vec<u8> {
        assert!(
            variant <= 15,
            "skippable-frame variant {variant} outside RFC 8878 0..=15 range",
        );
        let mut out = Vec::with_capacity(8 + payload.len());
        let magic: u32 = 0x184D2A50 + u32::from(variant);
        out.extend_from_slice(&magic.to_le_bytes());
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_all_with_skippable_visitor_sees_payloads_in_order() {
        // Build a stream: skippable(v0, "alpha") + zstd_frame +
        // skippable(v3, "beta") + zstd_frame + skippable(v15, "")
        // and verify the visitor is invoked exactly three times with
        // the correct (variant, payload) pairs in stream order while
        // the zstd frames decode normally.
        let payload_a: Vec<u8> = (0..256u16).map(|i| i as u8).collect();
        let payload_b: Vec<u8> = (0..256u16).map(|i| (i ^ 0xAA) as u8).collect();

        let mut comp_a = Vec::new();
        let mut c = FrameCompressor::new(CompressionLevel::Default);
        c.set_source(payload_a.as_slice());
        c.set_drain(&mut comp_a);
        c.compress();

        let mut comp_b = Vec::new();
        let mut c = FrameCompressor::new(CompressionLevel::Default);
        c.set_source(payload_b.as_slice());
        c.set_drain(&mut comp_b);
        c.compress();

        let skip0 = build_skippable_frame(0, b"alpha");
        let skip3 = build_skippable_frame(3, b"beta");
        let skip15 = build_skippable_frame(15, &[]);

        let mut stream = Vec::new();
        stream.extend_from_slice(&skip0);
        stream.extend_from_slice(&comp_a);
        stream.extend_from_slice(&skip3);
        stream.extend_from_slice(&comp_b);
        stream.extend_from_slice(&skip15);

        let mut decoder = FrameDecoder::new();
        let mut out = alloc::vec![0u8; payload_a.len() + payload_b.len()];
        let mut collected: Vec<(u8, Vec<u8>)> = Vec::new();
        let n = decoder
            .decode_all_with_skippable_visitor(stream.as_slice(), &mut out, |variant, payload| {
                collected.push((variant, payload.to_vec()));
            })
            .expect("decode_all_with_skippable_visitor should succeed");

        // All three skippables visited in stream order.
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0], (0u8, b"alpha".to_vec()));
        assert_eq!(collected[1], (3u8, b"beta".to_vec()));
        assert_eq!(collected[2], (15u8, Vec::<u8>::new()));

        // Both zstd frames decoded into `out` back-to-back.
        assert_eq!(n, payload_a.len() + payload_b.len());
        assert_eq!(&out[..payload_a.len()], payload_a.as_slice());
        assert_eq!(&out[payload_a.len()..n], payload_b.as_slice());
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_all_silently_skips_when_no_visitor() {
        // Regression gate: plain decode_all must still silently skip
        // skippable frames (RFC 8878 mandated behavior) with no
        // behavioral change after the visitor refactor.
        let payload: Vec<u8> = (0..512u16).map(|i| i as u8).collect();
        let mut comp = Vec::new();
        let mut c = FrameCompressor::new(CompressionLevel::Default);
        c.set_source(payload.as_slice());
        c.set_drain(&mut comp);
        c.compress();

        let skip = build_skippable_frame(7, b"ignored sidecar");
        let mut stream = Vec::new();
        stream.extend_from_slice(&skip);
        stream.extend_from_slice(&comp);

        let mut decoder = FrameDecoder::new();
        let mut out = alloc::vec![0u8; payload.len()];
        let n = decoder
            .decode_all(stream.as_slice(), &mut out)
            .expect("decode_all should succeed on skippable + zstd stream");
        assert_eq!(n, payload.len());
        assert_eq!(&out[..n], payload.as_slice());
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn frame_emit_info_describes_emitted_block_layout() {
        // Encode a payload large enough to force >1 block, fetch
        // FrameEmitInfo, walk blocks[] and verify each block's
        // (offset_in_frame, header_size, body_size) matches the bytes
        // actually emitted into the drain buffer.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let info = compressor
            .last_frame_emit_info()
            .expect("last_frame_emit_info populated after compress")
            .clone();
        drop(compressor);

        // Frame header range starts at 0 and is non-empty.
        assert_eq!(info.frame_header_range.start, 0);
        assert!(info.frame_header_range.end > 0);
        // Total size matches what was written to the drain.
        assert_eq!(info.total_size as usize, compressed.len());
        // At least one block, and the last entry has last_block=true.
        assert!(!info.blocks.is_empty());
        assert!(info.blocks.last().unwrap().last_block);
        // All non-final blocks have last_block=false.
        for b in &info.blocks[..info.blocks.len() - 1] {
            assert!(!b.last_block);
        }
        // Walk and verify each block's header bytes match the
        // recorded type / size by re-decoding the 3-byte header.
        // Walking arithmetic: offset_in_frame + header_size + body_size
        // must land exactly on the next block's offset_in_frame (or,
        // for the last block, on the checksum / end of frame).
        for (i, b) in info.blocks.iter().enumerate() {
            let off = b.offset_in_frame as usize;
            assert_eq!(b.header_size, 3);
            let mut hdr = [0u8; 4];
            hdr[..3].copy_from_slice(&compressed[off..off + 3]);
            let raw = u32::from_le_bytes(hdr);
            let last = (raw & 1) != 0;
            let ty = (raw >> 1) & 0b11;
            let sz = raw >> 3;
            assert_eq!(last, b.last_block);
            assert_eq!(sz, b.block_size_field);
            // body_size is the PHYSICAL length on the wire: spec's
            // Block_Size for Raw/Compressed, always 1 for RLE.
            let expected_physical = match b.block_type {
                crate::encoding::frame_emit_info::BlockType::RLE => 1,
                _ => sz,
            };
            assert_eq!(b.body_size, expected_physical);
            let expected_ty = match b.block_type {
                crate::encoding::frame_emit_info::BlockType::Raw => 0,
                crate::encoding::frame_emit_info::BlockType::RLE => 1,
                crate::encoding::frame_emit_info::BlockType::Compressed => 2,
                crate::encoding::frame_emit_info::BlockType::Reserved => 3,
            };
            assert_eq!(ty, expected_ty);
            // Walking-arithmetic invariant.
            let next_off = b.offset_in_frame + b.header_size as u32 + b.body_size;
            if let Some(next) = info.blocks.get(i + 1) {
                assert_eq!(
                    next_off, next.offset_in_frame,
                    "block {i} body_size doesn't reach next block's offset_in_frame",
                );
            } else if let Some(cs) = info.checksum_range.as_ref() {
                assert_eq!(
                    next_off, cs.start,
                    "last block body_size doesn't reach checksum_range.start",
                );
            } else {
                assert_eq!(
                    next_off, info.total_size,
                    "last block body_size doesn't reach total_size",
                );
            }
        }
        // Checksum range present iff `feature = "hash"` is enabled.
        assert_eq!(info.checksum_range.is_some(), cfg!(feature = "hash"));
    }

    #[cfg(all(feature = "lsm", feature = "hash"))]
    #[test]
    fn per_block_checksum_round_trip() {
        // Encode with per-block checksums enabled. Decode with
        // per-block verification. Both sides emit exactly 1
        // checksum per physical block written to / read from the
        // wire (encoder hashes per emission site, including each
        // post-split partition; decoder hashes each decoded block).
        // Cardinality and element-wise contents must match
        // round-trip.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        compressor.enable_per_block_checksums();
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let encoder_checksums = compressor
            .last_frame_block_checksums()
            .expect("checksums populated after enable + compress")
            .to_vec();
        drop(compressor);
        assert!(!encoder_checksums.is_empty());

        // Decode side: enable verification, decode, compare.
        let mut decoder = FrameDecoder::new();
        decoder.enable_per_block_checksums();
        let mut output = alloc::vec![0u8; payload.len()];
        let n = decoder
            .decode_all(compressed.as_slice(), &mut output)
            .expect("decode_all should succeed");
        assert_eq!(n, payload.len());
        assert_eq!(&output[..n], payload.as_slice());

        let decoder_checksums = decoder.computed_block_checksums();
        assert_eq!(decoder_checksums, encoder_checksums.as_slice());
    }

    // ── decode_blocks_partial (block-subset partial decode, lsm) ──

    /// Build a multi-block compressible frame and return
    /// `(compressed, full_decode, emit_info)`. The emit info's
    /// `decompressed_byte_range` maps decompressed offsets to block indices.
    #[cfg(feature = "lsm")]
    fn multi_block_fixture() -> (
        Vec<u8>,
        Vec<u8>,
        crate::encoding::frame_emit_info::FrameEmitInfo,
    ) {
        let mut data: Vec<u8> = Vec::with_capacity(400 * 1024);
        let mut x = 0x9E37_79B9u32;
        while data.len() < 400 * 1024 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let run = 16 + (x as usize % 48);
            let byte = (x >> 24) as u8;
            for _ in 0..run {
                data.push(byte);
            }
            data.extend_from_slice(b"the quick brown fox jumps over the lazy dog\n");
        }

        let mut compressed = Vec::new();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed);
        compressor.compress();
        let info = compressor
            .last_frame_emit_info()
            .expect("emit info populated")
            .clone();
        drop(compressor);

        let mut dec = FrameDecoder::new();
        let mut full = alloc::vec![0u8; data.len()];
        let n = dec
            .decode_all(compressed.as_slice(), &mut full)
            .expect("full decode");
        full.truncate(n);
        assert_eq!(full, data, "fixture must round-trip");
        (compressed, full, info)
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_blocks_partial_subset_matches_full_decode() {
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        assert!(
            nblocks >= 4,
            "fixture must have several blocks, got {nblocks}"
        );
        let half = nblocks / 2;
        // Boundaries: 1 block, 2 blocks, half, all, and a non-zero start.
        // `(0, u32::MAX)` exercises the "decode to end of frame" sentinel,
        // a distinct public contract from an explicit upper bound.
        for &(s, e) in &[
            (0u32, u32::MAX),
            (0, 1),
            (0, 2),
            (0, half),
            (0, nblocks),
            (1, 2),
            (half, nblocks),
        ] {
            // The sentinel decodes through the last block; map it to nblocks
            // for the expected-slice / block-count arithmetic below.
            let effective_end = if e == u32::MAX { nblocks } else { e };
            let mut source = compressed.as_slice();
            let mut dec = FrameDecoder::new();
            dec.reset(&mut source).unwrap();
            let pd = dec
                .decode_blocks_partial(&mut source, s, e, None, false)
                .unwrap_or_else(|err| panic!("range [{s},{e}) errored: {err:?}"));

            let start = info.decompressed_byte_range(s as usize).unwrap().start as usize;
            let end = info
                .decompressed_byte_range((effective_end - 1) as usize)
                .unwrap()
                .end as usize;
            assert_eq!(
                pd.data.as_slice(),
                &full[start..end],
                "subset bytes must equal the full-decode slice for [{s},{e})"
            );
            assert_eq!(pd.start_block, s);
            assert_eq!(pd.blocks_decoded, effective_end - s);
            assert!(pd.stopped_at.is_none(), "clean range [{s},{e})");
        }
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_blocks_partial_recovers_clean_prefix_on_truncated_block() {
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len();
        let k = nblocks / 2;
        assert!(k >= 1, "need a clean prefix before the failing block");

        // Truncate the source right after block k's 3-byte header, so its body
        // read fails regardless of block type (0 body bytes available).
        let cut = info.blocks[k].offset_in_frame as usize + info.blocks[k].header_size as usize;
        let truncated = &compressed[..cut];

        let mut source = truncated;
        let mut dec = FrameDecoder::new();
        dec.reset(&mut source).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut source, 0, u32::MAX, None, false)
            .unwrap();

        let (idx, _err) = pd.stopped_at.expect("must stop on the truncated block");
        assert_eq!(idx, k as u32, "stopped at the truncated block index");
        assert_eq!(pd.blocks_decoded, k as u32, "blocks 0..k decoded cleanly");
        assert!(!pd.frame_finished);
        let clean_end = info.decompressed_byte_range(k).unwrap().start as usize;
        assert_eq!(
            pd.data.as_slice(),
            &full[..clean_end],
            "clean prefix preserved through the failure"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_blocks_partial_invalid_range_errors() {
        let (compressed, _full, _info) = multi_block_fixture();
        let mut source = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut source).unwrap();
        let err = dec
            .decode_blocks_partial(&mut source, 5, 2, None, false)
            .expect_err("start > end must error");
        assert!(matches!(
            err,
            crate::decoding::errors::FrameDecoderError::InvalidBlockRange {
                start_block: 5,
                end_block: 2,
            }
        ));
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_blocks_partial_skips_trailing_blocks() {
        let (compressed, full, info) = multi_block_fixture();
        assert!(info.blocks.len() >= 3);
        let mut source = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut source).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut source, 0, 1, None, false)
            .unwrap();

        assert_eq!(pd.blocks_decoded, 1);
        assert!(pd.stopped_at.is_none());
        assert!(!pd.frame_finished, "block 0 is not the last block");
        let end = info.decompressed_byte_range(0).unwrap().end as usize;
        assert_eq!(pd.data.as_slice(), &full[..end]);
        // The trailing blocks + checksum were never consumed from the source.
        assert!(
            dec.bytes_read_from_source() < u64::from(info.total_size),
            "only block 0's region should be consumed, read {} of {}",
            dec.bytes_read_from_source(),
            info.total_size
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn lsm_style_range_query_partial_recovery() {
        // Simulates lsm-tree's range-query path: a key range resolves to a
        // decompressed byte window, which maps to inner zstd block indices via
        // `decompressed_byte_range`; decode only the covering blocks and check
        // the wanted window is recovered exactly (no key outside, all inside).
        let (compressed, full, info) = multi_block_fixture();
        let total = full.len() as u64;
        let want_start = total / 3;
        let want_end = (total * 2) / 3;

        // Map [want_start, want_end) to covering block indices.
        let nblocks = info.blocks.len();
        let mut start_block = 0u32;
        let mut end_block = nblocks as u32;
        for i in 0..nblocks {
            let r = info.decompressed_byte_range(i).unwrap();
            if r.start <= want_start && want_start < r.end {
                start_block = i as u32;
            }
            if r.start < want_end && want_end <= r.end {
                end_block = i as u32 + 1;
                break;
            }
        }

        let mut source = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut source).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut source, start_block, end_block, None, false)
            .unwrap();
        assert!(pd.stopped_at.is_none());

        let covered_start = info
            .decompressed_byte_range(start_block as usize)
            .unwrap()
            .start;
        let covered_end = info
            .decompressed_byte_range((end_block - 1) as usize)
            .unwrap()
            .end;
        assert!(
            covered_start <= want_start && want_end <= covered_end,
            "covering blocks must contain the wanted window"
        );
        assert_eq!(
            pd.data.as_slice(),
            &full[covered_start as usize..covered_end as usize],
            "covered subset must equal the full-decode slice"
        );
        // Slice the exact key range out of the covered subset.
        let off = (want_start - covered_start) as usize;
        let len = (want_end - want_start) as usize;
        assert_eq!(
            &pd.data[off..off + len],
            &full[want_start as usize..want_end as usize],
            "exact key range recovered from the partial decode"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_blocks_partial_leaves_no_residual_when_no_in_range_block() {
        // Regression: when the requested range reaches no in-range block (here
        // start_block is past EOF, so every block is decoded only as window
        // context), `PartialDecode::data` is empty — but the context bytes must
        // NOT linger in the decoder buffer, or a later collect()/read() on the
        // same decoder returns out-of-range data.
        let (compressed, _full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let mut source = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut source).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut source, nblocks + 5, u32::MAX, None, false)
            .unwrap();
        assert!(pd.data.is_empty(), "no in-range block → empty data");
        assert_eq!(pd.blocks_decoded, 0);
        assert!(
            pd.frame_finished,
            "frame's last block was reached as context"
        );
        assert_eq!(
            dec.can_collect(),
            0,
            "context bytes must not leak via collect()/read() when data is empty"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn decode_blocks_partial_empty_range_leaves_no_residual() {
        // Companion to the start-past-EOF case: an in-frame empty range `[k, k)`
        // (k < EOF) takes the same `prefix_window_len == None` path but with
        // `frame_finished == false` and up to `window_size` context bytes still
        // physically present. Assert the buffer is fully cleared directly (a
        // `can_collect()` check alone would pass even with <= window_size bytes
        // retained, because it holds the window back).
        let (compressed, _full, info) = multi_block_fixture();
        let k = ((info.blocks.len() as u32) / 2).max(1);
        let mut source = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut source).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut source, k, k, None, false)
            .unwrap();

        assert!(pd.data.is_empty(), "empty range must yield empty data");
        assert_eq!(pd.blocks_decoded, 0);
        assert!(
            !pd.frame_finished,
            "frame should still have trailing blocks"
        );
        assert_eq!(
            dec.state.as_ref().unwrap().decoder_scratch.buffer_len(),
            0,
            "empty-range partial decode must not retain context bytes"
        );
    }

    #[cfg(all(feature = "lsm", feature = "hash"))]
    #[test]
    fn decode_blocks_partial_captures_per_block_checksums() {
        // Regression: with per-block checksums enabled, decode_blocks_partial
        // must populate computed_block_checksums just like decode_blocks /
        // decode_all — otherwise callers verifying per-block digests silently
        // lose them on the partial path.
        let (compressed, full, _info) = multi_block_fixture();

        // Reference digests via decode_blocks (the path that captures them).
        let mut ref_dec = FrameDecoder::new();
        ref_dec.enable_per_block_checksums();
        let mut rsrc = compressed.as_slice();
        ref_dec.reset(&mut rsrc).unwrap();
        while !ref_dec.is_finished() {
            ref_dec
                .decode_blocks(&mut rsrc, crate::decoding::BlockDecodingStrategy::All)
                .unwrap();
        }
        let expected = ref_dec.computed_block_checksums().to_vec();
        assert!(!expected.is_empty(), "fixture must have multiple blocks");
        let _ = full;

        // Partial decode of the whole frame must capture the same digests.
        let mut source = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.enable_per_block_checksums();
        dec.reset(&mut source).unwrap();
        let _ = dec
            .decode_blocks_partial(&mut source, 0, u32::MAX, None, false)
            .unwrap();
        assert_eq!(
            dec.computed_block_checksums(),
            expected.as_slice(),
            "partial decode must capture the same per-block checksums as full decode"
        );
    }

    // ── resume (window-priming + entropy cold resume, lsm) ───────────

    /// Window size of `compressed`'s frame, read from a freshly-reset decoder.
    #[cfg(feature = "lsm")]
    fn frame_window_size(compressed: &[u8]) -> usize {
        let mut src = compressed;
        let mut dec = FrameDecoder::new();
        dec.reset(&mut src).unwrap();
        dec.state
            .as_ref()
            .unwrap()
            .frame_header
            .window_size()
            .unwrap_or(0) as usize
    }

    /// Build a large compressible MULTI-SEGMENT frame (window_size < content,
    /// so mid-frame blocks reach back only into a bounded window) and return
    /// `(compressed, full_decode, emit_info)`.
    #[cfg(feature = "lsm")]
    fn multi_segment_block_fixture() -> (
        Vec<u8>,
        Vec<u8>,
        crate::encoding::frame_emit_info::FrameEmitInfo,
    ) {
        // ~3 MiB of compressible (runs + repeated phrase) data — large enough
        // that the encoder picks window_size < content_size (multi-segment).
        let mut data: Vec<u8> = Vec::with_capacity(3 * 1024 * 1024);
        let mut x = 0x9E37_79B9u32;
        while data.len() < 3 * 1024 * 1024 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let run = 16 + (x as usize % 48);
            let byte = (x >> 24) as u8;
            for _ in 0..run {
                data.push(byte);
            }
            data.extend_from_slice(b"the quick brown fox jumps over the lazy dog\n");
        }

        let mut compressed = Vec::new();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed);
        compressor.compress();
        let info = compressor
            .last_frame_emit_info()
            .expect("emit info populated")
            .clone();
        drop(compressor);

        // Confirm the precondition: the frame must be multi-segment.
        let mut sanity = FrameDecoder::new();
        sanity.init(&mut compressed.as_slice()).unwrap();
        assert!(
            !sanity
                .state
                .as_ref()
                .unwrap()
                .frame_header
                .descriptor
                .single_segment_flag(),
            "fixture precondition: frame must be multi-segment (resize if encoder default changed)"
        );

        let mut dec = FrameDecoder::new();
        let mut full = alloc::vec![0u8; data.len()];
        let n = dec
            .decode_all(compressed.as_slice(), &mut full)
            .expect("full decode");
        full.truncate(n);
        assert_eq!(full, data, "fixture must round-trip");
        (compressed, full, info)
    }

    /// Emit a [`ResumeState`] for resuming at block `n` by decoding `[0, n)` on
    /// a throwaway decoder with `emit_resume = true`.
    #[cfg(feature = "lsm")]
    fn emit_resume_state_at(compressed: &[u8], n: u32) -> super::ResumeState {
        let mut src = compressed;
        let mut dec = FrameDecoder::new();
        dec.reset(&mut src).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut src, 0, n, None, true)
            .expect("prefix decode for resume-state emission");
        pd.resume_state
            .expect("emit_resume should populate resume_state")
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_matches_full_decode_at_first_mid_last() {
        // Acceptance criterion: after resuming at block N (cold decoder, primed
        // window + restored entropy), decode_blocks_partial yields bytes
        // byte-identical to a full decode's [ends[N-1]..ends[end-1]) slice, for
        // N in {1, mid, last}. Repeat_Mode entropy blocks are covered because
        // the emitted ResumeState carries the carry-over tables.
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        assert!(nblocks >= 4, "need several blocks, got {nblocks}");

        for &n in &[1u32, nblocks / 2, nblocks - 1] {
            // Producer: emit resume state for block n (separate decoder).
            let st = emit_resume_state_at(&compressed, n);
            assert_eq!(st.block_index(), n);
            let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;
            assert_eq!(st.output_offset(), output_offset);

            // Consumer: a FRESH (cold) decoder resumes at n. Pass the WHOLE
            // decompressed prefix as window_prime; it is capped to one window
            // internally, exercising the cap path.
            let window_prime = &full[..output_offset as usize];
            let mut header_src = compressed.as_slice();
            let mut dec = FrameDecoder::new();
            dec.reset(&mut header_src).unwrap();
            // Caller positions the source at block n's compressed frame offset.
            let off = info.blocks[n as usize].offset_in_frame as usize;
            let mut block_src = &compressed[off..];
            let pd = dec
                .decode_blocks_partial(
                    &mut block_src,
                    n,
                    u32::MAX,
                    Some(super::ResumeInput {
                        window_prime,
                        state: &st,
                    }),
                    false,
                )
                .unwrap_or_else(|e| panic!("resume decode at N={n} errored: {e:?}"));

            let start = output_offset as usize;
            let end = info
                .decompressed_byte_range((nblocks - 1) as usize)
                .unwrap()
                .end as usize;
            assert_eq!(
                pd.data.as_slice(),
                &full[start..end],
                "resumed bytes must equal the full-decode slice for N={n}"
            );
            assert_eq!(pd.start_block, n);
            assert_eq!(pd.blocks_decoded, nblocks - n);
            assert!(pd.stopped_at.is_none(), "clean resume at N={n}");
            assert!(pd.frame_finished, "decoded through the last block");
        }
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_with_exact_window_tail_matches_full_decode() {
        // Realistic cold-resume shape on a MULTI-SEGMENT frame: caller supplies
        // only the last `window_size` decompressed bytes (not the whole prefix),
        // which is all that can ever back a match.
        let (compressed, full, info) = multi_segment_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let window_size = frame_window_size(&compressed);
        // First block whose preceding output exceeds one window, so the tail
        // genuinely truncates the prefix.
        let n = (1..nblocks)
            .find(|&i| {
                info.decompressed_byte_range(i as usize).unwrap().start as usize > window_size
            })
            .expect("multi-segment frame must have a block past one window");
        let st = emit_resume_state_at(&compressed, n);
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;
        assert!(output_offset as usize > window_size);
        let tail_start = output_offset as usize - window_size;
        let window_prime = &full[tail_start..output_offset as usize];

        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut header_src).unwrap();
        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        let pd = dec
            .decode_blocks_partial(
                &mut block_src,
                n,
                u32::MAX,
                Some(super::ResumeInput {
                    window_prime,
                    state: &st,
                }),
                false,
            )
            .unwrap();

        let end = info
            .decompressed_byte_range((nblocks - 1) as usize)
            .unwrap()
            .end as usize;
        assert_eq!(pd.data.as_slice(), &full[output_offset as usize..end]);
        assert_eq!(pd.blocks_decoded, nblocks - n);
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_rejects_short_window_prime() {
        // Acceptance criterion: a window_prime shorter than the required window
        // is rejected with a typed error, not a silent mis-decode.
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let window_size = frame_window_size(&compressed);
        let n = nblocks / 2;
        let st = emit_resume_state_at(&compressed, n);
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;
        let required = core::cmp::min(window_size as u64, output_offset) as usize;
        assert!(required > 0, "mid block must require a non-empty window");

        // One byte short of the required window.
        let prime = &full[output_offset as usize - (required - 1)..output_offset as usize];

        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut header_src).unwrap();
        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        let err = dec
            .decode_blocks_partial(
                &mut block_src,
                n,
                u32::MAX,
                Some(super::ResumeInput {
                    window_prime: prime,
                    state: &st,
                }),
                false,
            )
            .expect_err("short window_prime must be rejected");
        match err {
            crate::decoding::errors::FrameDecoderError::ResumeWindowTooShort { got, need } => {
                assert_eq!(got, required - 1);
                assert_eq!(need, required);
            }
            other => panic!("expected ResumeWindowTooShort, got {other:?}"),
        }
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_range_validates_against_effective_start_not_start_block() {
        // In resume mode `start_block` is ignored and decoding begins at
        // `state.block_index()`. The range guard must therefore validate the
        // EFFECTIVE start against `end_block`: `end_block` below the resume
        // block is an inverted range and must error, not silently return an
        // empty decode. Caller passes the conventional ignored `start_block = 0`.
        let (compressed, _full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let n = (nblocks / 2).max(2);
        let st = emit_resume_state_at(&compressed, n);
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;

        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut header_src).unwrap();
        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        // end_block = n - 1 is below the resume block n → inverted range.
        let err = dec
            .decode_blocks_partial(
                &mut block_src,
                0,
                n - 1,
                Some(super::ResumeInput {
                    window_prime: &_full[..output_offset as usize],
                    state: &st,
                }),
                false,
            )
            .expect_err("end_block below the resume block must be an inverted range");
        match err {
            crate::decoding::errors::FrameDecoderError::InvalidBlockRange {
                start_block,
                end_block,
            } => {
                assert_eq!(start_block, n, "error must report the effective start");
                assert_eq!(end_block, n - 1);
            }
            other => panic!("expected InvalidBlockRange, got {other:?}"),
        }
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_rejects_state_from_a_different_frame() {
        // A ResumeState captured from one frame must not be applied to a frame
        // with a different decode shape (window size / single-segment / dict):
        // restoring foreign entropy tables would yield byte-wrong output. The
        // frame-identity guard must reject it up front with a typed error.
        let (frame_a, _full_a, info_a) = multi_block_fixture();
        let (frame_b, full_b, _info_b) = multi_segment_block_fixture();
        // Sanity: the two fixtures must differ in decode shape for the guard to
        // be exercised (single-segment vs multi-segment here).
        let st = emit_resume_state_at(&frame_a, (info_a.blocks.len() as u32 / 2).max(1));

        let mut header_src = frame_b.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut header_src).unwrap();
        // The frame-key check runs before the window-length check, so even a
        // valid-length window_prime for frame B is rejected on identity.
        let err = dec
            .decode_blocks_partial(
                &mut frame_b.as_slice(),
                st.block_index(),
                u32::MAX,
                Some(super::ResumeInput {
                    window_prime: &full_b,
                    state: &st,
                }),
                false,
            )
            .expect_err("resume state from a different frame must be rejected");
        assert!(
            matches!(
                err,
                crate::decoding::errors::FrameDecoderError::ResumeFrameMismatch
            ),
            "expected ResumeFrameMismatch, got {err:?}"
        );
    }

    #[cfg(all(feature = "lsm", feature = "hash"))]
    #[test]
    fn resume_rejects_wrong_window_prime_content() {
        // Same frame (FrameKey matches) but the caller supplies a window_prime
        // with one byte flipped. The shape key cannot catch this; the
        // content-exact XXH64 of the window must, rejecting before any restore
        // rather than mis-resolving matches against corrupted history.
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let n = (nblocks / 2).max(1);
        let st = emit_resume_state_at(&compressed, n);
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start as usize;
        assert!(output_offset > 0);

        // Correct prefix with the last byte corrupted (this byte is inside the
        // window the resume block reaches back into).
        let mut corrupted = full[..output_offset].to_vec();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;

        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut header_src).unwrap();
        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        let err = dec
            .decode_blocks_partial(
                &mut block_src,
                n,
                u32::MAX,
                Some(super::ResumeInput {
                    window_prime: &corrupted,
                    state: &st,
                }),
                false,
            )
            .expect_err("corrupted window_prime must be rejected by content hash");
        assert!(
            matches!(
                err,
                crate::decoding::errors::FrameDecoderError::ResumeFrameMismatch
            ),
            "expected ResumeFrameMismatch, got {err:?}"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_rejects_state_with_different_active_dictionary() {
        // A dictless-header frame can be decoded with an explicit dictionary
        // applied at runtime (force_dict / reset_with_dict_handle). Two such
        // decodes differ in entropy/repcode/dict context even though the header
        // dictionary_id is identically absent, so the resume guard must key on
        // the ACTIVE dictionary, not just the header field. Here the snapshot is
        // captured with no active dictionary; resuming with one applied must be
        // rejected before any state is restored.
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let n = (nblocks / 2).max(1);
        let st = emit_resume_state_at(&compressed, n); // active_dictionary_id = None
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start as usize;

        let raw = std::fs::read("./dict_tests/dictionary").expect("dictionary fixture");
        let dict = crate::decoding::dictionary::Dictionary::decode_dict(&raw).expect("parse dict");
        let dict_id = dict.id;

        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.add_dict(dict).unwrap();
        dec.reset(&mut header_src).unwrap();
        dec.force_dict(dict_id).unwrap(); // active_dictionary_id = Some(dict_id)
        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        let err = dec
            .decode_blocks_partial(
                &mut block_src,
                n,
                u32::MAX,
                Some(super::ResumeInput {
                    window_prime: &full[..output_offset],
                    state: &st,
                }),
                false,
            )
            .expect_err("resume with a different active dictionary must be rejected");
        assert!(
            matches!(
                err,
                crate::decoding::errors::FrameDecoderError::ResumeFrameMismatch
            ),
            "expected ResumeFrameMismatch, got {err:?}"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_invalid_range_does_not_mutate_decoder_state() {
        // An inverted effective range must be rejected WITHOUT priming the
        // decoder: no entropy restore, no window prime, no cursor advance. As
        // written before the fix, those mutations ran before the range check,
        // leaving the decoder in a synthetic resumed state on the error path.
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let n = (nblocks / 2).max(2);
        let st = emit_resume_state_at(&compressed, n);
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start as usize;

        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut header_src).unwrap();
        // Freshly reset: cursor at block 0.
        assert_eq!(dec.state.as_ref().unwrap().block_counter, 0);

        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        let err = dec
            .decode_blocks_partial(
                &mut block_src,
                0,
                n - 1, // below the resume block → inverted range
                Some(super::ResumeInput {
                    window_prime: &full[..output_offset],
                    state: &st,
                }),
                false,
            )
            .expect_err("inverted range must error");
        assert!(matches!(
            err,
            crate::decoding::errors::FrameDecoderError::InvalidBlockRange { .. }
        ));
        assert_eq!(
            dec.state.as_ref().unwrap().block_counter,
            0,
            "error path must not advance the cursor (validate before priming)"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn emit_resume_state_absent_on_terminal_block() {
        // When a decode reaches the frame's last block there is no "next block"
        // to resume at: the snapshot's block_index would be one past EOF and the
        // caller has no offset_in_frame for it. emit_resume must therefore yield
        // None on the terminal block, not a dangling snapshot.
        let (compressed, _full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let mut src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut src).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut src, 0, nblocks, None, true)
            .unwrap();
        assert!(pd.frame_finished, "decode must reach the last block");
        assert!(
            pd.resume_state.is_none(),
            "no resume state past the frame's last block"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn emit_resume_state_absent_when_not_requested() {
        // Default partial decode (emit_resume = false) must NOT pay the entropy
        // clone: resume_state stays None.
        let (compressed, _full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let mut src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut src).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut src, 0, nblocks, None, false)
            .unwrap();
        assert!(
            pd.resume_state.is_none(),
            "resume_state must be None unless emit_resume is set"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn resume_grow_loop_reconstructs_full() {
        // The motivating scenario: a symmetric one-call grow-loop. Each call
        // takes the previous ResumeState and emits the next, decoding only the
        // new extent — concatenated, the extents reconstruct the full output
        // with no prefix ever re-decompressed.
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        assert!(nblocks >= 4);

        // Walk the frame in extents of `step` blocks each.
        let step = (nblocks / 3).max(1);
        let mut combined: Vec<u8> = Vec::new();
        let mut next: u32 = 0;
        let mut carry: Option<super::ResumeState> = None;

        while next < nblocks {
            let end = (next + step).min(nblocks);
            let mut dec = FrameDecoder::new();
            let mut header_src = compressed.as_slice();
            dec.reset(&mut header_src).unwrap();

            let off = info.blocks[next as usize].offset_in_frame as usize;
            let mut block_src = &compressed[off..];

            let output_offset = info.decompressed_byte_range(next as usize).unwrap().start;
            let pd = if let Some(st) = carry.as_ref() {
                // Resume from the prior extent's state (cold: fresh decoder).
                let window_prime = &full[..output_offset as usize];
                dec.decode_blocks_partial(
                    &mut block_src,
                    next,
                    end,
                    Some(super::ResumeInput {
                        window_prime,
                        state: st,
                    }),
                    true,
                )
                .unwrap()
            } else {
                // First extent: no resume input, just emit for the next.
                dec.decode_blocks_partial(&mut block_src, next, end, None, true)
                    .unwrap()
            };

            combined.extend_from_slice(&pd.data);
            carry = pd.resume_state;
            next = end;
        }

        assert_eq!(
            combined, full,
            "grow-loop extents must reconstruct the full output"
        );
    }

    #[cfg(all(feature = "lsm", feature = "hash"))]
    #[test]
    fn resume_does_not_redecode_prefix_blocks() {
        // Instrumented confirmation that blocks < N are not re-decoded on
        // resume. With per-block checksums enabled on the resuming decoder, the
        // resumed decode must record exactly one digest per in-range block
        // (end - N), never one per frame block.
        let (compressed, full, info) = multi_block_fixture();
        let nblocks = info.blocks.len() as u32;
        let n = nblocks / 2;
        let st = emit_resume_state_at(&compressed, n);
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;

        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.enable_per_block_checksums();
        dec.reset(&mut header_src).unwrap();
        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        let _ = dec
            .decode_blocks_partial(
                &mut block_src,
                n,
                u32::MAX,
                Some(super::ResumeInput {
                    window_prime: &full[..output_offset as usize],
                    state: &st,
                }),
                false,
            )
            .unwrap();

        assert_eq!(
            dec.computed_block_checksums().len() as u32,
            nblocks - n,
            "resume must decode only in-range blocks, not re-decode the prefix"
        );
    }
}
