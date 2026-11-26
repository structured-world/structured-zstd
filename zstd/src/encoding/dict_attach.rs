//! Shared CDict-style dictionary-attach lifecycle, parameterized by the
//! matcher's immutable dictionary table type `T`.
//!
//! This is the level-1 scaffolding common to every match-finder backend that
//! supports upstream zstd `ZSTD_dictMatchState` attach-by-reference: instead of
//! re-priming the whole dictionary into the live hash table(s) on every frame
//! (O(dict) per frame, the dominant cost on small-payload `compress-dict`), the
//! dictionary is hashed ONCE into a SEPARATE immutable table `T` held here, and
//! the kernel dual-probes the live table(s) plus this dict table. A reused
//! compressor keeps `T` across the per-frame `reset` (the dict re-commits to the
//! same absolute history positions) via the `primed` cache flag — mirroring
//! C copying / referencing `cdict->matchState` rather than rebuilding it.
//!
//! `T` is backend-specific (Fast = single `FastHashTable`; Dfast = a long+short
//! pair; Row/HC/BT = their own structures); the per-backend BUILD of `T` and the
//! per-kernel dual-probe LOOKUP are level-2 (kept in each backend, expanded per
//! SIMD tier). This type only owns the shared lifecycle: presence, the
//! dict/input boundary, the build-once cache flag, and invalidation.
//!
//! A built table is held by reference count, so the dictionary itself can hold
//! it too ([`DictIndexSlot`]) and every compressor the dictionary attaches to
//! takes it by reference, as upstream attaches `cdict->matchState` instead of
//! indexing the dictionary per context.

/// Shared owner of a built dictionary table: `Arc` on atomic-pointer targets,
/// `Rc` otherwise, as the encoder dictionary itself is held.
#[cfg(target_has_atomic = "ptr")]
pub(crate) type Shared<T> = alloc::sync::Arc<T>;
#[cfg(not(target_has_atomic = "ptr"))]
pub(crate) type Shared<T> = alloc::rc::Rc<T>;

/// A fully built dictionary table with the boundary and fill origin it was
/// built to, the state [`DictKeep::revive`] restores, and how far into the
/// dictionary it starts indexing.
pub(crate) struct Published<T> {
    pub(crate) table: Shared<T>,
    pub(crate) region_len: usize,
    pub(crate) next_to_update: usize,
    pub(crate) index_floor: usize,
}

/// A dictionary's built table for one backend, published by the first
/// compressor that indexes the dictionary and attached by reference by every
/// later one whose geometry matches (upstream `cdict->matchState`, built once
/// in `ZSTD_createCDict`). Set once: a compressor resolving another geometry
/// builds its own table, as it would without the slot.
pub(crate) struct DictIndexSlot<T> {
    /// A leaked `Box<Published<T>>` once published, freed with the slot.
    #[cfg(target_has_atomic = "ptr")]
    table: core::sync::atomic::AtomicPtr<Published<T>>,
    /// The slot owns a `Shared<T>`, and is `Send` / `Sync` only as that is;
    /// the atomic pointer alone would claim both for any `T`.
    #[cfg(target_has_atomic = "ptr")]
    _owns: core::marker::PhantomData<Shared<T>>,
    /// Without atomic pointers the dictionary is held by `Rc` and stays on
    /// one thread.
    #[cfg(not(target_has_atomic = "ptr"))]
    table: core::cell::OnceCell<Published<T>>,
}

impl<T> Default for DictIndexSlot<T> {
    fn default() -> Self {
        Self {
            #[cfg(target_has_atomic = "ptr")]
            table: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
            #[cfg(target_has_atomic = "ptr")]
            _owns: core::marker::PhantomData,
            #[cfg(not(target_has_atomic = "ptr"))]
            table: core::cell::OnceCell::new(),
        }
    }
}

