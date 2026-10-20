//! Streaming decoder that runs entirely in caller-owned memory.
//!
//! [`WorkspaceDecoder`] decodes a stream of zstd frames chunk by chunk without
//! touching the heap: the window, the entropy tables, the literals and the
//! staging for blocks split across input chunks all live in one byte slice the
//! caller supplies. [`workspace_bytes`] says how large that slice must be for a
//! given window limit, so the caller can admit the memory in its own accounting
//! before decoding starts, and keep the charge for as long as the slice lives.
//!
//! Every step is bounded three ways, all chosen by the caller: the window a
//! frame may declare and the total output ([`DecodeLimits`]), and the work a
//! step may do ([`WorkBudget`]). A refusal is reported apart from malformed
//! input, an unsupported feature, and input that ends mid-frame, and it ends
//! the stream: every later step fails with [`WorkspaceDecodeError::Failed`]
//! until [`WorkspaceDecoder::reset`].
//!
//! Frames that name a dictionary are refused with
//! [`Unsupported::Dictionary`]; this decoder keeps no dictionary registry.
//! With the `legacy` feature, v0.5-v0.7 frames are refused with
//! `Unsupported::LegacyFrame`, since their decoders allocate.
//!
//! The decoding itself is the same code the other decoders run, so output is
//! identical to theirs.
//!
//! # Examples
//!
//! ```
//! use structured_zstd::decoding::workspace::{
//!     workspace_bytes, DecodeLimits, StepState, WorkBudget, WorkspaceDecoder,
//! };
//! use structured_zstd::encoding::{compress_slice_to_vec, CompressionLevel};
//!
//! let data = b"the same words, the same words, the same words".repeat(100);
//! let compressed = compress_slice_to_vec(&data, CompressionLevel::Default);
//!
//! let limits = DecodeLimits::new(1 << 20).with_max_output(1 << 20);
//! let mut workspace = vec![0u8; workspace_bytes(limits.max_window_size).unwrap()];
//! let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).unwrap();
//! let mut work = WorkBudget::new(1 << 24);
//!
//! let mut decoded = Vec::new();
//! let mut out = [0u8; 512];
//! let mut input = &compressed[..];
//! loop {
//!     // Feed at most 100 bytes at a time; say when nothing more will come.
//!     let chunk = &input[..input.len().min(100)];
//!     let last = chunk.len() == input.len();
//!     let progress = decoder.step(chunk, last, &mut out, &mut work).unwrap();
//!     decoded.extend_from_slice(&out[..progress.produced]);
//!     input = &input[progress.consumed..];
//!     if progress.state == StepState::Done {
//!         break;
//!     }
//! }
//! assert_eq!(decoded, data);
//! ```

use super::block_decoder::{self, BlockDecoder};
use super::buffer_backend::{BufferBackend, WILDCOPY_OVERLENGTH};
use super::decode_buffer::DecodeBuffer;
use super::errors::{
    BlockHeaderReadError, DecodeBlockContentError, FrameHeaderError, ReadFrameHeaderError,
};
use super::frame;
use super::ringbuffer::RingBuffer;
use super::scratch::{ByteScratch, FSEScratch, HuffmanScratch, Workspace, WorkspaceRef};
use super::user_slice_buf::UserSliceBackend;
use crate::blocks::block::{BlockHeader, BlockType};
use crate::common::{MAGIC_NUM, MAX_DECODER_WINDOW_SIZE, MIN_WINDOW_SIZE};
use core::marker::PhantomData;
use core::ptr::NonNull;

/// Work units charged before a compressed block decodes, on top of its input
/// bytes and its output bound: the most its entropy-table builds can cost
/// (one Huffman table of `1 << 12` entries and the literal-length,
/// match-length and offset tables at `1 << 9`, `1 << 9` and `1 << 8`).
pub const COMPRESSED_BLOCK_UNITS: u64 = (1 << crate::huff0::MAX_MAX_NUM_BITS)
    + (1 << super::sequence_section_decoder::LL_MAX_LOG)
    + (1 << super::sequence_section_decoder::ML_MAX_LOG)
    + (1 << super::sequence_section_decoder::OF_MAX_LOG);

/// Largest frame header: magic, descriptor, window byte, 4-byte dictionary id
/// and 8-byte content size (RFC 8878 3.1.1.1).
const MAX_FRAME_HEADER: usize = 18;

/// Alignment the entropy tables need at the start of the workspace.
const TABLE_ALIGN: usize = {
    let fse = core::mem::align_of::<FSEScratch>();
    let huf = core::mem::align_of::<HuffmanScratch>();
    if fse > huf { fse } else { huf }
};

/// Bytes of workspace a [`WorkspaceDecoder`] needs to decode frames whose
/// window is at most `max_window_size`.
///
/// The figure covers every byte the decoder writes: the window with room for
/// the block being decoded, the entropy tables, the literals, the staging for a
/// block split across input chunks, and alignment slack, so any sub-slice of
/// that length works whatever its address.
///
/// # Errors
///
/// [`WorkspaceConfigError::WindowLimitOutOfRange`] when `max_window_size` lies
/// outside 1 KiB ..= [`MAX_DECODER_WINDOW_SIZE`],
/// and [`WorkspaceConfigError::SizeOverflow`] when the size does not fit in
/// `usize` on this target.
///
/// # Examples
///
/// ```
/// use structured_zstd::decoding::workspace::workspace_bytes;
/// let small = workspace_bytes(64 * 1024).unwrap();
/// let large = workspace_bytes(8 << 20).unwrap();
/// assert!(small < large);
/// assert!(workspace_bytes(100).is_err());
/// ```
pub fn workspace_bytes(max_window_size: u64) -> Result<usize, WorkspaceConfigError> {
    Layout::for_window(max_window_size).map(|layout| layout.total)
}

