use super::*;

/// Roundtrip a single short symbol through HufCStream and verify
/// the byte output decodes back to the same bit pattern.
#[test]
fn add_bits_single_symbol_emits_correct_byte() {
    let mut out: Vec<u8> = Vec::new();
    let mut s = HufCStream::new(&mut out, 64).expect("init ok");
    // Symbol: nb_bits=4, value=0b1011 (11). Packed: low=4, top=11<<60.
    let elt = pack_huf_celt(0b1011, 4);
    s.add_bits::<false>(elt, 0);
    let n = s.close();
    assert!(n > 0);
    assert_eq!(out.len(), 1);
    // Upstream zstd `HUF_addBits` + `HUF_flushBits` layout (top-down
    // packing in the 64-bit container, then `flushBits` shifts
    // the buffered bits down to the bottom of a 0-padded word
    // and `MEM_writeLE` stores 8 bytes little-endian — emitted
    // byte 0 is the LOW byte of that word):
    //
    // After `add_bits(pack_huf_celt(0b1011, 4), 0)`:
    //   container top 4 bits = 0b1011, bit_pos = 4
    // After `close()` prepends end-mark `(value=1, nb_bits=1)`:
    //   container top 5 bits = [1, 1, 0, 1, 1] (high → low),
    //   bit_pos = 5
    // `flush_bits` then `container >> (64 - 5)` produces 0b11011
    // = 27 = 0x1B, which lands in `out[0]`.
    assert_eq!(
        out[0], 0x1B,
        "first emitted byte must mirror upstream zstd's HUF_addBits + \
             HUF_endMark packing collapsed to a 5-bit prefix 0b11011",
    );
}

/// Encode multiple symbols summing to > 64 bits; expect the
/// container to flush partway and write whole bytes to output.
#[test]
fn add_bits_overflowing_container_flushes_correctly() {
    let mut out: Vec<u8> = Vec::new();
    let mut s = HufCStream::new(&mut out, 256).expect("init ok");
    // 8 symbols of 8 bits each = 64 bits — exactly fills container.
    for i in 0..8 {
        let elt = pack_huf_celt(i as u32, 8);
        s.add_bits::<false>(elt, 0);
    }
    s.flush_bits::<false>();
    // After flushing 64 bits = 8 bytes; cursor advanced 8.
    assert_eq!(s.cursor - s.start_idx, 8);
    // pending bits should be 0 (cleanly flushed).
    assert_eq!(s.pending_bits(), 0);
    let n = s.close();
    // close adds 1-bit end mark + flush → 1 trailing byte for end mark.
    assert!(n >= 8);
}

/// Dual-container parallel encode through `encode_unrolled` (which
/// inlines the zero/merge of container 1 into container 0). With a
/// uniform 4-bit code over 16 symbols, the total emitted size is
/// order-independent: 16 * 4 = 64 payload bits + a 1-bit end mark =
/// 65 bits → 9 bytes. K_UNROLL=4 with 16 symbols runs phase 3 (the
/// dual-container loop) twice, so the merge path is exercised.
#[test]
fn encode_unrolled_dual_container_size_is_deterministic() {
    let mut out: Vec<u8> = Vec::new();
    let mut s = HufCStream::new(&mut out, 64).expect("init ok");
    // Every symbol maps to the same 4-bit code (value 0b1010).
    let table = [pack_huf_celt(0b1010, 4); 256];
    let data = [0u8; 16];
    s.encode_unrolled::<4, false, false>(&table, &data);
    let n = s.close();
    assert_eq!(
        n, 9,
        "16 symbols * 4 bits + 1 end-mark bit = 65 bits = 9 bytes"
    );
}