impl<T> DictIndexSlot<T> {
    /// The published table, if any.
    pub(crate) fn get(&self) -> Option<&Published<T>> {
        #[cfg(target_has_atomic = "ptr")]
        {
            let published = self.table.load(core::sync::atomic::Ordering::Acquire);
            // SAFETY: a non-null pointer was published by `publish` from a
            // leaked box, is never replaced, and is freed only by `drop`,
            // which `&self` outlives.
            unsafe { published.as_ref() }
        }
        #[cfg(not(target_has_atomic = "ptr"))]
        self.table.get()
    }

    /// Publish `table` unless one already is. Another compressor may have
    /// published first; the slot keeps that table, which a compressor at its
    /// geometry attaches while one at another builds its own.
    pub(crate) fn publish(&self, table: Published<T>) {
        #[cfg(target_has_atomic = "ptr")]
        {
            use core::sync::atomic::Ordering;
            if !self.table.load(Ordering::Acquire).is_null() {
                return;
            }
            let raw = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(table));
            if self
                .table
                .compare_exchange(
                    core::ptr::null_mut(),
                    raw,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                // SAFETY: `raw` came from `Box::into_raw` just above and was
                // not published, so this is its only owner.
                drop(unsafe { alloc::boxed::Box::from_raw(raw) });
            }
        }
        #[cfg(not(target_has_atomic = "ptr"))]
        if self.table.get().is_none() {
            self.table.get_or_init(|| table);
        }
    }
}

#[cfg(target_has_atomic = "ptr")]
impl<T> Drop for DictIndexSlot<T> {
    fn drop(&mut self) {
        let published = *self.table.get_mut();
        if !published.is_null() {
            // SAFETY: published by `publish` from `Box::into_raw` and owned by
            // the slot alone; `&mut self` means no reader is left.
            drop(unsafe { alloc::boxed::Box::from_raw(published) });
        }
    }
}

/// The tables a prepared dictionary holds for the backends that index it,
/// one slot each, filled by the first compressor that builds them.
#[derive(Default)]
pub(crate) struct DictIndex {
    pub(crate) dfast: DictIndexSlot<crate::encoding::dfast::DfastDictTables>,
    pub(crate) row: DictIndexSlot<crate::encoding::row::RowDictTables>,
    pub(crate) fast: DictIndexSlot<crate::encoding::simple::fast_kernel::hash_table::FastHashTable>,
    pub(crate) dms: DictIndexSlot<crate::encoding::match_table::storage::DmsDictTables>,
}

/// Lifecycle holder for an attached, immutable dictionary table of type `T`.
#[derive(Debug, Default)]
pub(crate) struct DictAttach<T> {
    /// The immutable dictionary table, built once over the dictionary region.
    /// `Some` activates the backend's dual-probe kernel; `None` means no dict
    /// is attached (or it was invalidated, e.g. on history eviction that would
    /// stale the absolute dict positions) and the plain kernel runs. Built
    /// here while it is this holder's alone, and shared once primed.
    table: Option<Shared<T>>,
    /// Number of dictionary bytes at the front of history — one past the last
    /// valid dict position. The boundary the dual-probe kernel uses to separate
    /// live-table (input) matches from dict-table matches. `0` when unattached.
    region_len: usize,
    /// CDict-equivalent cache flag: `true` once `table` is fully built for the
    /// attached dictionary. A reused compressor keeps the built table across
    /// per-frame `reset` and skips the re-hash. Cleared on parameter change,
    /// history eviction, or dictionary attach/clear via [`Self::invalidate`].
    primed: bool,
    /// Next history position a multi-slice dict fill should process (upstream
    /// zstd `ms->nextToUpdate`). A dictionary loaded across several
    /// committed slices is hashed incrementally; without a persistent
    /// high-water the second slice's fill would restart at its own slice
    /// origin and drop the `HASH_READ_SIZE - 1` seam positions just below it
    /// (their wide hash read straddles the slice boundary, so the prior slice
    /// could not reach them). Carrying the fill origin forward keeps the
    /// stride phase continuous and closes the seam gap. `0` until the first
    /// fill; reset by [`Self::invalidate`].
    next_to_update: usize,
    /// Tables this holder started: built anew, or copied from one it shared.
    /// Test-only.
    #[cfg(test)]
    pub(crate) built: usize,
}

