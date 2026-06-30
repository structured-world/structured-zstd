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
/// error surface stays byte-identical to the upstream zstd.
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
    /// Test-only observability: frames decoded via `run_direct_decode`.
    /// The direct and buffered paths are byte-identical, so dispatch
    /// regressions (e.g. re-excluding dictionary frames from the direct
    /// gate) are invisible to output assertions; tests pin the path here.
    #[cfg(test)]
    direct_frames: u64,
    // Registered dictionaries are stored by shared handle (Arc/Rc) so a
    // single content copy is referenced by every frame the decoder decodes
    // (upstream zstd `ZSTD_refDDict`), rather than re-copied into the decode buffer
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

    fn workspace_bytes(&self) -> usize {
        match self {
            Self::Ring(s) => s.workspace_bytes(),
            Self::Flat(s) => s.workspace_bytes(),
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
    ///
    /// `window_size` is the TARGET visible-window capacity: callers pass
    /// the full window, and the method itself computes the shortfall past
    /// the bytes already buffered before calling the backend's
    /// ADDITIONAL-semantics `reserve_exact`. That keeps re-entries (the
    /// decode_all fallback loop runs `decode_blocks` once per strategy
    /// chunk, and streaming callers invoke it per call) from growing a
    /// window-full buffer toward 2x window, while per-block growth keeps
    /// the amortized `reserve`.
    #[inline]
    fn reserve_buffer(&mut self, window_size: usize) {
        // Exact growth: this is the one-shot pre-reservation, and a request
        // landing one slack past the retained capacity (e.g. a dictionary
        // prefix already loaded into the buffer) must not DOUBLE a
        // window-sized allocation through the amortized policy. Per-block
        // growth keeps the amortized `reserve`.
        //
        // `reserve_exact` takes ADDITIONAL capacity, so request only the
        // shortfall past the bytes already buffered: the decode_all
        // fallback loop re-enters `decode_blocks` once per strategy chunk,
        // and re-requesting the full window each iteration would grow a
        // window-sized buffer toward 2x window.
        match self {
            Self::Ring(s) => {
                let additional = window_size.saturating_sub(s.buffer.len());
                s.buffer.reserve_exact(additional);
            }
            Self::Flat(s) => {
                let additional = window_size.saturating_sub(s.buffer.len());
                s.buffer.reserve_exact(additional);
            }
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
        dict: Option<&crate::decoding::dictionary::Dictionary>,
    ) -> (
        crate::decoding::scratch::FSEScratch,
        crate::decoding::scratch::HuffmanScratch,
        [u32; 3],
    ) {
        let (fse_src, huf_src, offset_hist) = match self {
            Self::Ring(s) => (&s.fse, &s.huf, s.offset_hist),
            Self::Flat(s) => (&s.fse, &s.huf, s.offset_hist),
        };
        // The live scratch may still be `Dict`-sourced; `reinit_from` /
        // `reinit_resolved_from` resolve those axes through the borrow into a
        // self-contained `Local` snapshot, so the dictionary is required here
        // whenever the captured frame is dict-backed.
        let mut fse = crate::decoding::scratch::FSEScratch::new();
        fse.reinit_from(fse_src, dict);
        let mut huf = crate::decoding::scratch::HuffmanScratch::new();
        huf.reinit_resolved_from(huf_src, dict);
        (fse, huf, offset_hist)
    }

    /// Install entropy/repcode state from a [`ResumeState`] into the live
    /// scratch so a `Repeat_Mode` / treeless resume block resolves against the
    /// same tables a contiguous decode would have carried over.
    #[cfg(feature = "lsm")]
    fn restore_entropy(&mut self, state: &ResumeState) {
        // The `ResumeState` snapshot is a self-contained `Local` materialization
        // (export resolved every Dict axis into local bytes and detached), so no
        // dictionary borrow is needed to install it back.
        match self {
            Self::Ring(s) => {
                s.fse.reinit_from(&state.fse, None);
                s.huf.reinit_resolved_from(&state.huf, None);
                s.offset_hist = state.offset_hist;
            }
            Self::Flat(s) => {
                s.fse.reinit_from(&state.fse, None);
                s.huf.reinit_resolved_from(&state.huf, None);
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
        dict: Option<&crate::decoding::dictionary::Dictionary>,
    ) -> Result<u64, DecodeBlockContentError> {
        match self {
            Self::Ring(s) => decoder.decode_block_content(header, s, dict, source),
            Self::Flat(s) => decoder.decode_block_content(header, s, dict, source),
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
    /// The dictionary handle applied to this frame, owned for the frame's whole
    /// decode so the block loop can hand the scratch a `&Dictionary` borrow at
    /// every `Dict`-sourced table read. ONE refcount clone per dict-apply (not
    /// per block, not per frame on the reuse path — the same handle stays held
    /// across `init_with_dict_handle` -> `decode_blocks`); the decode loop
    /// borrows from this field (disjoint from `decoder_scratch`) with zero
    /// further clones. `None` on the no-dict path. Cleared by `reset`.
    active_dict: Option<DictionaryHandle>,
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
    /// Window size to actually reserve for this frame's decode buffer.
    /// A declared content size caps the useful window: matches can never
    /// reference further back than the bytes that will ever exist, so an
    /// encoder-declared window above the FCS (e.g. a level-preset window
    /// on a smaller input) must not inflate the reservation. Every
    /// `reserve_buffer` site routes through this so the cap is uniform
    /// across `decode_all_impl`, `decode_blocks`, and the partial path.
    fn useful_window_size(&self) -> usize {
        let window_size = self.frame_header.window_size().unwrap_or(0);
        if self.frame_header.fcs_declared() {
            window_size.min(self.frame_header.frame_content_size()) as usize
        } else {
            window_size as usize
        }
    }

    /// Construct a new frame decoder state, reading the frame header
    /// from `source`. When `magicless` is `true`, the 4-byte magic
    /// number prefix is NOT consumed (upstream zstd `ZSTD_f_zstd1_magicless`).
    /// Crate-internal — reached only via `FrameDecoder::init` /
    /// `FrameDecoder::init_with_dict_handle`. The decode buffer is
    /// allocated lazily on BOTH backends (`new_ring` and `new_flat`):
    /// direct-eligible frames pay zero buffer allocation, and the
    /// non-direct fallback reserves `window_size` once in
    /// `decode_all_impl` / `decode_blocks` via `reserve_buffer` before
    /// any block write.
    #[inline]
    pub(crate) fn new_with_format(
        source: impl Read,
        magicless: bool,
    ) -> Result<FrameDecoderState, FrameDecoderError> {
        let (frame, header_size) = frame::read_frame_header_with_format(source, magicless)?;
        Self::new_with_parsed_header(frame, header_size)
    }

    /// Build a fresh state from an already-parsed frame header (the non-parsing
    /// tail of [`new_with_format`]). Shared by the `Read` path and the
    /// slice-direct path ([`FrameDecoder::reset_from_slice`]).
    pub(crate) fn new_with_parsed_header(
        frame: frame::FrameHeader,
        header_size: u8,
    ) -> Result<FrameDecoderState, FrameDecoderError> {
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
            active_dict: None,
        })
    }

    /// Reset this state for a new frame read from `source`, reusing
    /// existing allocations. When `magicless` is `true`, the frame
    /// header is read WITHOUT expecting a magic-number prefix
    /// (upstream zstd `ZSTD_f_zstd1_magicless`). Crate-internal — reached
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
    #[inline]
    pub(crate) fn reset_with_format(
        &mut self,
        source: impl Read,
        magicless: bool,
    ) -> Result<(), FrameDecoderError> {
        let (frame_header, header_size) = frame::read_frame_header_with_format(source, magicless)?;
        self.reset_with_parsed_header(frame_header, header_size)
    }

    /// Apply an already-parsed frame header to this state (the non-parsing tail
    /// of [`reset_with_format`]). Shared by the `Read` path and the slice-direct
    /// path ([`FrameDecoder::reset_from_slice`]).
    #[inline]
    pub(crate) fn reset_with_parsed_header(
        &mut self,
        frame_header: frame::FrameHeader,
        header_size: u8,
    ) -> Result<(), FrameDecoderError> {
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
        // `active_dict` is intentionally NOT cleared here: it is only ever READ
        // while a scratch table source is `Dict`, which `init_from_dict` arms
        // on a dict frame and which a no-dict frame leaves `Local` (so a stale
        // held handle is never read). Keeping it lets the per-apply `ptr::eq`
        // reuse-check below skip the clone when the SAME dictionary is
        // re-applied frame-over-frame (the CoordiNode per-label-dict hot path)
        // — zero refcount churn on reuse, one clone only on a genuine swap.
        Ok(())
    }

    /// Hold the dictionary handle for this frame's whole decode so the block
    /// loop can borrow `&Dictionary` at every `Dict`-sourced read. Clones the
    /// handle ONLY when it is a different dictionary than the one already held
    /// (`ptr::eq` on the `Arc`'s pointee) — so re-applying the SAME dictionary
    /// frame-over-frame (the reuse hot path) costs zero refcount churn.
    fn set_active_dict(&mut self, dict: &DictionaryHandle) {
        if self
            .active_dict
            .as_ref()
            .is_none_or(|held| !core::ptr::eq(held.as_dict(), dict.as_dict()))
        {
            self.active_dict = Some(dict.clone());
        }
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
            #[cfg(test)]
            direct_frames: 0,
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

    /// Heap bytes currently held by the decoder's lazily-grown workspace:
    /// the decode-window buffer plus the per-block literal/content buffers
    /// and the entropy tables. Returns 0 before the first frame is initialised
    /// (no workspace allocated yet). The window allocation dominates and grows
    /// with the frame's window size; this is the value to track for decode-time
    /// memory pressure, mirroring the workspace term of upstream
    /// `ZSTD_sizeof_DCtx`. Shared dictionaries (ref-counted handles) are not
    /// counted, matching upstream excluding `refDDict` memory.
    pub fn workspace_size(&self) -> usize {
        self.state
            .as_ref()
            .map_or(0, |s| s.decoder_scratch.workspace_bytes())
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
    /// semantic as upstream zstd `ZSTD_d_windowLogMax` (which is a
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
    /// Byte-exact equality, NOT a ceiling. Upstream zstd
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
    #[inline]
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
    #[inline]
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
            state.set_active_dict(dict);
            state.using_dict = Some(dict_id);
        }
        Ok(())
    }

    /// Slice-direct equivalent of [`reset`](Self::reset) for the in-memory
    /// decode path: parses the frame header straight out of `*input` via
    /// [`frame::read_frame_header_from_slice`] (no `Read`-trait `read_exact`
    /// per field) and advances `*input` past it, then applies it through the
    /// shared parsed-header path. Behaviour — including skippable-frame and
    /// truncation errors, dictionary-id resolution, and pinned-expectation
    /// validation — is identical to `reset`; only the header read avoids the
    /// `io::impls` dispatch.
    pub(crate) fn reset_from_slice(&mut self, input: &mut &[u8]) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        #[cfg(all(feature = "lsm", feature = "hash"))]
        self.computed_block_checksums.clear();
        let magicless = self.magicless;
        let (frame_header, header_size) = frame::read_frame_header_from_slice(input, magicless)?;
        let dict_id = match &mut self.state {
            Some(s) => {
                s.reset_with_parsed_header(frame_header, header_size)?;
                s.frame_header.dictionary_id()
            }
            None => {
                self.state = Some(FrameDecoderState::new_with_parsed_header(
                    frame_header,
                    header_size,
                )?);
                self.state
                    .as_ref()
                    .and_then(|state| state.frame_header.dictionary_id())
            }
        };
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
            state.set_active_dict(dict);
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
        state.set_active_dict(dict);
        state.using_dict = Some(dict.id());
        Ok(())
    }

    /// Slice-direct equivalent of [`reset_with_dict_handle`](Self::reset_with_dict_handle)
    /// for the in-memory decode path: parses the frame header straight out of
    /// `*input` via [`frame::read_frame_header_from_slice`] (no `Read`-trait
    /// `read_exact` per field) and applies it through the shared parsed-header
    /// path, then attaches `dict`. Behaviour — dictionary-id mismatch, pinned
    /// expectations, scratch init — is identical to `reset_with_dict_handle`;
    /// only the header read avoids the `io::impls` dispatch.
    pub(crate) fn reset_from_slice_with_dict_handle(
        &mut self,
        input: &mut &[u8],
        dict: &DictionaryHandle,
    ) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        #[cfg(all(feature = "lsm", feature = "hash"))]
        self.computed_block_checksums.clear();
        Self::validate_registered_dictionary(dict.as_dict())?;
        let magicless = self.magicless;
        let (frame_header, header_size) = frame::read_frame_header_from_slice(input, magicless)?;
        match &mut self.state {
            Some(s) => s.reset_with_parsed_header(frame_header, header_size)?,
            None => {
                self.state = Some(FrameDecoderState::new_with_parsed_header(
                    frame_header,
                    header_size,
                )?);
            }
        }
        #[cfg(feature = "lsm")]
        {
            let header = &self
                .state
                .as_ref()
                .expect("state populated by reset_with_parsed_header/new_with_parsed_header")
                .frame_header;
            self.validate_expectations(header)?;
        }
        let state = self
            .state
            .as_mut()
            .expect("state populated by reset_with_parsed_header/new_with_parsed_header");
        if let Some(dict_id) = state.frame_header.dictionary_id()
            && dict_id != dict.id()
        {
            return Err(err::DictIdMismatch {
                expected: dict_id,
                provided: dict.id(),
            });
        }
        state.decoder_scratch.init_from_dict(dict);
        state.set_active_dict(dict);
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
        state.set_active_dict(dict);
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

    /// Test-only: number of frames decoded through the single-copy direct
    /// path (`run_direct_decode`). Lets cross-module tests assert that a
    /// given decode took the decode-in-place path rather than the ring drain.
    #[cfg(test)]
    pub(crate) fn direct_frames(&self) -> u64 {
        self.direct_frames
    }

    /// Test-only: whether the decode state currently holds an owning dictionary
    /// handle (`active_dict`). Every path that arms `Dict`-sourced scratch tables
    /// must also install this handle, or a later dict-table read resolves `None`.
    #[cfg(test)]
    pub(crate) fn active_dict_installed(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.active_dict.is_some())
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
        // any block decodes into the ring. Hash only when a digest is both
        // wanted (mode != None) AND present in the frame (content_checksum_flag
        // set) — a flag-off frame has nothing to verify or expose, so hashing
        // it is wasted work. Mirrors the direct path and get_calculated_checksum.
        #[cfg(feature = "hash")]
        let checksum_mode = self.content_checksum;
        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;
        #[cfg(feature = "hash")]
        {
            let compute_hash = checksum_mode != ContentChecksum::None
                && state.frame_header.descriptor.content_checksum_flag();
            state.decoder_scratch.set_compute_hash(compute_hash);
        }

        // Streaming entry point: pre-reserve the backing buffer to
        // the FCS-capped window so multi-block frames don't pay repeated
        // `reserve_amortized` grow steps (128 KiB → 256 KiB → ... →
        // window) as blocks accumulate. `decode_all` does the same up
        // front in `decode_all_impl`; this mirrors it for callers
        // driving `decode_blocks` directly. Idempotent — the
        // backend's `reserve` early-returns when capacity is already
        // sufficient.
        let useful_window = state.useful_window_size();
        state.decoder_scratch.reserve_buffer(useful_window);

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
            // Only expose the held dictionary while THIS frame is dict-backed
            // (`using_dict` is set per dict-apply, cleared on reset). A reused
            // decoder keeps `active_dict` across a no-dict frame for the
            // `ptr::eq` reuse-skip, so it must be gated here or a stray
            // out-of-window offset on a dictless frame would resolve against the
            // stale dictionary content instead of erroring.
            let dict_ref = if state.using_dict.is_some() {
                state.active_dict.as_ref().map(|h| h.as_dict())
            } else {
                None
            };
            let bytes_read_in_block_body = state
                .decoder_scratch
                .decode_block_content(&mut block_dec, &block_header, &mut source, dict_ref)
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
        #[cfg(feature = "hash")]
        let checksum_mode = self.content_checksum;
        let magicless = self.magicless;
        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;

        // Honor the checksum mode before any drain/read can hash: `None` must
        // compute no XXH64. `decode_blocks` sets this; the partial path must too,
        // or a reused scratch keeps hashing with the default-enabled state.
        #[cfg(feature = "hash")]
        {
            let compute_hash = checksum_mode != ContentChecksum::None
                && state.frame_header.descriptor.content_checksum_flag();
            state.decoder_scratch.set_compute_hash(compute_hash);
        }

        // Mirror `decode_blocks`: pre-reserve the backing buffer to the
        // FCS-capped window so multi-block frames don't pay repeated grow
        // steps. The RAW frame window stays separately bound — the resume
        // logic below bounds match reach by the frame's window semantics,
        // not by the (possibly smaller) reservation cap.
        let window_size = state.frame_header.window_size().unwrap_or(0) as usize;
        let useful_window = state.useful_window_size();
        state.decoder_scratch.reserve_buffer(useful_window);

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
            // Only expose the held dictionary while THIS frame is dict-backed
            // (`using_dict` is set per dict-apply, cleared on reset). A reused
            // decoder keeps `active_dict` across a no-dict frame for the
            // `ptr::eq` reuse-skip, so it must be gated here or a stray
            // out-of-window offset on a dictless frame would resolve against the
            // stale dictionary content instead of erroring.
            let dict_ref = if state.using_dict.is_some() {
                state.active_dict.as_ref().map(|h| h.as_dict())
            } else {
                None
            };
            match state.decoder_scratch.decode_block_content(
                &mut block_dec,
                &block_header,
                &mut source,
                dict_ref,
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
            let dict_ref = if state.using_dict.is_some() {
                state.active_dict.as_ref().map(|h| h.as_dict())
            } else {
                None
            };
            let (fse, huf, offset_hist) = state.decoder_scratch.export_entropy(dict_ref);
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

                // Honour the content-checksum mode on this hand-rolled decode
                // loop (it does not go through `decode_blocks`): hash only when
                // a digest is wanted and the frame carries one. `None` skips the
                // XXH64 pass; verification happens after the final drain below.
                #[cfg(feature = "hash")]
                {
                    let compute_hash = self.content_checksum != ContentChecksum::None
                        && state.frame_header.descriptor.content_checksum_flag();
                    state.decoder_scratch.set_compute_hash(compute_hash);
                }

                if state.frame_header.descriptor.content_checksum_flag()
                    && state.frame_finished
                    && state.check_sum.is_none()
                {
                    // The trailing checksum arrived on a separate call (the last
                    // block finished earlier). Consume it and fall through to the
                    // shared `self.read` + post-drain verify below — NOT an early
                    // return — so any output still buffered from a prior
                    // small-`target` call is flushed on this call too, and the
                    // checksum is verified through the one shared path.
                    if mt_source.len() >= 4 {
                        let chksum = mt_source[..4].try_into().expect("optimized away");
                        state.bytes_read_counter += 4;
                        let chksum = u32::from_le_bytes(chksum);
                        state.check_sum = Some(chksum);
                        mt_source = &mt_source[4..];
                    }
                }

                loop {
                    // The frame is fully decoded (last block seen, trailer
                    // consumed above); no more blocks to read. Any leftover
                    // bytes are not a block header — stop before misreading them.
                    if state.frame_finished {
                        break;
                    }
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

                    // Only expose the held dictionary while THIS frame is dict-backed
                    // (`using_dict` is set per dict-apply, cleared on reset). A reused
                    // decoder keeps `active_dict` across a no-dict frame for the
                    // `ptr::eq` reuse-skip, so it must be gated here or a stray
                    // out-of-window offset on a dictless frame would resolve against the
                    // stale dictionary content instead of erroring.
                    let dict_ref = if state.using_dict.is_some() {
                        state.active_dict.as_ref().map(|h| h.as_dict())
                    } else {
                        None
                    };
                    let bytes_read_in_block_body = state
                        .decoder_scratch
                        .decode_block_content(
                            &mut block_dec,
                            &block_header,
                            &mut mt_source,
                            dict_ref,
                        )
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
        // Once the frame is fully decoded and drained, the running digest is
        // final: validate it in `Verify` mode (no-op otherwise). Same finish
        // point as the streaming reader.
        #[cfg(feature = "hash")]
        if self.is_finished() && self.can_collect() == 0 {
            self.verify_content_checksum()?;
        }
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
            self.decode_all_impl(input, output, |this, src| this.reset_from_slice(src))
        }
        #[cfg(feature = "lsm")]
        {
            self.decode_all_impl(input, output, |this, src| this.reset_from_slice(src), None)
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
            |this, src| this.reset_from_slice(src),
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
                this.reset_from_slice_with_dict_handle(src, dict)
            })
        }
        #[cfg(feature = "lsm")]
        {
            self.decode_all_impl(
                input,
                output,
                |this, src| this.reset_from_slice_with_dict_handle(src, dict),
                None,
            )
        }
    }

    /// Whether the decoder sits at the very start of an initialised frame:
    /// the header has been read (state populated) but no block has been
    /// decoded and the frame is not finished. In this state the wrapped
    /// source is positioned exactly after the frame header, so
    /// [`Self::decode_current_frame_to_vec`] can decode the rest of the frame
    /// straight from the remaining source bytes.
    pub(crate) fn is_at_frame_start(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|s| s.block_counter == 0 && !s.frame_finished)
    }

    /// Decode the CURRENT (already-initialised) frame, APPENDING the
    /// decompressed bytes to `output`, and return the number appended.
    ///
    /// `input` must be the frame's post-header bytes (the wrapped source after
    /// `init` consumed the header). Unlike [`Self::decode_all_to_vec`] this
    /// neither re-reads a header nor requires the caller to pre-reserve
    /// capacity: a frame that declares its content size decodes DIRECTLY into
    /// freshly-grown `output` capacity via the single-copy direct path
    /// ([`Self::run_direct_decode`]) — bypassing the `Ring`/`FlatBuf` →
    /// `read()` drain copy the streaming loop pays — while an unsized frame
    /// falls back to the window-bounded ring drain (still one copy, into
    /// `output`). Backs [`StreamingDecoder`](crate::decoding::StreamingDecoder)'s
    /// `read_to_end` fast path; the caller must ensure
    /// [`Self::is_at_frame_start`].
    ///
    /// # Errors
    ///
    /// Propagates any [`FrameDecoderError`] from block decode, content-size
    /// mismatch, or (in `Verify` mode) checksum validation.
    pub(crate) fn decode_current_frame_to_vec(
        &mut self,
        mut input: &[u8],
        output: &mut Vec<u8>,
        dict: Option<&DictionaryHandle>,
    ) -> Result<usize, FrameDecoderError> {
        let start_len = output.len();
        // The current frame is already initialised (its header consumed by the
        // caller, WITH `dict` applied if the decoder was constructed with one).
        // Decode it, then decode any FOLLOWING concatenated / skippable frames
        // in `input` so the whole source is consumed to EOF and nothing is
        // dropped (matching `read_to_end` semantics).
        self.decode_one_frame_to_vec(&mut input, output)?;
        self.decode_concatenated_frames_to_vec(&mut input, output, dict)?;
        Ok(output.len() - start_len)
    }

    /// Initialise and decode every frame remaining in `input` (concatenated /
    /// skippable), APPENDING to `output`. `input` is advanced as frames are
    /// consumed; on return it is empty. Re-initialisation honours `dict`: when
    /// `Some`, each following frame is initialised via
    /// [`Self::init_with_dict_handle`] so a forced dictionary is preserved even
    /// for frames that omit the dictionary id (plain [`Self::init`] would
    /// resolve dictionaries by id only). Backs the `read_to_end` fast path (the
    /// frames after the current one) and its mid-frame fallback (the frames
    /// after the partially-read one).
    pub(crate) fn decode_concatenated_frames_to_vec(
        &mut self,
        input: &mut &[u8],
        output: &mut Vec<u8>,
        dict: Option<&DictionaryHandle>,
    ) -> Result<usize, FrameDecoderError> {
        let start_len = output.len();
        while !input.is_empty() {
            let init_result = match dict {
                Some(d) => self.init_with_dict_handle(&mut *input, d),
                None => self.init(&mut *input),
            };
            match init_result {
                Ok(_) => {}
                Err(FrameDecoderError::ReadFrameHeaderError(
                    crate::decoding::errors::ReadFrameHeaderError::SkipFrame { length, .. },
                )) => {
                    *input = input
                        .get(length as usize..)
                        .ok_or(FrameDecoderError::FailedToSkipFrame)?;
                    continue;
                }
                Err(e) => return Err(e),
            }
            self.decode_one_frame_to_vec(&mut *input, output)?;
        }
        Ok(output.len() - start_len)
    }

    /// Decode the single CURRENT (already-initialised) frame, APPENDING to
    /// `output`. Helper for [`Self::decode_current_frame_to_vec`].
    fn decode_one_frame_to_vec(
        &mut self,
        input: &mut &[u8],
        output: &mut Vec<u8>,
    ) -> Result<usize, FrameDecoderError> {
        let frame_start = output.len();
        let (content_size, fcs_declared) = {
            let s = self.state.as_ref().expect("frame is initialised");
            (
                s.frame_header.frame_content_size(),
                s.frame_header.fcs_declared(),
            )
        };
        // Direct path: a declared, non-empty content size that FITS in `usize`
        // (and whose end offset does not overflow). `usize::try_from` guards the
        // 32-bit / oversized-FCS truncation; an unrepresentable size falls
        // through to the window-bounded ring drain rather than allocating a
        // truncated buffer that would violate `run_direct_decode`'s precondition.
        //
        // Plausibility gate: the direct path `resize`s `output` to the declared
        // size up front, so a tiny/truncated frame declaring a huge (but
        // representable) FCS would allocate + zero that whole size before the
        // body is validated. zstd's per-block ceiling is MAX_BLOCK_SIZE from as
        // little as ~4 input bytes, so the declared size cannot legitimately
        // exceed `input.len() * (MAX_BLOCK_SIZE / 4)`. Anything larger falls
        // through to the ring drain, which grows only as real bytes are produced
        // and errors out cheaply on truncated input. `input` spans the remaining
        // source (this frame plus any following ones), so the bound only ever
        // over-permits — a legitimate frame is never forced off the direct path.
        // saturating_mul is intentional: an overflow means the available input
        // is so large that any representable FCS is plausible (cap = "no limit").
        const MAX_DECOMPRESSION_RATIO: usize = (crate::common::MAX_BLOCK_SIZE / 4) as usize;
        if content_size > 0
            && let Ok(cs) = usize::try_from(content_size)
            && cs <= input.len().saturating_mul(MAX_DECOMPRESSION_RATIO)
            && let Some(frame_end) = frame_start.checked_add(cs)
        {
            // Reserve exactly the frame's content and decode straight into it
            // (single copy, no ring). The direct path writes precisely
            // `content_size` bytes (erroring otherwise), so the grown region is
            // fully written.
            output.resize(frame_end, 0);
            // On error, drop the just-grown (zeroed) tail before propagating so
            // callers never observe bytes that were never decoded.
            let written =
                match self.run_direct_decode(&mut *input, &mut output[frame_start..], content_size)
                {
                    Ok(n) => n,
                    Err(e) => {
                        output.truncate(frame_start);
                        return Err(e);
                    }
                };
            output.truncate(frame_start + written);
            #[cfg(feature = "hash")]
            self.verify_content_checksum()?;
            return Ok(written);
        }
        // The ring-drain fallback below pre-reserves `useful_window_size()`
        // (= `window.min(FCS)`), which for a single-segment frame is the
        // declared FCS itself — so a truncated single-segment frame lying about
        // its size would still allocate the pledged window before the body
        // errors, sidestepping the direct-path gate above. Reject such a frame
        // up front when its declared (FCS-bearing) window exceeds what the
        // available input could plausibly produce. Frames without a declared
        // size keep their window-descriptor reservation (already capped at
        // `MAXIMUM_ALLOWED_WINDOW_SIZE` at init); a small-window multi-segment
        // frame still falls through to the ring drain, which errors cheaply on
        // the truncated body.
        if fcs_declared
            && let Some(state) = self.state.as_ref()
            && state.useful_window_size() > input.len().saturating_mul(MAX_DECOMPRESSION_RATIO)
        {
            return Err(FrameDecoderError::FrameContentSizeMismatch {
                declared: content_size,
                produced: 0,
            });
        }
        // No declared size, explicit FCS=0, or an unrepresentable FCS: window-
        // bounded ring drain, appended directly to `output` via
        // `collect_to_writer` (no staging buffer).
        loop {
            self.decode_blocks(&mut *input, BlockDecodingStrategy::UptoBytes(1024 * 1024))?;
            self.collect_to_writer(&mut *output)
                .map_err(FrameDecoderError::FailedToDrainDecodebuffer)?;
            if self.is_finished() {
                // Final flush of the retained window tail.
                self.collect_to_writer(&mut *output)
                    .map_err(FrameDecoderError::FailedToDrainDecodebuffer)?;
                break;
            }
        }
        let produced = (output.len() - frame_start) as u64;
        // A declared content size MUST match what the body produced — otherwise
        // accept the same corrupt frames `decode_all_impl` rejects (e.g. an
        // explicit FCS=0 whose body emits bytes). Use `fcs_declared()` so an
        // on-wire FCS=0 is validated, while an unknown size is not.
        if fcs_declared && produced != content_size {
            return Err(FrameDecoderError::FrameContentSizeMismatch {
                declared: content_size,
                produced,
            });
        }
        #[cfg(feature = "hash")]
        self.verify_content_checksum()?;
        Ok(produced as usize)
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
            // up as `FrameDecoderError`. Eligibility (FCS > 0,
            // remaining `output` slice holds the declared content)
            // puts the frame on the fast path that bypasses the
            // FlatBuf/Ring -> `read()` drain copy. Ineligible frames
            // (no FCS, output too small) fall through to the legacy
            // `decode_blocks` + `read` drain loop below. Dictionary
            // frames are eligible: `run_direct_decode` hands the
            // shared dict handle to its buffer, and beyond-prefix
            // offsets resolve through `repeat_from_dict`.
            let (content_size, fcs_declared) = {
                let state_ref = self.state.as_ref().expect("init populated state");
                (
                    state_ref.frame_header.frame_content_size(),
                    state_ref.frame_header.fcs_declared(),
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
            let direct_eligible = content_size > 0 && (output.len() as u64) >= content_size;
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
                // FCS-capped via `useful_window_size` — the same cap
                // `decode_blocks` applies, so its per-iteration reserve in
                // the loop below cannot grow the buffer back to the raw
                // frame window.
                let useful_window = state.useful_window_size();
                state.decoder_scratch.reserve_buffer(useful_window);
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
            // up as `FrameDecoderError`. Eligibility (FCS > 0,
            // remaining `output` slice holds the declared content)
            // puts the frame on the fast path that bypasses the
            // FlatBuf/Ring -> `read()` drain copy. Ineligible frames
            // (no FCS, output too small) fall through to the legacy
            // `decode_blocks` + `read` drain loop below. Dictionary
            // frames are eligible (see the no-lsm path above).
            let (content_size, fcs_declared) = {
                let state_ref = self.state.as_ref().expect("init populated state");
                (
                    state_ref.frame_header.frame_content_size(),
                    state_ref.frame_header.fcs_declared(),
                )
            };
            // Only `cap >= frame_content_size` needed; the trailing
            // sequence(s) take the bounded copy in
            // `UserSliceBackend::exec_sequence_bounded`, so no
            // `WILDCOPY_OVERLENGTH` trailing slack is required (see the
            // no-lsm path above).
            let direct_eligible = content_size > 0 && (output.len() as u64) >= content_size;
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
                // FCS-capped via `useful_window_size` — the same cap
                // `decode_blocks` applies, so its per-iteration reserve in
                // the loop below cannot grow the buffer back to the raw
                // frame window.
                let useful_window = state.useful_window_size();
                state.decoder_scratch.reserve_buffer(useful_window);
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
    ///
    /// Dictionary frames are supported: the scratch buffer's shared
    /// dict handle is forwarded to the stack-local `DecodeBuffer`, so
    /// offsets reaching past the frame's own output resolve through
    /// `repeat_from_dict` (the ext-dict slow path).
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
        #[cfg(test)]
        {
            self.direct_frames += 1;
        }
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

        // Fast path: a frame that is a single RAW block spanning the whole
        // declared content. Upstream zstd handles this as one `ZSTD_copyRawBlock`
        // (a `memmove`) inside `ZSTD_decompressFrame`; do the same here — a direct
        // `copy_from_slice` into the caller's output slice — skipping the
        // `DirectScratch` / `DecodeBuffer` / `UserSliceBackend` wrapper
        // construction and the general per-block loop. Incompressible payloads
        // (random / already-compressed data) emit exactly this shape, so the win
        // lands on small high-entropy frames where the per-frame machinery, not
        // the 1-block copy, dominates.
        {
            let mut probe = *input;
            let mut header_dec = block_decoder::new();
            if let Ok((bh, hsize)) = header_dec.read_block_header(&mut probe) {
                let n = bh.decompressed_size as usize;
                if bh.last_block
                    && matches!(bh.block_type, crate::blocks::block::BlockType::Raw)
                    && n as u64 == content_size
                    && probe.len() >= n
                    && output.len() >= n
                {
                    output[..n].copy_from_slice(&probe[..n]);
                    *input = &probe[n..];
                    state.bytes_read_counter += u64::from(hsize) + n as u64;
                    state.block_counter += 1;
                    // Consume the trailing 4-byte content checksum UNCONDITIONALLY
                    // when the frame declares one — exactly like the general
                    // direct loop and `decode_blocks`. Only the hash SEEDING is
                    // `hash`-gated; the byte consumption / counter / `check_sum`
                    // must not be, or a no-`hash` build leaves the 4 bytes in
                    // `*input` (misparsed as the next frame) and never sets
                    // `check_sum` (so `is_finished` stays false).
                    if state.frame_header.descriptor.content_checksum_flag() {
                        let mut chksum = [0u8; 4];
                        Read::read_exact(input, &mut chksum).map_err(err::FailedToReadChecksum)?;
                        state.bytes_read_counter += 4;
                        state.check_sum = Some(u32::from_le_bytes(chksum));
                        // Mirror the general path: seed the scratch hash so
                        // `verify_content_checksum` / `get_calculated_checksum`
                        // read the digest. Skipped under `ContentChecksum::None`.
                        #[cfg(feature = "hash")]
                        if self.content_checksum != ContentChecksum::None {
                            use core::hash::Hasher;
                            let mut h = twox_hash::XxHash64::with_seed(0);
                            h.write(&output[..n]);
                            match &mut state.decoder_scratch {
                                DecoderScratchKind::Flat(s) => s.buffer.hash = h,
                                DecoderScratchKind::Ring(s) => s.buffer.hash = h,
                            }
                        }
                    }
                    #[cfg(all(feature = "lsm", feature = "hash"))]
                    if self.per_block_checksums_enabled {
                        use core::hash::Hasher;
                        let mut h = twox_hash::XxHash64::with_seed(0);
                        h.write(&output[..n]);
                        self.computed_block_checksums.push(h.finish() as u32);
                    }
                    state.frame_finished = true;
                    return Ok(n);
                }
            }
        }

        // Borrow persistent fields out of whichever scratch variant
        // `init` produced (Flat for single_segment, Ring for
        // multi-segment) — both expose the same HUF/FSE/Vec
        // fields; only `buffer` differs and we don't use that here.
        // Macro-style binding avoids the closure / generic
        // gymnastics of returning multiple `&mut` from a match arm.
        // Resolve the dictionary borrow for this frame BEFORE taking the
        // `&mut` field borrows below — `active_dict` is a disjoint field, so
        // the shared borrow coexists with the mutable scratch borrows. It is
        // threaded as a call-scoped argument into every `Dict`-sourced read
        // (the direct path's `repeat_from_dict` ext-dict slow path), mirroring
        // C's per-frame pointer hand-off with zero refcount churn.
        // Only expose the held dictionary while THIS frame is dict-backed
        // (`using_dict` is set per dict-apply, cleared on reset). A reused
        // decoder keeps `active_dict` across a no-dict frame for the
        // `ptr::eq` reuse-skip, so it must be gated here or a stray
        // out-of-window offset on a dictless frame would resolve against the
        // stale dictionary content instead of erroring.
        let dict_ref = if state.using_dict.is_some() {
            state.active_dict.as_ref().map(|h| h.as_dict())
        } else {
            None
        };
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
        // Frame-level XXH64, accumulated PER BLOCK right after each block
        // decodes — the bytes are still cache-resident then. The previous
        // shape hashed the whole output once after the loop, which re-read
        // the entire frame cold: a full extra memory pass that the
        // reference implementation does not make (it hashes incrementally
        // per block). Invisible on outputs that fit L3, ~1.14x wall on a
        // 100 MiB all-raw decode and the dominant CI gap on
        // bandwidth-limited hosts.
        #[cfg(feature = "hash")]
        let mut running_hash: Option<twox_hash::XxHash64> =
            if state.frame_header.descriptor.content_checksum_flag()
                && self.content_checksum != ContentChecksum::None
            {
                Some(twox_hash::XxHash64::with_seed(0))
            } else {
                None
            };
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
                dict_ref,
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
                // executor (upstream zstd-inline path) or the match-repeat
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
            // Hash this block's freshly-written bytes while they are hot
            // (see `running_hash` above). `tail()` is the physical write
            // cursor: `drop_to_window_size` below only advances the head,
            // so `[prev_tail, tail)` is exactly this block's output.
            #[cfg(feature = "hash")]
            if let Some(hasher) = running_hash.as_mut() {
                use core::hash::Hasher;
                hasher.write(direct.buffer.buffer_ref().written_since(produced as usize));
            }
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
        // `direct`'s last use is in the decode loop above; NLL therefore
        // releases its `&mut output` borrow before here, freeing `output` for
        // the hash re-borrow below. No explicit `drop(direct)` is needed:
        // `DirectScratch` now holds only borrowed dict POINTERS (not an owned
        // `Arc`), so it is not a `Drop` type whose glue would hold the borrow
        // to end-of-scope.
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
        if let Some(hasher) = running_hash {
            // Propagate the per-block-accumulated hasher state (see the
            // `running_hash` rationale above the loop) so the frame-tail
            // XXH64 check and `get_calculated_checksum()` read the digest.
            // `running_hash` is `None` for flag-off frames or
            // `ContentChecksum::None` — nothing to verify there, and
            // `get_calculated_checksum()` returns `None`, matching the skip.
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
mod tests;
