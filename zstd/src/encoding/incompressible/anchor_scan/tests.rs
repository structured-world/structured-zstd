use super::{ANCHOR_BATCH, fill_anchors, fill_anchors_scalar};
use crate::encoding::fastpath::select_kernel;

const NEEDLE: u8 = 0x9E;

/// Every offset the selected tier reports, in order, walking the batches the
/// way the grid does.
fn all_anchors(hay: &[u8], scalar: bool) -> alloc::vec::Vec<u32> {
    let kernel = select_kernel();
    let mut out = [0u32; ANCHOR_BATCH];
    let mut found = alloc::vec::Vec::new();
    let mut at = 0usize;
    while at < hay.len() {
        let (n, consumed) = if scalar {
            fill_anchors_scalar(&hay[at..], NEEDLE, &mut out)
        } else {
            fill_anchors(kernel, &hay[at..], NEEDLE, &mut out)
        };
        found.extend(out[..n].iter().map(|off| off + at as u32));
        if consumed == 0 {
            break;
        }
        at += consumed;
    }
    found
}

/// The selected tier and the scalar baseline must agree on every offset, at
/// lengths either side of each vector width and with the needle at each
/// position in turn — a tier that disagrees anywhere would make the grid's
/// answer depend on the CPU.
#[test]
fn every_tier_reports_the_same_anchors_as_the_scalar_path() {
    for len in [0usize, 1, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 129, 257] {
        let empty = alloc::vec![0u8; len];
        assert!(
            all_anchors(&empty, false).is_empty(),
            "len {len}, no needle"
        );

        for at in 0..len {
            let mut hay = alloc::vec![0u8; len];
            hay[at] = NEEDLE;
            assert_eq!(
                all_anchors(&hay, false),
                alloc::vec![at as u32],
                "len {len}, needle at {at}",
            );
            assert_eq!(
                all_anchors(&hay, true),
                alloc::vec![at as u32],
                "scalar: len {len}, needle at {at}",
            );
        }
    }
}

/// Every occurrence, in order, and across the batch boundary: the grid records
/// each anchor it is given, so a dropped or reordered one is a repeat it never
/// sees.
#[test]
fn all_occurrences_come_back_in_order_across_batches() {
    let mut hay = alloc::vec![0u8; 8192];
    let mut expected = alloc::vec::Vec::new();
    // More than one batch's worth, spaced unevenly so no offset is a multiple
    // of a vector width.
    for i in 0..(ANCHOR_BATCH + 37) {
        let at = 3 + i * 13;
        hay[at] = NEEDLE;
        expected.push(at as u32);
    }
    assert_eq!(all_anchors(&hay, false), expected);
    assert_eq!(all_anchors(&hay, true), expected);
}

/// A run of anchors longer than the buffer: the call must report where it
/// stopped so the caller resumes at the next unexamined byte rather than
/// skipping the rest of the run.
#[test]
fn a_full_buffer_reports_where_the_scan_stopped() {
    let hay = alloc::vec![NEEDLE; ANCHOR_BATCH * 3];
    let expected: alloc::vec::Vec<u32> = (0..hay.len() as u32).collect();
    assert_eq!(all_anchors(&hay, false), expected);
    assert_eq!(all_anchors(&hay, true), expected);
}