impl<T: Clone> Clone for DictAttach<T> {
    fn clone(&self) -> Self {
        Self {
            table: self.table.clone(),
            region_len: self.region_len,
            primed: self.primed,
            next_to_update: self.next_to_update,
            #[cfg(test)]
            built: self.built,
        }
    }

    // Recurse into the table's `clone_from` (via `Option::clone_from`) so
    // snapshot restores reuse the retained table buffers.
    fn clone_from(&mut self, source: &Self) {
        self.table.clone_from(&source.table);
        self.region_len = source.region_len;
        self.primed = source.primed;
        self.next_to_update = source.next_to_update;
        #[cfg(test)]
        {
            self.built = source.built;
        }
    }
}

impl<T: Clone> DictAttach<T> {
    /// Get the table for building, initializing it with `init` if absent.
    /// Backends call this lazily inside their per-backend prime once they know
    /// at least one position is hashable. A table is built while it is this
    /// holder's alone; one already shared is copied first rather than written
    /// under the other holders.
    #[inline]
    pub(crate) fn table_mut_or_init(&mut self, init: impl FnOnce() -> T) -> &mut T {
        #[cfg(test)]
        if self
            .table
            .as_ref()
            .is_none_or(|table| Shared::strong_count(table) > 1)
        {
            self.built += 1;
        }
        Shared::make_mut(self.table.get_or_insert_with(|| Shared::new(init())))
    }

    /// Mutable reference to the table, if attached (for the build/fill
    /// pass), under the same rule as [`Self::table_mut_or_init`].
    #[inline]
    pub(crate) fn table_mut(&mut self) -> Option<&mut T> {
        #[cfg(test)]
        if self
            .table
            .as_ref()
            .is_some_and(|table| Shared::strong_count(table) > 1)
        {
            self.built += 1;
        }
        self.table.as_mut().map(Shared::make_mut)
    }
}

impl<T> DictAttach<T> {
    pub(crate) const fn new() -> Self {
        Self {
            table: None,
            region_len: 0,
            primed: false,
            next_to_update: 0,
            #[cfg(test)]
            built: 0,
        }
    }

    /// Next history position a multi-slice dict fill should process (upstream
    /// zstd `ms->nextToUpdate`). Backends carry the fill origin forward across
    /// slices via this so the cross-slice seam positions are not dropped.
    #[inline]
    pub(crate) fn next_to_update(&self) -> usize {
        self.next_to_update
    }

    /// Record how far the dict fill has advanced (the first position not yet
    /// hashed). The next slice's fill resumes here, keeping the stride phase
    /// continuous across the slice seam.
    #[inline]
    pub(crate) fn set_next_to_update(&mut self, pos: usize) {
        self.next_to_update = pos;
    }

    /// Whether a dict table is attached (drives the dual-probe dispatch).
    #[inline]
    pub(crate) fn is_attached(&self) -> bool {
        self.table.is_some()
    }

    /// Shared reference to the dict table, if attached.
    #[inline]
    pub(crate) fn table(&self) -> Option<&T> {
        self.table.as_deref()
    }

    /// The fully built table, built from `index_floor` on, to publish in the
    /// dictionary's [`DictIndexSlot`].
    pub(crate) fn published(&self, index_floor: usize) -> Option<Published<T>> {
        self.table
            .as_ref()
            .filter(|_| self.primed)
            .map(|table| Published {
                table: Shared::clone(table),
                region_len: self.region_len,
                next_to_update: self.next_to_update,
                index_floor,
            })
    }