/// Where each part of the workspace sits, relative to its aligned start.
struct Layout {
    huf_offset: usize,
    tables_len: usize,
    ring_cap: usize,
    literals_len: usize,
    staging_len: usize,
    total: usize,
}

impl Layout {
    fn for_window(max_window_size: u64) -> Result<Layout, WorkspaceConfigError> {
        use WorkspaceConfigError::SizeOverflow;
        if !(MIN_WINDOW_SIZE..=MAX_DECODER_WINDOW_SIZE).contains(&max_window_size) {
            return Err(WorkspaceConfigError::WindowLimitOutOfRange {
                requested: max_window_size,
            });
        }
        let window = usize::try_from(max_window_size).map_err(|_| SizeOverflow)?;
        let block_max = block_decoder::block_maximum(window);
        let huf_offset = core::mem::size_of::<FSEScratch>()
            .next_multiple_of(core::mem::align_of::<HuffmanScratch>());
        let tables_len = huf_offset + core::mem::size_of::<HuffmanScratch>();
        // The window, plus two block maxima: a block's matches stop at one
        // block past the window (the per-block ceiling), and literals copied
        // after its last match can add up to one more before the block-size
        // check rejects it. One more slot is the ring's empty/full sentinel.
        let ring_cap = window
            .checked_add(2 * block_max)
            .and_then(|n| n.checked_add(1))
            .ok_or(SizeOverflow)?;
        let literals_len = block_max + WILDCOPY_OVERLENGTH;
        let staging_len = block_max;
        let total = [
            tables_len,
            ring_cap,
            WILDCOPY_OVERLENGTH,
            literals_len,
            staging_len,
        ]
        .iter()
        .try_fold(TABLE_ALIGN - 1, |sum, &part| sum.checked_add(part))
        .ok_or(SizeOverflow)?;
        Ok(Layout {
            huf_offset,
            tables_len,
            ring_cap,
            literals_len,
            staging_len,
            total,
        })
    }
}

/// Limits a [`WorkspaceDecoder`] holds every frame of the stream to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Largest window a frame may declare. Also sizes the workspace; see
    /// [`workspace_bytes`].
    pub max_window_size: u64,
    /// Most decompressed bytes the whole stream may produce.
    pub max_output: u64,
}

impl DecodeLimits {
    /// Limits with the given window ceiling and no output limit.
    pub const fn new(max_window_size: u64) -> Self {
        Self {
            max_window_size,
            max_output: u64::MAX,
        }
    }

    /// The same limits with the total output capped at `max_output` bytes.
    pub const fn with_max_output(self, max_output: u64) -> Self {
        Self { max_output, ..self }
    }
}

/// Work a caller allows its decode steps, in units charged before the work
/// they pay for runs.
///
/// One unit is charged per input byte consumed (frame and block headers,
/// block contents, skippable frames, checksums), and one per byte a block may
/// produce, before it is produced: a Raw block's bytes as each part of it is
/// copied, an RLE block's stated size, and a compressed block's frame block
/// maximum plus [`COMPRESSED_BLOCK_UNITS`] for its table builds. A charge the
/// budget cannot cover refuses the step before that work starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkBudget {
    remaining: u64,
}

impl WorkBudget {
    /// A budget of `units`.
    pub const fn new(units: u64) -> Self {
        Self { remaining: units }
    }

    /// Units still available.
    pub const fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Add `units` to the budget, stopping at `u64::MAX`, which is already
    /// more work than any stream can ask for.
    pub fn add(&mut self, units: u64) {
        self.remaining = self.remaining.saturating_add(units);
    }

    fn charge(&mut self, units: u64) -> Result<(), WorkspaceDecodeError> {
        match self.remaining.checked_sub(units) {
            Some(left) => {
                self.remaining = left;
                Ok(())
            }
            None => Err(WorkspaceDecodeError::Limit(LimitExceeded::Work {
                needed: units,
                remaining: self.remaining,
            })),
        }
    }
}

/// What one [`WorkspaceDecoder::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Input bytes consumed; the next step starts after them.
    pub consumed: usize,
    /// Bytes written to the front of the output slice.
    pub produced: usize,
    /// What the decoder needs next.
    pub state: StepState,
}

/// Why a [`WorkspaceDecoder::step`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StepState {
    /// Every input byte was consumed; call again with more, or with the
    /// final-input flag set once there is no more.
    NeedInput,
    /// The output slice is full; call again with room.
    NeedOutput,
    /// The input was final and every frame in it is decoded and delivered.
    Done,
}

/// A workspace size or limit [`workspace_bytes`] or [`WorkspaceDecoder::new`]
/// refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkspaceConfigError {
    /// The window limit lies outside 1 KiB ..=
    /// [`MAX_DECODER_WINDOW_SIZE`].
    WindowLimitOutOfRange {
        /// The limit asked for.
        requested: u64,
    },
    /// The workspace size does not fit in `usize` on this target.
    SizeOverflow,
    /// The workspace slice is shorter than [`workspace_bytes`] asks for.
    WorkspaceTooSmall {
        /// Bytes the limits require.
        needed: usize,
        /// Bytes supplied.
        provided: usize,
    },
}

