//! The caller-workspace decoder never allocates: from construction through
//! every block type, table changes, limits, malformed and truncated input,
//! reset and drop. A counting global allocator, armed on this thread only
//! around the decoder's use, proves it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use structured_zstd::decoding::workspace::{
    DecodeLimits, LimitExceeded, StepState, WorkBudget, WorkspaceDecodeError, WorkspaceDecoder,
    workspace_bytes,
};
use structured_zstd::encoding::{
    CompressionLevel, CompressionParameters, compress_slice_to_vec, compress_with_parameters,
};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

fn note_allocation() {
    if ARMED.with(Cell::get) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards every call to `System` unchanged; it only counts.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Run `f` and require that it allocated nothing on this thread.
fn without_allocating<R>(what: &str, f: impl FnOnce() -> R) -> R {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = f();
    ARMED.with(|armed| armed.set(false));
    let count = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(count, 0, "{what}: {count} allocations");
    result
}

fn text(len: usize) -> Vec<u8> {
    let words = ["alpha ", "beta ", "gamma ", "delta ", "manifest ", "chunk "];
    let mut out = Vec::with_capacity(len);
    let mut i = 0usize;
    while out.len() < len {
        out.extend_from_slice(words[(i * 7 + i / 13) % words.len()].as_bytes());
        out.extend_from_slice(&(i as u32).to_le_bytes()[..i % 3]);
        i += 1;
    }
    out.truncate(len);
    out
}

fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Decode `stream` in `chunk`-byte steps through a fixed output buffer,
/// comparing each delivered piece against `expected` in place.
fn decode_and_compare(
    decoder: &mut WorkspaceDecoder<'_>,
    stream: &[u8],
    expected: &[u8],
    chunk: usize,
    work: &mut WorkBudget,
) -> Result<(), WorkspaceDecodeError> {
    let mut out = [0u8; 1500];
    let mut pos = 0;
    let mut matched = 0;
    loop {
        let end = (pos + chunk).min(stream.len());
        let progress = decoder.step(&stream[pos..end], end == stream.len(), &mut out, work)?;
        let piece = &out[..progress.produced];
        assert_eq!(piece, &expected[matched..matched + piece.len()]);
        matched += piece.len();
        pos += progress.consumed;
        if progress.state == StepState::Done {
            assert_eq!(matched, expected.len());
            return Ok(());
        }
    }
}

/// The guard sees an allocation, so a clean run of the test below means
/// something.
#[test]
#[should_panic(expected = "1 allocations")]
fn guard_counts_allocations() {
    without_allocating("a Vec", || std::hint::black_box(vec![1u8; 16]));
}

#[test]
fn workspace_decoder_never_allocates() {
    let mut mixed = text(200_000);
    mixed.extend(noise(100_000));
    mixed.extend(std::iter::repeat_n(0x5A, 150_000));
    mixed.extend(text(60_000));
    let mut streams = Vec::new();
    for level in [
        CompressionLevel::Uncompressed,
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Best,
        CompressionLevel::Level(19),
    ] {
        streams.push((compress_slice_to_vec(&mixed, level), mixed.clone()));
    }
    // Multi-segment with a 1 KiB window: decoded through a ring that wraps.
    let ring_parameters = CompressionParameters::builder(CompressionLevel::Level(3))
        .window_log(10)
        .build()
        .unwrap();
    streams.push((
        compress_with_parameters(&mixed, &ring_parameters),
        mixed.clone(),
    ));
    let small = text(9_000);
    let mut concatenated = compress_slice_to_vec(&small, CompressionLevel::Default);
    concatenated.extend_from_slice(&0x184D_2A51u32.to_le_bytes());
    concatenated.extend_from_slice(&3u32.to_le_bytes());
    concatenated.extend_from_slice(b"pad");
    concatenated.extend(compress_slice_to_vec(
        &noise(4_000),
        CompressionLevel::Fastest,
    ));
    let mut concatenated_expected = small.clone();
    concatenated_expected.extend(noise(4_000));
    streams.push((concatenated, concatenated_expected));
    streams.push((
        compress_slice_to_vec(&[], CompressionLevel::Default),
        Vec::new(),
    ));

    let corrupt: Vec<Vec<u8>> = {
        let frame = compress_slice_to_vec(&small, CompressionLevel::Default);
        (0..frame.len())
            .flat_map(|i| {
                [0x01u8, 0x80, 0xFF].map(|flip| {
                    let mut c = frame.clone();
                    c[i] ^= flip;
                    c
                })
            })
            .collect()
    };
    let truncated = compress_slice_to_vec(&small, CompressionLevel::Best);
    let dictionary_frame = include_bytes!("../dict_tests/files/ModemManager.service.zst");

    let limits = DecodeLimits::new(1 << 20);
    let mut workspace = vec![0u8; workspace_bytes(limits.max_window_size).unwrap()];
    let mut small_workspace = vec![0u8; workspace_bytes(4 << 10).unwrap()];

    without_allocating("construction, every stream, failures, reset, drop", || {
        let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).unwrap();
        for chunk in [1, 977, usize::MAX] {
            for (stream, expected) in &streams {
                decoder.reset();
                decode_and_compare(
                    &mut decoder,
                    stream,
                    expected,
                    chunk.min(stream.len().max(1)),
                    &mut WorkBudget::new(u64::MAX),
                )
                .unwrap();
            }
        }

        for c in &corrupt {
            decoder.reset();
            let mut out = [0u8; 700];
            let mut work = WorkBudget::new(u64::MAX);
            let mut pos = 0;
            while let Ok(progress) = decoder.step(&c[pos..], true, &mut out, &mut work) {
                pos += progress.consumed;
                if progress.state == StepState::Done {
                    break;
                }
            }
        }

        for cut in 1..truncated.len() {
            decoder.reset();
            let mut out = [0u8; 700];
            let mut work = WorkBudget::new(u64::MAX);
            let mut pos = 0;
            let error = loop {
                match decoder.step(&truncated[pos..cut], true, &mut out, &mut work) {
                    Ok(progress) => pos += progress.consumed,
                    Err(e) => break e,
                }
            };
            assert!(matches!(error, WorkspaceDecodeError::Truncated));
        }

        decoder.reset();
        let mut out = [0u8; 64];
        assert!(matches!(
            decoder.step(
                dictionary_frame,
                true,
                &mut out,
                &mut WorkBudget::new(u64::MAX)
            ),
            Err(WorkspaceDecodeError::Unsupported(_))
        ));

        let (stream, expected) = &streams[2];
        decoder.reset();
        assert!(matches!(
            decode_and_compare(
                &mut decoder,
                stream,
                expected,
                4096,
                &mut WorkBudget::new(50_000)
            ),
            Err(WorkspaceDecodeError::Limit(LimitExceeded::Work { .. }))
        ));
        drop(decoder);

        let mut capped = WorkspaceDecoder::new(
            &mut workspace,
            limits.with_max_output(expected.len() as u64 - 1),
        )
        .unwrap();
        assert!(matches!(
            decode_and_compare(
                &mut capped,
                stream,
                expected,
                4096,
                &mut WorkBudget::new(u64::MAX)
            ),
            Err(WorkspaceDecodeError::Limit(LimitExceeded::Output { .. }))
        ));
        drop(capped);

        let mut narrow =
            WorkspaceDecoder::new(&mut small_workspace, DecodeLimits::new(4 << 10)).unwrap();
        assert!(matches!(
            decode_and_compare(
                &mut narrow,
                stream,
                expected,
                4096,
                &mut WorkBudget::new(u64::MAX)
            ),
            Err(WorkspaceDecodeError::Limit(LimitExceeded::Window { .. }))
        ));
        drop(narrow);
    });
}
