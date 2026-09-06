# Reviewing this codebase for performance

structured-zstd is a compression codec. Its hot paths run per input byte, per
scanned position, or per emitted sequence, and the reference implementation it
competes with is a mature C library. A change that is correct and readable can
still be wrong here if it does avoidable work in one of those loops.

This file is a checklist for reviewing that dimension. It is about performance
only; correctness, API design and style are reviewed elsewhere.

Two framing rules before the checklist:

- **Scope is not risk.** "Large refactor", "many lines", "the win looks small"
  are not grounds to reject a change or to defer one. A failed measurement, a
  correctness problem, or a broken invariant are.
- **A cost that is removable and whose removal leaves the design cleaner is
  worth removing, even when a benchmark cannot resolve it.** Tail latency is
  made of individually invisible costs. What is NOT acceptable is claiming such
  a change is faster without a measurement that says so.

---

## 1. Borrow instead of cloning

The single most common defect. Ask, for every value crossing a boundary in a
hot path:

- **Is it duplicated only to be read and dropped?** Read it in place.
- **Is a reference-counted handle (`Arc`, `Rc`) cloned per item, per block or
  per frame, when the callee only looks at it?** Each clone is an atomic
  increment and a matching decrement. Pass the reference. The reference
  implementation borrows its dictionary tables by raw pointer from a
  caller-owned structure and never takes ownership; a design that holds an
  owned handle pays what it does not.
- **Is a whole structure copied to hand over one field?** Pass the field.
- **Does the copy exist only to satisfy an ownership requirement that a
  different signature would remove?** Change the signature. The borrow checker
  resisting is a design signal, not a reason to clone.

**The inverse is also a defect, so check both directions.** A borrow into a
large shared buffer that outlives the operation pins the whole buffer. If the
result is stored (cached, queued, returned upward) while the buffer it views
is much larger, copy the bytes out. Retention is a cost too, and one an
allocator's accounting will not show you.

## 2. Allocation

Count allocations per unit of work: per byte, per position, per sequence, per
block, per frame. Per-item allocation on a hot path is the finding; per-frame is
usually fine.

- **A buffer that is cleared but not reserved climbs the doubling ladder on
  every frame.** Reserve at the bound once and let `clear()` keep the capacity.
- **An intermediate collection whose only purpose is to reshape another** should
  not exist; build the final shape once.
- **Scratch buffers belong to the caller.** A helper called per block or per
  sequence must not create its own workspace. Take `&mut` scratch that lives on
  the compressor state, so it is allocated once and reused across blocks and
  frames.
- **Do not pre-allocate on a path that may never use the buffer.** An
  unconditional allocation serving a conditional need is the same defect facing
  the other way.
- **If a structure is retained across frames, its memory must be reported.**
  This crate exposes context size through the C API; a caller budgets against
  that figure. Anything held between frames belongs in `heap_size`, and
  `heap_size` means bytes on the heap: count `capacity()`, never the inline
  size of a struct that lives inside another.

## 3. Arithmetic: `checked_*` yes, `saturating_*` almost never

**`saturating_*` is not a safety measure.** It silently pins a result at the
type's bound, turning a bug into corrupted data downstream instead of a loud
failure at the cause. Reject it unless clamping at the bound is the stated
business rule, and then require a comment saying so.

Use instead:

- `checked_*` with explicit handling (`expect` on an invariant, an `Err`, a
  branch) so bad input fails where it originates;
- plain arithmetic with a comment justifying why the bounds provably hold, and
  a `debug_assert` pinning the invariant.

**On a hot path, a gate is a BRANCH, not a value.** Do not materialise something
you only need to make an early exit from. Write the rejection as a chain of bare
comparisons ordered most-selective first, so the common outcome falls through.
This applies to every wrapping form, not just arithmetic: `checked_* ` plus
`if let Some`, `Option`/`Result` as a gate, `.then()`, `map_or` in place of a
branch, `find`/`any` where a loop with `break` belongs. `saturating_*` in a gate
is the worst of them: it both masks the violation and replaces the branch with
an unconditional conditional-move, costing the early exit.

The converse is worth knowing: a branch that is genuinely unpredictable, where
a miss costs more than computing both sides, is better branchless. Predictable
branch stays a branch; unpredictable goes branchless; either way the claim is
settled by measurement, not taste.

## 4. Overhead that rides along

