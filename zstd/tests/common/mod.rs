//! Shared test utilities for integration probes.

/// Block-type counts produced by [`parse_zstd_block_breakdown`].
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct BlockBreakdown {
    pub raw: usize,
    pub rle: usize,
    pub compressed: usize,
    pub total_block_bytes: usize,
}

/// Walk a complete zstd frame and tally block-type counts. The loop
/// terminates on (a) the in-block `last_block` sentinel, or (b)
/// running out of frame bytes — there is no fixed iteration cap, so
/// large frames are not truncated.
///
/// Returns `None` if the input does not start with the zstd magic
/// or is too short for a frame header.
#[allow(dead_code)]
pub fn parse_zstd_block_breakdown(frame: &[u8]) -> Option<BlockBreakdown> {
    if frame.len() < 6 || &frame[..4] != b"\x28\xb5\x2f\xfd" {
        return None;
    }
    let fhd = frame[4];
    let single_segment = (fhd & 0x20) != 0;
    let fcs_flag = fhd >> 6;
    let dict_id_flag = fhd & 0x03;
    let dict_id_len = match dict_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let window_descriptor_len = if single_segment { 0 } else { 1 };
    let fcs_len = match fcs_flag {
        0 => {
            if single_segment {
                1
            } else {
                0
            }
        }
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut off = 5 + window_descriptor_len + dict_id_len + fcs_len;
    let mut out = BlockBreakdown::default();
    while off + 3 <= frame.len() {
        let h = u32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], 0]);
        let last = h & 1;
        let bt = (h >> 1) & 3;
        let bs = (h >> 3) as usize;
        let payload = if bt == 1 { 1 } else { bs };
        let step = 3 + payload;
        if off + step > frame.len() {
            break;
        }
        match bt {
            0 => out.raw += 1,
            1 => out.rle += 1,
            2 => out.compressed += 1,
            _ => {}
        }
        out.total_block_bytes += bs;
        off += step;
        if last == 1 {
            break;
        }
    }
    Some(out)
}

/// Convenience wrapper for diagnostic test prints — formats the
/// breakdown into the legacy diagnostic line shape used by the
/// level22 probes.
#[allow(dead_code)]
pub fn dump_block_breakdown(label: &str, frame: &[u8]) {
    match parse_zstd_block_breakdown(frame) {
        Some(bb) => println!(
            "  {label}: raw={}, rle={}, compressed={}, total_block_bytes={}",
            bb.raw, bb.rle, bb.compressed, bb.total_block_bytes
        ),
        None => println!("  {label}: not a zstd frame"),
    }
}
