use super::super::{ADVANCE, decode_and_execute_sequences, init_sequence_stream};
use crate::blocks::sequence_section::SequencesHeader;
use crate::cpu_kernel::{CpuKernelTag, ScalarKernel, detect_cpu_kernel};
use crate::decoding::buffer_backend::BufferBackend;
use crate::decoding::dictionary::Dictionary;
use crate::decoding::errors::{DecompressBlockError, ExecuteSequencesError};
use crate::decoding::flat_buf::FlatBuf;
use crate::decoding::ringbuffer::RingBuffer;
use crate::decoding::scratch::DecoderScratch;
use alloc::vec::Vec;

const PREFIX: &[u8] = b"previous output:";
const DICTIONARY: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz";

// (literal length, match length, resolved offset). The first five encoded
// offsets are 43, 1, 2, 3, 3: a dictionary reference followed by repcodes,
// including the zero-literal rep1 and rep1-minus-one cases. The remaining
// offsets are explicit. Distinct literals and varying matches expose ring
// ordering errors; 19 sequences wrap the eight-slot ring and drain from slot 3.
const SEQUENCES: [(usize, usize, usize); 19] = [
    (1, 5, 40),
    (0, 3, 1),
    (2, 7, 40),
    (3, 11, 4),
    (0, 9, 3),
    (0, 8, 18),
    (1, 9, 19),
    (2, 10, 20),
    (3, 11, 21),
    (4, 3, 22),
    (0, 4, 23),
    (1, 5, 24),
    (2, 6, 25),
    (3, 7, 26),
    (4, 8, 27),
    (0, 9, 28),
    (1, 10, 29),
    (2, 11, 30),
    (3, 3, 31),
];

// Wire bitstream for SEQUENCES using the predefined LL/ML/OF FSE tables.
// Kept fixed so the test does not depend on the encoder's matching or table
// selection. All three FSE states consume transition bits, and OF extra bits
// cross multiple reader windows. The sequence header is [19, 0x00].
const BITSTREAM: &[u8] = &[
    66, 129, 17, 148, 65, 160, 54, 255, 44, 214, 39, 33, 61, 243, 224, 57, 184, 243, 88, 167, 101,
    115, 2, 130, 211, 140, 155, 27, 109, 22, 174, 73, 236, 45, 225, 53, 195, 205, 16, 64, 172, 75,
    93, 1,
];

fn supported_kernels() -> Vec<CpuKernelTag> {
    let mut kernels = alloc::vec![CpuKernelTag::Scalar];
    #[cfg(all(
        feature = "std",
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-sse"
    ))]
    if std::is_x86_feature_detected!("sse2") {
        kernels.push(CpuKernelTag::Sse2);
    }
    #[cfg(all(
        feature = "std",
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-bmi2"
    ))]
    if std::is_x86_feature_detected!("bmi2") {
        kernels.push(CpuKernelTag::Bmi2);
    }
    #[cfg(all(
        feature = "std",
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-avx2"
    ))]
    if std::is_x86_feature_detected!("bmi2") && std::is_x86_feature_detected!("avx2") {
        kernels.push(CpuKernelTag::Avx2);
    }
    #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
    kernels.push(CpuKernelTag::Neon);
    // Includes SVE/VBMI2 only when the production detector has verified their
    // full feature set. Never ask an unsafe trampoline to run on another CPU.
    let native = detect_cpu_kernel();
    if !kernels.contains(&native) {
        kernels.push(native);
    }
    kernels
}