impl core::fmt::Display for WorkspaceConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WindowLimitOutOfRange { requested } => write!(
                f,
                "window limit {requested} lies outside {MIN_WINDOW_SIZE}..={MAX_DECODER_WINDOW_SIZE}"
            ),
            Self::SizeOverflow => write!(f, "workspace size does not fit in usize"),
            Self::WorkspaceTooSmall { needed, provided } => write!(
                f,
                "workspace of {provided} bytes is smaller than the {needed} the limits require"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for WorkspaceConfigError {}

/// Why a [`WorkspaceDecoder::step`] failed. Every variant ends the stream:
/// later steps return [`Self::Failed`] until [`WorkspaceDecoder::reset`].
#[derive(Debug)]
#[non_exhaustive]
pub enum WorkspaceDecodeError {
    /// A caller-chosen limit refused the stream.
    Limit(LimitExceeded),
    /// The input is not a valid zstd stream.
    Malformed(MalformedInput),
    /// The input is valid but needs something this decoder does not do.
    Unsupported(Unsupported),
    /// The input was marked final in the middle of a frame.
    Truncated,
    /// An earlier step failed; call [`WorkspaceDecoder::reset`] first.
    Failed,
}

/// A limit from [`DecodeLimits`] or [`WorkBudget`] that refused the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitExceeded {
    /// A frame declared a window above [`DecodeLimits::max_window_size`].
    Window {
        /// The window the frame declared.
        requested: u64,
        /// The limit.
        limit: u64,
    },
    /// The stream would produce more than [`DecodeLimits::max_output`].
    Output {
        /// The limit.
        limit: u64,
    },
    /// The next unit of work costs more than the budget has left.
    Work {
        /// Units the refused work costs.
        needed: u64,
        /// Units the budget had.
        remaining: u64,
    },
}

/// Something this decoder does not do, met in otherwise valid input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unsupported {
    /// The frame needs dictionary `id`, and this decoder takes none.
    Dictionary {
        /// The dictionary id the frame header names.
        id: u32,
    },
    /// A v0.5, v0.6 or v0.7 frame, which the owned decoders read and this
    /// one does not: their decoders allocate. Upstream's static context
    /// refuses them likewise.
    #[cfg(feature = "legacy")]
    LegacyFrame {
        /// The format of the frame.
        version: crate::decoding::LegacyVersion,
    },
}

/// How the input breaks the zstd format.
#[derive(Debug)]
#[non_exhaustive]
pub enum MalformedInput {
    /// A frame header, or the magic number before it, is invalid.
    FrameHeader(ReadFrameHeaderError),
    /// The frame header declares an invalid window.
    Window(FrameHeaderError),
    /// A block header is invalid.
    BlockHeader(BlockHeaderReadError),
    /// A block states a size above its frame's block maximum
    /// (RFC 8878 3.1.1.2.4).
    BlockTooLarge {
        /// The size the block header states.
        size: usize,
        /// The frame's block maximum.
        maximum: usize,
    },
    /// A block's content does not decode.
    Block(DecodeBlockContentError),
    /// The frame's blocks do not add up to its declared content size.
    ContentSize {
        /// The size the frame header declares.
        declared: u64,
        /// The size its blocks produce (so far, when they overrun it).
        produced: u64,
    },
    /// The content checksum does not match the decoded output.
    Checksum {
        /// The checksum the frame carries.
        expected: u32,
        /// The low 32 bits of the XXH64 of the decoded frame.
        calculated: u32,
    },
}

impl core::fmt::Display for WorkspaceDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Limit(limit) => match limit {
                LimitExceeded::Window { requested, limit } => write!(
                    f,
                    "frame window of {requested} bytes exceeds the {limit}-byte limit"
                ),
                LimitExceeded::Output { limit } => {
                    write!(f, "output exceeds the {limit}-byte limit")
                }
                LimitExceeded::Work { needed, remaining } => write!(
                    f,
                    "work of {needed} units exceeds the {remaining} remaining"
                ),
            },
            Self::Malformed(malformed) => match malformed {
                MalformedInput::FrameHeader(e) => write!(f, "invalid frame header: {e}"),
                MalformedInput::Window(e) => write!(f, "invalid frame window: {e}"),
                MalformedInput::BlockHeader(e) => write!(f, "invalid block header: {e}"),
                MalformedInput::BlockTooLarge { size, maximum } => write!(
                    f,
                    "block of {size} bytes exceeds the frame's block maximum of {maximum}"
                ),
                MalformedInput::Block(e) => write!(f, "invalid block: {e}"),
                MalformedInput::ContentSize { declared, produced } => write!(
                    f,
                    "frame declares {declared} bytes but its blocks produce {produced}"
                ),
                MalformedInput::Checksum {
                    expected,
                    calculated,
                } => write!(
                    f,
                    "content checksum {calculated:#010x} does not match the stored {expected:#010x}"
                ),
            },
            Self::Unsupported(Unsupported::Dictionary { id }) => {
                write!(
                    f,
                    "frame needs dictionary {id}, which this decoder does not take"
                )
            }
            #[cfg(feature = "legacy")]
            Self::Unsupported(Unsupported::LegacyFrame { version }) => write!(
                f,
                "v0.{} frame, which this decoder does not read",
                version.minor()
            ),
            Self::Truncated => write!(f, "input ended in the middle of a frame"),
            Self::Failed => write!(f, "an earlier step failed; reset the decoder"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for WorkspaceDecodeError {}

/// A fixed region of the workspace standing in for the `Vec` the owned
/// decoders keep for literals.
///
/// A push past the end is dropped but still counted, so the literals decoder's
/// own count check rejects the block as it would with a growable buffer. Every
/// other write is sized by the caller below the region's length; the asserts
/// are what keeps a broken size argument from writing outside it.
struct SliceScratch<'w> {
    buf: &'w mut [u8],
    len: usize,
}