- **Synchronisation on a value read far more often than written**, or written
  once. Latch it locally or narrow the scope. A `OnceLock` read inside a
  per-position loop is an atomic per position.
- **A general-purpose call used for a narrower question**: a lookup that returns
  and clones when only presence was asked; a sort where an ordering check would
  do; a full parse where a length would do. Watch especially for calls that
  also mutate hidden state (usage counters, eviction bookkeeping): using one as
  a probe corrupts that state as well as costing time.
- **Work repeated across layers**: the same value computed, decoded or validated
  twice on one path because two functions each did it defensively.
- **Work done eagerly that the caller may never ask for**, where the trigger is
  knowable.
- **Loop-invariant work inside the loop.** The optimiser fails to hoist far more
  often than intuition suggests, because anything that obscures invariance
  defeats it: an interior write through `&mut self`, an iterator or closure
  borrowing the structure for the loop's lifetime, or a method call whose body
  reads fields it cannot prove unchanging. Symptom on sight: a `self.`-field
  read or a method call inside a per-item loop whose arguments are all
  invariant. Bind it above the loop.

## 5. Dispatch and specialisation

- **Feature detection happens once, before the operation; dispatch happens
  once, above the loop.** These are two separate things and both get asked
  about. The detector is answered at construction and stored in a field, so no
  encode or decode ever calls it — not per position, and not per block either:
  a cached selector still costs an atomic load and a branch each time, and its
  presence in the middle of the work invites callers to add more. What may sit
  above the loop is the `match` on the already-resolved value, selecting a
  monomorph that then runs with the tier and the const-shaped parameters baked
  in and no test for them in the body.

  **The reason for the monomorph is register pressure, not the branch.** A
  predictable branch is nearly free; a value is not. A carried "which SIMD do we
  have" tag is a live value for the whole loop, and these loops already run
  saturated, holding about as many live values as the machine has registers.
  One more forces the allocator to spill something hot, and the reload is then
  paid every iteration by code that has nothing to do with the tier. Baking the
  tier into the instantiation means no such value exists to be carried. Same
  argument as the coordinate-system rule below, applied to dispatch. It also
  says where the line falls: a tag that never enters the saturated loop, read
  only on a rare path or from a closure's environment, costs nothing worth
  duplicating a loop body over.
- **Which kernels EXIST is a compile-time question; which one RUNS is not.**
  The tiers a build carries follow from the target and the `kernel-*` features.
  Choosing among them is a property of the CPU executing the binary, so it is
  answered by runtime detection, resolved once ahead of the work and carried
  down as a value.

  `#[cfg(target_feature = ...)]` cannot answer it. That flag reflects the
  build's baseline (the target spec plus `-C target-cpu` / `RUSTFLAGS`), not the
  host that compiled it and not the host that will run it: a plain `cargo build`
  on an AVX2 workstation does NOT set `target_feature = "avx2"` for
  `x86_64-unknown-linux-gnu`. Using it to select a tier therefore fails in both
  directions. Raise the baseline and the artifact executes an illegal
  instruction on older CPUs; leave it at stock and the wide tier is compiled out
  entirely, so the code silently takes a narrower path, possibly all the way
  down to the `memcpy` call a routine was written to avoid. The second failure
  is the quiet one, and it is invisible on aarch64.

  A `cfg` gate is legitimate only where the feature is guaranteed by the target
  itself (NEON on aarch64, SSE2 on x86_64): there is nothing to detect, and the
  gate states an architectural fact rather than making a choice. It is also the
  only option under `no_std`, where no detection exists — there the baseline
  genuinely is all the evidence available, and that limitation belongs in a
  comment.

  Require of every SIMD path: each tier present in the artifact independently of
  the baseline (`#[target_feature(enable = ...)]` on the kernel, not a `cfg` on
  its existence), a scalar fallback, and a test sweeping EVERY tier the running
  CPU supports against that fallback for bit-identical output — not just the one
  the detector happens to prefer, or the narrow tiers go untested on the
  machines that run them.
- **More monomorphisation is not automatically better.** Duplicating a body into
  many instantiations, or inlining a helper called from a dozen sites inside a
  loop, buys instruction-cache and decode pressure that can exceed the calls it
  saves. In a match-emitting loop the productive direction is making a shared
  helper do less, not pulling it inline.

## 6. Structure of a hot loop

