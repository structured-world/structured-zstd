//! The dictionary copier writes a sequence as literals followed by a match
//! read out of the dictionary. It picks between two copy shapes by how much
//! dictionary is left past the match: enough for the wildcopy stride to
//! over-read, or not, in which case the copy is exact. Dispatch alone never
//! reaches both, and on a given host it reaches only the tier that host
//! selects, so these drive each shape and each available tier directly and
//! check the bytes against what the sequence defines.

use crate::decoding::buffer_backend::BufferBackend;
use crate::decoding::decode_buffer::DecodeBuffer;
use crate::decoding::user_slice_buf::UserSliceBackend;

extern crate std;
use alloc::vec;
use alloc::vec::Vec;

/// Literals and dictionary content that are distinguishable everywhere, so a
/// copy that reads or writes at the wrong offset shows up as wrong bytes
/// rather than as a coincidence of repeated values.
fn literals(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 + 1).collect()
}

fn dictionary(len: usize) -> Vec<u8> {
    (0..len).map(|i| 255 - (i % 251) as u8).collect()
}

/// What the sequence defines: `lit_length` literals, then `match_length` bytes
/// read from the front of the dictionary slice.
fn expected(lits: &[u8], lit_length: usize, dict: &[u8], match_length: usize) -> Vec<u8> {
    let mut want = Vec::with_capacity(lit_length + match_length);
    want.extend_from_slice(&lits[..lit_length]);
    want.extend_from_slice(&dict[..match_length]);
    want
}

/// Run one sequence through the tier-neutral copier and return what landed in
/// the output. `dict_room` is the dictionary length past the match, which is
/// what selects the copy shape; `tail_slack` is the writable room past the
/// sequence, which selects the tight-tail branch.
fn run_default(
    lit_length: usize,
    match_length: usize,
    dict_room: usize,
    tail_slack: usize,
) -> Vec<u8> {
    let lits = literals(lit_length.max(16) + 64);
    let dict = dictionary(match_length + dict_room);
    let mut out = vec![0u8; lit_length + match_length + tail_slack];
    let backend = UserSliceBackend::from_slice(out.as_mut_slice());
    let mut buf = DecodeBuffer::from_backend(backend, 1 << 20);
    // SAFETY: the literal source carries the whole `lits` allocation, which is
    // at least 16 bytes longer than `lit_length` so the unconditional 16-byte
    // read stays inside it; the dictionary slice covers `match_length`; the
    // output holds the sequence.
    unsafe {
        buf.buffer_mut()
            .exec_sequence_inline_dict(lits.as_ptr(), lit_length, &dict, match_length)
            .expect("the output was sized for this sequence");
    }
    let written = buf.buffer_mut().tail();
    assert_eq!(
        written,
        lit_length + match_length,
        "the copier must advance the cursor by exactly the sequence"
    );
    out[..written].to_vec()
}

/// Every shape the branch picks between, on the tier-neutral body: a
/// dictionary with room to over-read, one ending exactly at the match, and an
/// output whose tail leaves no room for the overshoot.
#[test]
fn dictionary_copy_writes_the_sequence_in_every_shape() {
    for &(lit_length, match_length, dict_room, tail_slack, what) in &[
        (20usize, 40usize, 64usize, 64usize, "room to over-read"),
        (20, 40, 0, 64, "dictionary ends at the match"),
        (20, 40, 64, 0, "no room past the output"),
        (20, 40, 0, 0, "neither has room"),
        (0, 3, 64, 64, "no literals, shortest legal match"),
        (17, 17, 1, 64, "both lengths just past one stride"),
        (
            64,
            200,
            3,
            64,
            "long match, dictionary ends inside the stride",
        ),
    ] {
        let lits = literals(lit_length.max(16) + 64);
        let dict = dictionary(match_length + dict_room);
        assert_eq!(
            run_default(lit_length, match_length, dict_room, tail_slack),
            expected(&lits, lit_length, &dict, match_length),
            "dictionary copy wrote the wrong bytes with {what}"
        );
    }
}

/// The AVX2 body is a separate copier with a wider stride and its own choice
/// between the same shapes, and normal dispatch only ever runs one tier on a
/// given host. Run it against the tier-neutral body on identical input and
/// require the same bytes, so a stride or overshoot mistake in either cannot
/// hide behind the other never running.
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
#[test]
fn avx2_dictionary_copy_matches_the_tier_neutral_body() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    for &(lit_length, match_length, dict_room, tail_slack) in &[
        (20usize, 40usize, 64usize, 64usize),
        (20, 40, 0, 64),
        (20, 40, 64, 0),
        (20, 40, 0, 0),
        (0, 3, 64, 64),
        (17, 17, 1, 64),
        (64, 200, 3, 64),
        // Between the 16- and 32-byte strides: the wider body takes the
        // narrower copy here and the two must still agree.
        (33, 48, 20, 64),
    ] {
        let lits = literals(lit_length.max(16) + 64);
        let dict = dictionary(match_length + dict_room);
        let mut out = vec![0u8; lit_length + match_length + tail_slack];
        let backend = UserSliceBackend::from_slice(out.as_mut_slice());
        let mut buf = DecodeBuffer::from_backend(backend, 1 << 20);
        // The macro carries its own `unsafe` block. Its preconditions are as
        // for `run_default`, plus the host advertising AVX2, checked above.
        let result =
            exec_sequence_avx2_dict_inline!(buf, lits.as_ptr(), lit_length, &dict, match_length);
        result.expect("the output was sized for this sequence");
        let written = buf.buffer_mut().tail();
        assert_eq!(written, lit_length + match_length);
        assert_eq!(
            out[..written].to_vec(),
            run_default(lit_length, match_length, dict_room, tail_slack),
            "the AVX2 dictionary copy diverged from the tier-neutral one at \
             lit={lit_length} ml={match_length} room={dict_room} slack={tail_slack}"
        );
    }
}