fn check_pipeline<B: BufferBackend>(kernel: CpuKernelTag, fail_at: Option<usize>) {
    let dict = Dictionary::from_raw_content(0, DICTIONARY.to_vec())
        .unwrap()
        .into_handle();
    let mut header = SequencesHeader::new();
    header.parse_from_header(&[19, 0x00]).unwrap();

    // Independent byte-at-a-time oracle, using resolved offsets rather than
    // the decoder's repcode or match-copy helpers. Includes overlapping copies
    // and references that reach back past PREFIX into the dictionary.
    let mut expected = DICTIONARY.to_vec();
    expected.extend_from_slice(PREFIX);
    let mut literals = Vec::new();
    for (ll, ml, offset) in SEQUENCES {
        for _ in 0..ll {
            let literal = 0x80 + literals.len() as u8;
            literals.push(literal);
            expected.push(literal);
        }
        for _ in 0..ml {
            expected.push(expected[expected.len() - offset]);
        }
    }
    literals.extend_from_slice(b"tail");
    expected.extend_from_slice(b"tail");
    let literals_len = literals.len();
    literals.resize(literals_len + crate::WILDCOPY_OVERLENGTH, 0);

    let mut scratch = DecoderScratch::<B>::new(4096);
    scratch.init_from_dict(&dict);
    // Reserve before the checkpoint, as the block decoder does, so an error
    // can restore the cursor without an intervening allocation invalidating it.
    scratch.buffer.reserve_exact(4096);
    scratch.buffer.push::<ScalarKernel>(PREFIX);
    {
        let setup = init_sequence_stream::<B, ScalarKernel>(
            &header,
            BITSTREAM,
            &mut scratch.fse,
            &mut scratch.buffer,
            Some(dict.as_dict()),
        )
        .unwrap();
        assert!(
            setup.use_long_pipeline,
            "fixture must exercise the long path"
        );
        assert!(
            setup.max_update_bits > 0,
            "fixture must use FSE transitions"
        );
    }
    // The probe consumed the cold-dictionary flag; reattach before decoding.
    scratch.init_from_dict(&dict);
    let decode = |scratch: &mut DecoderScratch<B>, literal_count| {
        decode_and_execute_sequences(
            &header,
            BITSTREAM,
            &mut scratch.fse,
            &mut scratch.buffer,
            &mut scratch.offset_hist,
            &literals,
            literal_count,
            Some(dict.as_dict()),
            kernel,
        )
    };

    if let Some(sequence) = fail_at {
        let have: usize = SEQUENCES[..sequence].iter().map(|s| s.0).sum();
        let wanted = have + SEQUENCES[sequence].0;
        assert!(
            wanted > have,
            "failure must occur at the requested sequence"
        );
        let err = decode(&mut scratch, have).expect_err("missing literals must fail");
        assert!(
            matches!(
                err,
                DecompressBlockError::ExecuteSequencesError(
                    ExecuteSequencesError::NotEnoughBytesForSequence { wanted: w, have: h }
                ) if w == wanted && h == have
            ),
            "{kernel:?}, sequence {sequence}: {err:?}"
        );
        assert_eq!(
            scratch.buffer.len(),
            PREFIX.len(),
            "{kernel:?}: rollback length"
        );
        assert_eq!(scratch.buffer.buffer_ref().as_slices(), (PREFIX, &[][..]));
        assert_eq!(
            scratch.offset_hist,
            dict.as_dict().offset_hist,
            "{kernel:?}: rollback offsets"
        );
        // Retry against the same output buffer: stale cursor/history must not
        // leak into the next decode after the failed main-loop or drain copy.
        scratch.init_from_dict(&dict);
    }

    decode(&mut scratch, literals_len).unwrap();
    assert!(!scratch.fse.ddict_is_cold, "the cold flag is consumed once");
    assert_eq!(
        scratch.offset_hist,
        [31, 30, 29],
        "{kernel:?}: final offsets"
    );
    assert_eq!(
        scratch.buffer.drain(),
        expected[DICTIONARY.len()..],
        "{kernel:?}: output"
    );
}

#[test]
fn long_pipeline_decodes_dictionary_repcodes_and_ring_wrap() {
    for kernel in supported_kernels() {
        check_pipeline::<RingBuffer>(kernel, None);
        check_pipeline::<FlatBuf>(kernel, None);
    }
}

#[test]
fn long_pipeline_rolls_back_main_loop_failure() {
    // The main loop executes sequences 0..NUM_SEQUENCES-ADVANCE.
    let fail_at = 3;
    assert!(fail_at < SEQUENCES.len() - ADVANCE);
    for kernel in supported_kernels() {
        check_pipeline::<RingBuffer>(kernel, Some(fail_at));
        check_pipeline::<FlatBuf>(kernel, Some(fail_at));
    }
}

#[test]
fn long_pipeline_rolls_back_drain_failure() {
    // Several drain copies succeed before sequence 14 runs out of literals.
    let fail_at = 14;
    assert!(fail_at >= SEQUENCES.len() - ADVANCE);
    for kernel in supported_kernels() {
        check_pipeline::<RingBuffer>(kernel, Some(fail_at));
        check_pipeline::<FlatBuf>(kernel, Some(fail_at));
    }
}