    /// Attach the table another compressor built for this dictionary and
    /// published in `slot`, fully built, when `accept` takes it for the
    /// dictionary about to be primed (given the table, the boundary it was
    /// built to and its index floor), as [`DictKeep::revive`] takes a parked
    /// one. Only on a holder with no table. Returns whether it was attached.
    pub(crate) fn attach_published(
        &mut self,
        slot: &DictIndexSlot<T>,
        accept: impl FnOnce(&T, usize, usize) -> bool,
    ) -> bool {
        let Some(published) = slot.get() else {
            return false;
        };
        if self.table.is_some()
            || !accept(
                &published.table,
                published.region_len,
                published.index_floor,
            )
        {
            return false;
        }
        self.table = Some(Shared::clone(&published.table));
        self.region_len = published.region_len;
        self.primed = true;
        self.next_to_update = published.next_to_update;
        true
    }

    /// The dict/input boundary (`dict_end`) for kernel bounds checks.
    #[inline]
    pub(crate) fn region_len(&self) -> usize {
        self.region_len
    }

    /// Record the dict/input boundary. Set every prime call regardless of
    /// whether any position was hashable (a sub-min-match dict still bounds the
    /// input floor).
    #[inline]
    pub(crate) fn set_region_len(&mut self, region_len: usize) {
        self.region_len = region_len;
    }

    /// CDict cache flag: `true` once the table is fully built. The prime path
    /// checks this to skip the re-hash on reused frames.
    #[inline]
    pub(crate) fn is_primed(&self) -> bool {
        self.primed
    }

    /// Mark the table fully built (CDict cache). Only marks when a table
    /// actually exists — a sub-min-match dict builds no table and must re-run
    /// the (cheap, no-op) prime path each frame.
    #[inline]
    pub(crate) fn mark_primed(&mut self) {
        if self.table.is_some() {
            self.primed = true;
        }
    }

    /// Take the table out with how far it was built (whether fully, its
    /// boundary and fill origin), leaving the state of [`Self::invalidate`].
    fn take_table(&mut self) -> Option<(Shared<T>, bool, usize, usize)> {
        let taken = self
            .table
            .take()
            .map(|table| (table, self.primed, self.region_len, self.next_to_update));
        self.invalidate();
        taken
    }

    /// Drop the cached dict table, boundary, and primed flag. Called when the
    /// next frame carries no dictionary (or on eviction/param change) so the
    /// kernel never probes a stale dict region.
    #[inline]
    pub(crate) fn invalidate(&mut self) {
        self.table = None;
        self.region_len = 0;
        self.primed = false;
        self.next_to_update = 0;
    }
}

/// Dictionary tables a backend detached and keeps for later: one fully built
/// for the dictionary it still compresses with, set aside while frames do
/// not attach it, or one kept only for its allocation. A cache of the
/// holder's, not matcher state: a clone of it is empty, so a snapshot of the
/// matcher carries none.
///
/// The table sits in a slot allocated at the first park and kept until
/// [`Self::release`], so a holder costs its matcher one pointer and a park
/// allocates nothing after the first.
#[derive(Debug, Default)]
pub(crate) struct DictKeep<T>(Option<alloc::boxed::Box<Option<Kept<T>>>>);

#[derive(Debug)]
enum Kept<T> {
    /// Fully built, with the boundary and fill origin it was built to.
    Parked {
        table: Shared<T>,
        region_len: usize,
        next_to_update: usize,
    },
    /// Built for an earlier dictionary, or only in part.
    Spare(Shared<T>),
}

impl<T> Clone for DictKeep<T> {
    fn clone(&self) -> Self {
        Self(None)
    }
}

impl<T> DictKeep<T> {
    pub(crate) const fn new() -> Self {
        Self(None)
    }

    /// The table kept, for the owner's heap accounting.
    pub(crate) fn table(&self) -> Option<&T> {
        match self.kept() {
            Some(Kept::Parked { table, .. } | Kept::Spare(table)) => Some(&**table),
            None => None,
        }
    }

    /// The bytes the slot itself takes, for the owner's heap accounting.
    pub(crate) fn slot_bytes(&self) -> usize {
        self.0
            .as_ref()
            .map_or(0, |_| core::mem::size_of::<Option<Kept<T>>>())
    }

