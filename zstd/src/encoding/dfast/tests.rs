use super::*;
use crate::encoding::workspace::{IngestPlan, Workspace, no_trailing};

/// A matcher whose tables are laid out in `workspace`, in the given slot
/// format.
fn bound(workspace: &mut Workspace, tagged: bool) -> DfastMatchGenerator {
    let mut matcher = DfastMatchGenerator::new(1 << 20);
    matcher.set_hash_bits(12, 11);
    workspace.begin_layout(0, no_trailing, IngestPlan::Stream);
    workspace.open(matcher.tables_workspace_bytes(), usize::MAX);
    matcher.bind_tables(workspace, tagged);
    matcher
}

/// A tagged rebase moves every position by the reducer and keeps its tag;
/// a position at or under the reducer empties. The base moves by the
/// reducer in position units, not in slot units.
#[test]
fn a_tagged_rebase_moves_positions_and_keeps_tags() {
    let mut workspace = Workspace::new();
    let mut matcher = bound(&mut workspace, true);
    let base = matcher.position_base;
    let keep = ((DFAST_TAGGED_REBASE + 500) << DFAST_TAG_BITS) | 0x5A;
    let drop = (DFAST_TAGGED_REBASE << DFAST_TAG_BITS) | 0xFF;
    matcher.tables.as_mut_slice()[3] = keep;
    matcher.tables.as_mut_slice()[4] = drop;

    matcher.reduce(DFAST_TAGGED_REBASE);

    assert_eq!(matcher.tables[3], (500 << DFAST_TAG_BITS) | 0x5A);
    assert_eq!(matcher.tables[4], DFAST_EMPTY_SLOT);
    assert_eq!(
        matcher.position_base,
        base + DFAST_TAGGED_REBASE as usize,
        "the base moves in position units",
    );
}

/// A tagged table rebases before a position leaves its range, and keeps
/// the window across the rebase: a slot a window behind the new position
/// survives.
#[test]
fn a_tagged_table_rebases_within_its_range_and_keeps_the_window() {
    let mut workspace = Workspace::new();
    let mut matcher = bound(&mut workspace, true);
    let base = matcher.position_base;
    let near = base + DFAST_TAGGED_MAX_REL - DFAST_TAGGED_WINDOW_LIMIT;
    let slot = matcher.pack_slot(near);
    matcher.tables.as_mut_slice()[7] = (slot << DFAST_TAG_BITS) | 1;

    let far = base + DFAST_TAGGED_MAX_REL + 1;
    matcher.ensure_room_for(far);

    assert!(far - matcher.position_base <= DFAST_TAGGED_MAX_REL);
    let kept = matcher.tables[7];
    assert_ne!(kept, DFAST_EMPTY_SLOT, "a slot within the window survives");
    assert_eq!(kept & DFAST_TAG_MASK, 1, "and keeps its tag");
    assert_eq!(
        matcher.position_base + ((kept >> DFAST_TAG_BITS) as usize) - 1,
        near,
        "and still names the same position",
    );
}

/// A probe reads a tagged word only under its own tag; another tag in the
/// same slot reads as empty.
#[test]
fn a_tagged_slot_answers_only_its_own_tag() {
    let mut workspace = Workspace::new();
    let matcher = bound(&mut workspace, true);
    let shift = 64 - matcher.long_hash_bits;
    let mixed = 0x0123_4567_89AB_CDEFu64;
    let word = live_slot::<true>(42, mixed, shift);
    assert_eq!(matcher.packed_in(word, mixed, shift), 42);
    let other = mixed ^ (1u64 << (shift - DFAST_TAG_BITS as usize));
    assert_eq!(matcher.packed_in(word, other, shift), DFAST_EMPTY_SLOT);
}

/// Fresh tagged tables start at the floor the next frame begins at, not at the
/// previous frame's start: the layout runs after the history is retired and
/// before the reset applies the new floor, and a base left behind would make
/// the first insertion rebase once per `DFAST_TAGGED_REBASE` of the gap, each
/// a pass over both tables.
#[test]
fn fresh_tagged_tables_start_at_the_next_frames_floor() {
    let mut workspace = Workspace::new();
    let mut matcher = bound(&mut workspace, false);
    let floor = 3 * DFAST_TAGGED_REBASE as usize;
    matcher.retired = Some(RetiredHistory {
        next_floor: floor,
        kept: None,
    });
    workspace.begin_layout(0, no_trailing, IngestPlan::Stream);
    workspace.open(matcher.tables_workspace_bytes(), usize::MAX);
    matcher.bind_tables(&mut workspace, true);
    matcher.reset();
    assert_eq!(matcher.history_abs_start, floor);
    assert_eq!(matcher.position_base, floor);
}

/// Laying the tables out again in the other slot format is not a
/// continuation: they come back empty and fresh.
#[test]
fn changing_the_slot_format_empties_the_tables() {
    let mut workspace = Workspace::new();
    let mut matcher = bound(&mut workspace, true);
    matcher.tables.as_mut_slice()[0] = 0x1234;
    workspace.begin_layout(0, no_trailing, IngestPlan::Stream);
    workspace.open(matcher.tables_workspace_bytes(), usize::MAX);
    matcher.bind_tables(&mut workspace, false);
    assert!(!matcher.tagged);
    assert!(matcher.tables.iter().all(|&slot| slot == DFAST_EMPTY_SLOT));
}

/// Live tables filled from a dictionary's index are the tables hashing the
/// dictionary into them leaves, in either slot format: the same slots, the
/// last position of each key winning, counted from the same base.
#[test]
fn a_copied_dictionary_index_fills_the_tables_as_hashing_the_dictionary_does() {
    // Short repeating phrases, so many positions share a key and a slot keeps
    // only the last of them.
    let mut state = 0x2545_F491u32;
    let dict: alloc::vec::Vec<u8> = (0..3000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            b'a' + (state >> 30) as u8
        })
        .collect();
    for tagged in [false, true] {
        let mut hashed_workspace = Workspace::new();
        let mut hashed = bound(&mut hashed_workspace, tagged);
        hashed.commit_input(&dict);
        hashed.skip_matching_dense();

        let mut copied_workspace = Workspace::new();
        let mut copied = bound(&mut copied_workspace, tagged);
        copied.commit_input(&dict);
        copied.skip_matching_for_dict_copy();
        copied.finish_dict_copy();

        assert!(copied.dict_copied_from_index());
        assert_eq!(
            copied.position_base, hashed.position_base,
            "tagged {tagged}"
        );
        assert!(
            copied.tables.iter().any(|&slot| slot != DFAST_EMPTY_SLOT),
            "tagged {tagged}: the dictionary is indexed"
        );
        assert!(
            copied.tables.iter().eq(hashed.tables.iter()),
            "tagged {tagged}: the copied tables differ from the hashed ones"
        );
    }
}

/// A copy with no dictionary block committed indexes nothing and leaves the
/// live tables and their base as they were.
#[test]
fn a_dictionary_copy_without_a_block_leaves_the_tables_alone() {
    let mut workspace = Workspace::new();
    let mut matcher = bound(&mut workspace, false);
    matcher.tables.as_mut_slice()[5] = 0x77;
    let base = matcher.position_base;

    matcher.skip_matching_for_dict_copy();
    matcher.finish_dict_copy();

    assert!(matcher.dict.table().is_none());
    assert_eq!(matcher.tables[5], 0x77);
    assert_eq!(matcher.position_base, base);
}