impl ByteScratch for SliceScratch<'_> {
    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    fn clear(&mut self) {
        self.len = 0;
    }
    #[inline(always)]
    fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
    #[inline(always)]
    fn reserve(&mut self, additional: usize) {
        assert!(
            self.len
                .checked_add(additional)
                .is_some_and(|end| end <= self.buf.len()),
            "literals exceed the workspace region sized for a block"
        );
    }
    #[inline(always)]
    fn push(&mut self, byte: u8) {
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = byte;
        }
        self.len += 1;
    }
    fn resize(&mut self, new_len: usize, value: u8) {
        if new_len > self.len {
            self.buf[self.len..new_len].fill(value);
        }
        self.len = new_len;
    }
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }
    #[inline(always)]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.buf.as_mut_ptr()
    }
    #[inline(always)]
    unsafe fn set_len(&mut self, new_len: usize) {
        assert!(new_len <= self.buf.len());
        self.len = new_len;
    }
    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len.min(self.buf.len())]
    }
    #[inline(always)]
    fn as_mut_slice(&mut self) -> &mut [u8] {
        let len = self.len.min(self.buf.len());
        &mut self.buf[..len]
    }
}

/// The current frame's window over the workspace's window region, chosen per
/// frame as `FrameDecoder` chooses its backend: a single-segment frame's whole
/// content is its window, so it decodes linearly into the region, the shape
/// `decode_all` writes into a caller's slice; any other frame decodes into a
/// ring.
enum Window<'w> {
    /// No frame has started since construction or reset.
    Idle,
    Ring(DecodeBuffer<RingBuffer>),
    Linear(DecodeBuffer<UserSliceBackend<'w>>),
}

/// Run `$body` with `$buffer` bound to the active window's buffer, or yield
/// `$idle` when no frame has started.
macro_rules! with_window {
    ($window:expr, $buffer:ident => $body:expr, idle => $idle:expr) => {
        match $window {
            Window::Ring($buffer) => $body,
            Window::Linear($buffer) => $body,
            Window::Idle => $idle,
        }
    };
}

/// Output bytes `buffer` has produced this frame. The linear backend's inline
/// sequence path skips the running counter, so its write cursor counts there;
/// the ring keeps the counter, its cursor wrapping.
fn produced_so_far<B: BufferBackend>(buffer: &DecodeBuffer<B>) -> u64 {
    if B::INLINE_EXEC_MAINTAINS_OUTPUT_COUNTER {
        buffer.total_output()
    } else {
        buffer.buffer_ref().tail() as u64
    }
}

/// The state the shared block decoder works on for one block: the tables and
/// literals in the workspace, and the active window's buffer.
struct BlockScratch<'a, 'w, B: BufferBackend> {
    huf: &'a mut HuffmanScratch,
    fse: &'a mut FSEScratch,
    buffer: &'a mut DecodeBuffer<B>,
    offset_hist: &'a mut [u32; 3],
    literals: &'a mut SliceScratch<'w>,
    /// The Read-sourced block path's buffer; this decoder stages blocks
    /// itself and hands them over as slices, so it stays empty.
    block_content: &'a mut SliceScratch<'w>,
}

impl<'w, B: BufferBackend> Workspace for BlockScratch<'_, 'w, B> {
    type Backend = B;
    type Bytes = SliceScratch<'w>;
    fn split(&mut self) -> WorkspaceRef<'_, B, SliceScratch<'w>> {
        WorkspaceRef {
            huf: &mut *self.huf,
            fse: &mut *self.fse,
            buffer: &mut *self.buffer,
            offset_hist: &mut *self.offset_hist,
            literals_buffer: &mut *self.literals,
            block_content_buffer: &mut *self.block_content,
        }
    }
}

/// Decode one block whose whole content is `content`, returning the bytes it
/// produced.
fn run_block<B: BufferBackend>(
    decoder: &mut BlockDecoder,
    scratch: &mut BlockScratch<'_, '_, B>,
    header: &BlockHeader,
    mut content: &[u8],
) -> Result<u64, DecodeBlockContentError> {
    let before = produced_so_far(scratch.buffer);
    decoder.decode_block_content_from_slice(header, scratch, None, &mut content)?;
    Ok(produced_so_far(scratch.buffer) - before)
}

/// What the decoder reads next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// The 4-byte magic number of the next frame.
    Magic,
    /// The descriptor byte, which gives the header's length.
    Descriptor,
    /// The rest of the frame header.
    FrameHeader,
    /// The 4-byte length of a skippable frame.
    SkippableSize,
    /// The payload of a skippable frame.
    Skipping { remaining: u64 },
    /// A 3-byte block header.
    BlockHeader,
    /// A block's content.
    BlockBody(PendingBlock),
    /// The 4-byte content checksum.
    Checksum,
}

/// A block whose header was read, carried while its content arrives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PendingBlock {
    last_block: bool,
    block_type: BlockType,
    decompressed_size: u32,
    content_size: u32,
}

impl PendingBlock {
    fn header(self) -> BlockHeader {
        BlockHeader {
            last_block: self.last_block,
            block_type: self.block_type,
            decompressed_size: self.decompressed_size,
            content_size: self.content_size,
        }
    }
}

/// The frame being decoded.
#[derive(Clone, Copy)]
struct FrameState {
    declared_size: Option<u64>,
    has_checksum: bool,
    block_maximum: usize,
    produced: u64,
}