    /// The parked table, fully built from `index_floor` on, to publish in the
    /// dictionary's [`DictIndexSlot`].
    pub(crate) fn published(&self, index_floor: usize) -> Option<Published<T>> {
        match self.kept() {
            Some(Kept::Parked {
                table,
                region_len,
                next_to_update,
            }) => Some(Published {
                table: Shared::clone(table),
                region_len: *region_len,
                next_to_update: *next_to_update,
                index_floor,
            }),
            _ => None,
        }
    }

    fn kept(&self) -> Option<&Kept<T>> {
        self.0.as_deref().and_then(Option::as_ref)
    }

    fn is_parked(&self) -> bool {
        matches!(self.kept(), Some(Kept::Parked { .. }))
    }

    fn take(&mut self) -> Option<Kept<T>> {
        self.0.as_deref_mut().and_then(Option::take)
    }

    fn store(&mut self, kept: Option<Kept<T>>) {
        match &mut self.0 {
            Some(slot) => **slot = kept,
            None if kept.is_some() => self.0 = Some(alloc::boxed::Box::new(kept)),
            None => {}
        }
    }

    /// Detach `attach`'s table, leaving the state of
    /// [`DictAttach::invalidate`], and keep it: parked when fully built, for
    /// [`Self::revive`], else as the spare unless a parked table is already
    /// kept. With no table attached, what is kept stays.
    pub(crate) fn park(&mut self, attach: &mut DictAttach<T>) {
        match attach.take_table() {
            Some((table, true, region_len, next_to_update)) => {
                self.store(Some(Kept::Parked {
                    table,
                    region_len,
                    next_to_update,
                }));
            }
            Some((table, false, ..)) if !self.is_parked() => {
                self.store(Some(Kept::Spare(table)));
            }
            _ => {}
        }
    }

    /// Attach the parked table to `attach` again, fully built, when `accept`
    /// takes it for the dictionary about to be primed (given the table and
    /// the boundary it was built to). A refused one becomes the spare.
    /// Returns whether a table was revived.
    pub(crate) fn revive(
        &mut self,
        attach: &mut DictAttach<T>,
        accept: impl FnOnce(&T, usize) -> bool,
    ) -> bool {
        let accept = |table: &Shared<T>, region_len| accept(&**table, region_len);
        if !self.is_parked() {
            return false;
        }
        let Some(Kept::Parked {
            table,
            region_len,
            next_to_update,
        }) = self.take()
        else {
            unreachable!("checked just above");
        };
        if attach.is_attached() || !accept(&table, region_len) {
            self.store(Some(Kept::Spare(table)));
            return false;
        }
        attach.table = Some(table);
        attach.region_len = region_len;
        attach.primed = true;
        attach.next_to_update = next_to_update;
        true
    }

    /// The dictionary changed: detach `attach`'s table and keep one table
    /// only as the spare, for the next dictionary's build.
    pub(crate) fn forget(&mut self, attach: &mut DictAttach<T>) {
        let kept = attach
            .take_table()
            .map(|(table, ..)| table)
            .or_else(|| self.take_any());
        self.store(kept.map(Kept::Spare));
    }

    /// The spare, for a build to empty and reuse. A parked table is not one:
    /// it stays for [`Self::revive`]. Neither is one another holder still
    /// shares: it is let go, and the build allocates.
    pub(crate) fn take_spare(&mut self) -> Option<T> {
        if !matches!(self.kept(), Some(Kept::Spare(_))) {
            return None;
        }
        match self.take() {
            Some(Kept::Spare(table)) => Shared::try_unwrap(table).ok(),
            _ => unreachable!("checked just above"),
        }
    }

    /// Drop whatever is kept, slot included.
    pub(crate) fn release(&mut self) {
        self.0 = None;
    }

    fn take_any(&mut self) -> Option<Shared<T>> {
        self.take().map(|kept| match kept {
            Kept::Parked { table, .. } | Kept::Spare(table) => table,
        })
    }
}

#[cfg(test)]
mod tests;