- **Carry one coordinate system.** If a loop holds conversion constants between
  several (a relative cursor, an absolute position, an index into a buffer),
  those constants are the register pressure, because each must stay live for the
  whole loop. Fold the conversion into a single base that the loop's index adds
  to directly, and reconstruct the other systems only on the paths that need
  them.
- **Watch what crosses a per-iteration call boundary.** A struct larger than two
  words is returned and passed through memory with a copy at every call; an
  `Option` tag is enough to push a payload over that line. Pass a reference at
  the deepest crossing, or shrink the type. Pass a raw pointer rather than a
  slice where the callee never needs the length.
- **One out-of-line function per position, not a chain of them.** Nested
  per-position calls cost more than the single function they replace. Equally, a
  register-hungry helper inlined into a hot caller spills the caller's own live
  values, so out-of-line with a small return can beat inline.

## 7. What to require of a performance claim

A reviewer should not accept, and an author should not offer, any of these:

- **"Fewer instructions, therefore faster."** Instruction count is a
  diagnostic, not a verdict. Instructions are not equally priced: addressing
  mode, dependency chain, port pressure and sign-extension all change what one
  costs, and changes that cut instructions while raising cycles are common here.
  Cycles decide.
- **An instruction count compared across a boundary that moved.** Counts are
  comparable whole-program against whole-program on the same input and output,
  or one function against itself. Inlining moves work between symbols without
  changing its cost.
- **A cycle figure from separately built binaries, below roughly one and a half
  percent.** Two builds of the same source differ by about that much from code
  layout alone. Such a delta needs a second measurement on a path the change
  cannot execute, with the difference of the two being the attributable figure.
  Below the band, decide on the architecture and say the timing is not
  established, in either direction.
- **A figure measured on a fixture that does not exercise the changed code.**
  Compressible, incompressible, dictionary-primed and tiny-frame inputs take
  different paths end to end.
- **A figure given only against another revision of this codebase.** The
  reference is upstream through the C bindings, measured in the same harness on
  the same host and in the same run. A revision of our own can be fast because
  it is doing less than the format asks for, so "faster than what we had" is
  not a result on its own: state both, ours against upstream and ours against
  the revision, and when the two disagree the upstream comparison is the one
  that decides whether the change is good.

Require instead: what changed, on which fixture, against upstream, with cycles
and instructions both, and for anything under a couple of percent, what the
control arm said.

## 8. Correctness constraints a performance change must not break

- **Output that changes is a different change** and must be argued as one, with
  a compressed-size comparison across levels and fixture shapes rather than a
  byte-identity check. When output does not change, prove it: compare the md5 of
  the frames, not their lengths, on at least two fixture shapes including one
  that exercises the touched path.
- **A removed runtime check that is provable becomes a `debug_assert`.** The
  test suite then exercises the invariant on every fixture and level at no
  release cost, which is a far stronger argument than the reasoning that
  justified removing it.
- **An invariant that holds within one frame may not hold across a reused
  context.** State that survives between frames (hash tables, cached
  descriptions, retained buffers) is where "cannot happen by construction"
  reasoning goes wrong. Ask what a second, shorter frame on the same context
  sees.
- **Pointer arithmetic that leaves its allocation is undefined even when never
  dereferenced.** `offset` and `add` require every intermediate to stay in
  bounds; a folded base pointer that steps before the buffer needs
  `wrapping_offset` / `wrapping_add`, with the gate placing the final address
  back inside before the read.
- **A heuristic that skips the search must be paired with what makes being
  wrong survivable.** A sample cannot see a repeat that spans more than the
  sample, so a check reading only the block's own bytes writes off input that
  would have compressed, and the cost is a factor rather than a percent. Two
  things make the trade sound, and both are load-bearing: a probe of what the
  frame has already emitted, so a block duplicating an earlier one is searched
  however random it looks; and indexing whatever is skipped, so the later
  duplicate has something to match against — skipping without indexing loses
  the repeat on both blocks and no probe can recover it.

  Confining the skip to a band of levels on top of that is NOT part of the
  rule, and has been measured twice: it costs more than an order of magnitude
  on incompressible input at the upper levels and recovers nothing, including
  on input built specifically to make the classifier answer wrongly. If a
  reviewer proposes it again, re-measure rather than reason — the numbers live
  in the commit that removed it, not here.