/// Allocation-free streaming zstd decoder over a caller-owned workspace.
///
/// See the [module documentation](self) for the contract and an example.
pub struct WorkspaceDecoder<'w> {
    huf: &'w mut HuffmanScratch,
    fse: &'w mut FSEScratch,
    offset_hist: [u32; 3],
    literals: SliceScratch<'w>,
    block_content: SliceScratch<'w>,
    window: Window<'w>,
    /// The workspace region every frame's window is laid over, held as a
    /// pointer because frames alternate between a ring (which addresses it by
    /// pointer) and a linear buffer (which borrows it), never both at once.
    region: NonNull<u8>,
    /// `region`'s length: the ring's capacity plus the copiers' slack.
    region_len: usize,
    ring_cap: usize,
    _region: PhantomData<&'w mut [u8]>,
    staging: &'w mut [u8],
    staged: usize,
    block_decoder: BlockDecoder,
    kernel: crate::cpu_kernel::CpuKernelTag,
    limits: DecodeLimits,
    phase: Phase,
    header: [u8; MAX_FRAME_HEADER],
    header_len: usize,
    header_target: usize,
    frame: FrameState,
    /// Every block of the current frame is decoded, so all of the window is
    /// output to deliver rather than history to keep.
    blocks_done: bool,
    stream_produced: u64,
    failed: bool,
}

// SAFETY: `region` points into the `&'w mut [u8]` workspace this decoder
// borrows exclusively for `'w` (`_region` records that borrow), and every other
// field is `Send`. Moving the decoder moves that exclusive borrow with it, as
// moving the `&mut [u8]` itself would.
unsafe impl Send for WorkspaceDecoder<'_> {}

impl<'w> WorkspaceDecoder<'w> {
    /// Build a decoder in `workspace`, which must hold at least
    /// [`workspace_bytes`]`(limits.max_window_size)` bytes. Nothing is
    /// allocated, now or later; the workspace stays borrowed for the decoder's
    /// life.
    ///
    /// # Errors
    ///
    /// [`WorkspaceConfigError`] when the window limit is out of range or the
    /// workspace is too small for it.
    pub fn new(
        workspace: &'w mut [u8],
        limits: DecodeLimits,
    ) -> Result<Self, WorkspaceConfigError> {
        let layout = Layout::for_window(limits.max_window_size)?;
        let provided = workspace.len();
        if provided < layout.total {
            return Err(WorkspaceConfigError::WorkspaceTooSmall {
                needed: layout.total,
                provided,
            });
        }
        // `layout.total` holds `TABLE_ALIGN - 1` bytes of slack, and the
        // offset a byte pointer needs to reach an alignment is below it.
        let skip = workspace.as_ptr().align_offset(TABLE_ALIGN);
        debug_assert!(skip < TABLE_ALIGN);
        let rest = &mut workspace[skip..];
        let (tables, rest) = rest.split_at_mut(layout.tables_len);
        let (ring, rest) = rest.split_at_mut(layout.ring_cap + WILDCOPY_OVERLENGTH);
        let (literals, rest) = rest.split_at_mut(layout.literals_len);
        let staging = &mut rest[..layout.staging_len];

        let tables = tables.as_mut_ptr();
        let fse_ptr = tables.cast::<FSEScratch>();
        // SAFETY: `tables` starts at a `TABLE_ALIGN` boundary and spans
        // `tables_len` bytes: the FSE scratch at its start and the Huffman
        // scratch at `huf_offset`, a multiple of its alignment, both in bounds.
        // Each is fully initialised in place before a reference to it exists,
        // and the bytes are reached through nothing else while `workspace`
        // stays borrowed for `'w`. Neither type owns heap memory under this
        // feature, so never running their destructors leaks nothing.
        let (fse, huf) = unsafe {
            let huf_ptr = tables.add(layout.huf_offset).cast::<HuffmanScratch>();
            FSEScratch::init_in_place(fse_ptr);
            HuffmanScratch::init_in_place(huf_ptr);
            (&mut *fse_ptr, &mut *huf_ptr)
        };
        let region_len = ring.len();
        let region = NonNull::from(ring).cast::<u8>();

        let kernel = crate::cpu_kernel::detect_cpu_kernel();
        Ok(WorkspaceDecoder {
            huf,
            fse,
            offset_hist: [1, 4, 8],
            literals: SliceScratch {
                buf: literals,
                len: 0,
            },
            block_content: SliceScratch {
                buf: &mut [],
                len: 0,
            },
            window: Window::Idle,
            region,
            region_len,
            ring_cap: layout.ring_cap,
            _region: PhantomData,
            staging,
            staged: 0,
            block_decoder: block_decoder::with_kernel(kernel),
            kernel,
            limits,
            phase: Phase::Magic,
            header: [0; MAX_FRAME_HEADER],
            header_len: 0,
            header_target: 4,
            frame: FrameState {
                declared_size: None,
                has_checksum: false,
                block_maximum: 0,
                produced: 0,
            },
            blocks_done: false,
            stream_produced: 0,
            failed: false,
        })
    }

    /// Start a new stream: forget the current one, including a failure, and
    /// expect a frame's magic number next. The workspace is reused as it is.
    pub fn reset(&mut self) {
        self.window = Window::Idle;
        self.literals.clear();
        self.staged = 0;
        self.block_decoder = block_decoder::with_kernel(self.kernel);
        self.phase = Phase::Magic;
        self.header_len = 0;
        self.header_target = 4;
        self.blocks_done = false;
        self.stream_produced = 0;
        self.failed = false;
    }

    /// Decompressed bytes the stream has produced so far, delivered or not.
    pub fn total_output(&self) -> u64 {
        self.stream_produced
    }

    /// Decode from `input` into `output` until one of them runs out or the
    /// stream ends.
    ///
    /// `input_is_final` says no input follows this slice. Without it, running
    /// out of input asks for more ([`StepState::NeedInput`]); with it, input
    /// that ends between frames finishes the stream ([`StepState::Done`]) and
    /// input that ends inside one is [`WorkspaceDecodeError::Truncated`].
    /// Unconsumed input must be passed again, first, on the next call.
    ///
    /// Concatenated frames decode one after another, and skippable frames are
    /// consumed without output.
    ///
    /// # Errors
    ///
    /// See [`WorkspaceDecodeError`]. Any error ends the stream; the output
    /// already written by this call is then not counted in a [`Progress`].
    pub fn step(
        &mut self,
        input: &[u8],
        input_is_final: bool,
        output: &mut [u8],
        work: &mut WorkBudget,
    ) -> Result<Progress, WorkspaceDecodeError> {
        if self.failed {
            return Err(WorkspaceDecodeError::Failed);
        }
        let result = self.step_inner(input, input_is_final, output, work);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn step_inner(
        &mut self,
        input: &[u8],
        input_is_final: bool,
        output: &mut [u8],
        work: &mut WorkBudget,
    ) -> Result<Progress, WorkspaceDecodeError> {
        let mut consumed = 0;
        let mut produced = 0;
        loop {
            produced += self.deliver(&mut output[produced..]);
            if self.pending() != 0 {
                return Ok(Progress {
                    consumed,
                    produced,
                    state: StepState::NeedOutput,
                });
            }
            let rest = &input[consumed..];
            match self.phase {
                Phase::Skipping { remaining } => {
                    if remaining == 0 {
                        self.expect_frame();
                        continue;
                    }
                    if rest.is_empty() {
                        return self.out_of_input(input_is_final, consumed, produced);
                    }
                    let take = rest
                        .len()
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    work.charge(take as u64)?;
                    consumed += take;
                    self.phase = Phase::Skipping {
                        remaining: remaining - take as u64,
                    };
                }
                Phase::BlockBody(block) if block.block_type == BlockType::Raw => {
                    // A Raw block's content is its output: it goes straight
                    // into the window as it arrives, never through staging.
                    // `staged` counts the bytes written so far.
                    let remaining = block.content_size as usize - self.staged;
                    if remaining == 0 {
                        self.finish_block(block)?;
                        continue;
                    }
                    if rest.is_empty() {
                        return self.out_of_input(input_is_final, consumed, produced);
                    }
                    let take = rest.len().min(remaining);
                    // One unit for the input byte, one for the output byte.
                    work.charge(2 * take as u64)?;
                    self.push_raw(&rest[..take])?;
                    self.staged += take;
                    consumed += take;
                }
                Phase::BlockBody(block) => {
                    let size = block.content_size as usize;
                    if self.staged == 0 && rest.len() >= size {
                        work.charge(size as u64)?;
                        consumed += size;
                        self.decode_block(block, &rest[..size], work)?;
                        continue;
                    }
                    if rest.is_empty() {
                        return self.out_of_input(input_is_final, consumed, produced);
                    }
                    let take = rest.len().min(size - self.staged);
                    work.charge(take as u64)?;
                    self.staging[self.staged..self.staged + take].copy_from_slice(&rest[..take]);
                    self.staged += take;
                    consumed += take;
                    if self.staged == size {
                        self.staged = 0;
                        // The staging region is a separate field from the
                        // state the block decoder writes, so it lends the
                        // content in place.
                        let staging = core::mem::take(&mut self.staging);
                        let decoded = self.decode_block(block, &staging[..size], work);
                        self.staging = staging;
                        decoded?;
                    }
                }
                _ => {
                    if self.header_len < self.header_target {
                        if rest.is_empty() {
                            return self.out_of_input(input_is_final, consumed, produced);
                        }
                        let take = rest.len().min(self.header_target - self.header_len);
                        work.charge(take as u64)?;
                        self.header[self.header_len..self.header_len + take]
                            .copy_from_slice(&rest[..take]);
                        self.header_len += take;
                        consumed += take;
                    }
                    if self.header_len == self.header_target {
                        self.header_complete()?;
                    }
                }
            }
        }
    }

    /// Input ran out. Only between frames does final input end the stream.
    fn out_of_input(
        &self,
        input_is_final: bool,
        consumed: usize,
        produced: usize,
    ) -> Result<Progress, WorkspaceDecodeError> {
        if !input_is_final {
            return Ok(Progress {
                consumed,
                produced,
                state: StepState::NeedInput,
            });
        }
        if self.phase == Phase::Magic && self.header_len == 0 {
            return Ok(Progress {
                consumed,
                produced,
                state: StepState::Done,
            });
        }
        Err(WorkspaceDecodeError::Truncated)
    }

    /// Decoded bytes not delivered yet, whether or not the frame is finished.
    fn pending(&self) -> usize {
        with_window!(&self.window, buffer => buffer.undelivered(), idle => 0)
    }

    fn deliver(&mut self, output: &mut [u8]) -> usize {
        if output.is_empty() {
            return 0;
        }
        let blocks_done = self.blocks_done;
        let drained = with_window!(&mut self.window, buffer => if blocks_done {
            buffer.read_all(output)
        } else {
            buffer.read_reporting_pending(output).map(|(read, _)| read)
        }, idle => Ok(0));
        // Draining into a slice cannot fail: the copy reports no error.
        drained.unwrap_or(0)
    }

    fn expect_frame(&mut self) {
        self.phase = Phase::Magic;
        self.header_len = 0;
        self.header_target = 4;
    }

    fn expect_header(&mut self, phase: Phase, target: usize) {
        self.phase = phase;
        self.header_target = target;
    }

    /// The bytes the current phase collects are all in `header`.
    fn header_complete(&mut self) -> Result<(), WorkspaceDecodeError> {
        use WorkspaceDecodeError::Malformed;
        match self.phase {
            Phase::Magic => {
                let magic = u32::from_le_bytes([
                    self.header[0],
                    self.header[1],
                    self.header[2],
                    self.header[3],
                ]);
                if (0x184D2A50..=0x184D2A5F).contains(&magic) {
                    self.expect_header(Phase::SkippableSize, 8);
                } else if magic == MAGIC_NUM {
                    self.expect_header(Phase::Descriptor, 5);
                } else {
                    #[cfg(feature = "legacy")]
                    if let Some(version) = crate::decoding::LegacyVersion::of(&magic.to_le_bytes())
                    {
                        return Err(WorkspaceDecodeError::Unsupported(
                            Unsupported::LegacyFrame { version },
                        ));
                    }
                    return Err(Malformed(MalformedInput::FrameHeader(
                        ReadFrameHeaderError::BadMagicNumber(magic),
                    )));
                }
            }
            Phase::SkippableSize => {
                let length = u32::from_le_bytes([
                    self.header[4],
                    self.header[5],
                    self.header[6],
                    self.header[7],
                ]);
                self.phase = Phase::Skipping {
                    remaining: u64::from(length),
                };
            }
            Phase::Descriptor => {
                let length = frame::measure_header_from_slice(&self.header[..5])
                    .map_err(|e| Malformed(MalformedInput::FrameHeader(e)))?;
                self.expect_header(Phase::FrameHeader, length);
            }
            Phase::FrameHeader => self.start_frame()?,
            Phase::BlockHeader => self.start_block()?,
            Phase::Checksum => {
                self.check_checksum()?;
                self.expect_frame();
            }
            Phase::Skipping { .. } | Phase::BlockBody(_) => {
                unreachable!("phases without a header buffer are handled by the step loop")
            }
        }
        Ok(())
    }

    fn start_frame(&mut self) -> Result<(), WorkspaceDecodeError> {
        use WorkspaceDecodeError::{Limit, Malformed};
        let mut source = &self.header[..self.header_target];
        let (header, _) = frame::read_frame_header_from_slice(&mut source, false)
            .map_err(|e| Malformed(MalformedInput::FrameHeader(e)))?;
        if let Some(id) = header.dictionary_id() {
            return Err(WorkspaceDecodeError::Unsupported(Unsupported::Dictionary {
                id,
            }));
        }
        let window = header
            .window_size()
            .map_err(|e| Malformed(MalformedInput::Window(e)))?;
        if window > self.limits.max_window_size {
            return Err(Limit(LimitExceeded::Window {
                requested: window,
                limit: self.limits.max_window_size,
            }));
        }
        let declared_size = header.fcs_declared().then(|| header.frame_content_size());
        if let Some(declared) = declared_size
            && self
                .stream_produced
                .checked_add(declared)
                .is_none_or(|total| total > self.limits.max_output)
        {
            return Err(Limit(LimitExceeded::Output {
                limit: self.limits.max_output,
            }));
        }
        // Fits `usize`: it is at most the limit, which `Layout` converted.
        let window = window as usize;
        let has_checksum = header.descriptor.content_checksum_flag();

        // End the previous frame's view of the region before laying out the
        // next one, so the two never coexist.
        self.window = Window::Idle;
        self.window = if header.descriptor.single_segment_flag() {
            // SAFETY: `region` is `region_len` initialised bytes borrowed for
            // `'w`, and no other view of it is alive: the previous window was
            // dropped just above.
            let region =
                unsafe { core::slice::from_raw_parts_mut(self.region.as_ptr(), self.region_len) };
            Window::Linear(DecodeBuffer::from_backend(
                UserSliceBackend::from_slice(region),
                window,
            ))
        } else {
            // SAFETY: as above; the ring addresses `ring_cap` bytes of the
            // region plus the copiers' slack, `region_len` in all, and
            // `ring_cap >= 1`.
            let ring = unsafe { RingBuffer::from_borrowed(self.region, self.ring_cap) };
            Window::Ring(DecodeBuffer::from_backend(ring, window))
        };
        with_window!(&mut self.window, buffer => {
            buffer.set_declared_content(declared_size);
            #[cfg(feature = "hash")]
            buffer.set_compute_hash(has_checksum);
        }, idle => {});
        self.offset_hist = [1, 4, 8];
        self.literals.clear();
        super::scratch::reset_entropy(self.huf, self.fse);

        self.frame = FrameState {
            declared_size,
            has_checksum,
            block_maximum: block_decoder::block_maximum(window),
            produced: 0,
        };
        self.blocks_done = false;
        self.header_len = 0;
        self.expect_header(Phase::BlockHeader, 3);
        Ok(())
    }

    fn start_block(&mut self) -> Result<(), WorkspaceDecodeError> {
        use WorkspaceDecodeError::Malformed;
        let (header, _) = self
            .block_decoder
            .read_block_header(&self.header[..3])
            .map_err(|e| Malformed(MalformedInput::BlockHeader(e)))?;
        // RFC 8878 3.1.1.2.4: Block_Size is at most Block_Maximum_Size for
        // every block type; for RLE it is the regenerated size.
        let size = match header.block_type {
            BlockType::RLE => header.decompressed_size,
            _ => header.content_size,
        } as usize;
        let maximum = self.frame.block_maximum;
        if size > maximum {
            return Err(Malformed(MalformedInput::BlockTooLarge { size, maximum }));
        }
        self.header_len = 0;
        self.staged = 0;
        self.phase = Phase::BlockBody(PendingBlock {
            last_block: header.last_block,
            block_type: header.block_type,
            decompressed_size: header.decompressed_size,
            content_size: header.content_size,
        });
        Ok(())
    }

    /// Decode one block whose whole content is `content`, charging its work
    /// first and holding its output to the limits.
    fn decode_block(
        &mut self,
        block: PendingBlock,
        content: &[u8],
        work: &mut WorkBudget,
    ) -> Result<(), WorkspaceDecodeError> {
        use WorkspaceDecodeError::{Limit, Malformed};
        let output_bound = match block.block_type {
            BlockType::Compressed => self.frame.block_maximum as u64 + COMPRESSED_BLOCK_UNITS,
            _ => u64::from(block.decompressed_size),
        };
        work.charge(output_bound)?;
        let output_limit = Limit(LimitExceeded::Output {
            limit: self.limits.max_output,
        });
        if block.block_type != BlockType::Compressed
            && self
                .stream_produced
                .checked_add(u64::from(block.decompressed_size))
                .is_none_or(|total| total > self.limits.max_output)
        {
            return Err(output_limit);
        }

        let header = block.header();
        let produced = with_window!(&mut self.window, buffer => run_block(
            &mut self.block_decoder,
            &mut BlockScratch {
                huf: &mut *self.huf,
                fse: &mut *self.fse,
                buffer,
                offset_hist: &mut self.offset_hist,
                literals: &mut self.literals,
                block_content: &mut self.block_content,
            },
            &header,
            content,
        ), idle => unreachable!("a block is decoded only inside a started frame"))
        .map_err(|e| Malformed(MalformedInput::Block(e)))?;

        self.stream_produced += produced;
        self.frame.produced += produced;
        if self.stream_produced > self.limits.max_output {
            return Err(output_limit);
        }
        if let Some(declared) = self.frame.declared_size
            && self.frame.produced > declared
        {
            return Err(Malformed(MalformedInput::ContentSize {
                declared,
                produced: self.frame.produced,
            }));
        }
        self.finish_block(block)
    }

    /// Append the next part of a Raw block to the window, after holding it to
    /// the output limit and to the frame's declared size.
    fn push_raw(&mut self, bytes: &[u8]) -> Result<(), WorkspaceDecodeError> {
        use crate::cpu_kernel::{BaselineKernel, CpuKernelTag, ScalarKernel};
        use WorkspaceDecodeError::{Limit, Malformed};
        let len = bytes.len() as u64;
        let stream_total = self.stream_produced + len;
        if stream_total > self.limits.max_output {
            return Err(Limit(LimitExceeded::Output {
                limit: self.limits.max_output,
            }));
        }
        let frame_total = self.frame.produced + len;
        if let Some(declared) = self.frame.declared_size
            && frame_total > declared
        {
            return Err(Malformed(MalformedInput::ContentSize {
                declared,
                produced: frame_total,
            }));
        }
        // One copy per piece, under the kernel a Raw block takes elsewhere:
        // the scalar tier stays portable.
        let scalar = self.kernel == CpuKernelTag::Scalar;
        let pushed = with_window!(&mut self.window, buffer => if scalar {
            buffer.try_push::<ScalarKernel>(bytes)
        } else {
            buffer.try_push::<BaselineKernel>(bytes)
        }, idle => unreachable!("a block is decoded only inside a started frame"));
        // The block was held to the block maximum and the window region holds
        // a window and two block maxima, so only a frame that outruns its own
        // window can reach here.
        pushed.map_err(|_| {
            Malformed(MalformedInput::Block(
                DecodeBlockContentError::BackendOverflow {
                    step: BlockType::Raw,
                },
            ))
        })?;
        self.stream_produced = stream_total;
        self.frame.produced = frame_total;
        Ok(())
    }

    /// The block's content is all decoded: check a last block against the
    /// declared size and read what follows it.
    fn finish_block(&mut self, block: PendingBlock) -> Result<(), WorkspaceDecodeError> {
        if let Some(declared) = self.frame.declared_size
            && block.last_block
            && self.frame.produced != declared
        {
            return Err(WorkspaceDecodeError::Malformed(
                MalformedInput::ContentSize {
                    declared,
                    produced: self.frame.produced,
                },
            ));
        }
        self.staged = 0;
        self.header_len = 0;
        if !block.last_block {
            self.expect_header(Phase::BlockHeader, 3);
        } else {
            self.blocks_done = true;
            if self.frame.has_checksum {
                self.expect_header(Phase::Checksum, 4);
            } else {
                self.expect_frame();
            }
        }
        Ok(())
    }

    /// Compare the stored checksum with the decoded frame's. The step loop
    /// reaches this only once the whole frame was delivered, which is when the
    /// running digest covers all of it. Without the `hash` feature the
    /// checksum is read and not checked.
    fn check_checksum(&mut self) -> Result<(), WorkspaceDecodeError> {
        #[cfg(feature = "hash")]
        {
            use core::hash::Hasher;
            let expected = u32::from_le_bytes([
                self.header[0],
                self.header[1],
                self.header[2],
                self.header[3],
            ]);
            let calculated =
                with_window!(&self.window, buffer => buffer.hash.finish(), idle => 0) as u32;
            if expected != calculated {
                return Err(WorkspaceDecodeError::Malformed(MalformedInput::Checksum {
                    expected,
                    calculated,
                }));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
