# Skippable Frame Magic Allocations

RFC 8878 reserves 16 magic numbers (`0x184D2A50..=0x184D2A5F`) for
"skippable frames" — opaque user-defined payloads that compliant zstd
decoders skip past. The `structured-zstd` crate provides a typed
`SkippableFrame` API (`structured_zstd::skippable` module) for emitting
and parsing these but **does not allocate any variant**.

> **Feature gate:** the typed `SkippableFrame` API and adjacent
> storage-format extensions require enabling the `lsm` Cargo feature:
>
> ```toml
> [dependencies]
> structured-zstd = { version = "0", features = ["lsm"] }
> ```
>
> The C FFI `cdylib` build remains strict drop-in for upstream `libzstd`
> v1.5.7 regardless of which Rust features are enabled — magic-variant
> allocations affect only Rust-side typed wrappers, not the cdylib
> symbol surface.

Allocations are an application-protocol concern. Downstream consumers
embedding metadata in zstd streams should register their variants here
to prevent collisions.

<!-- The variant table below uses single leading pipes per
GitHub Flavored Markdown. Render-checked on github.com — does not
produce a spurious empty column. -->

## Allocated variants

| Variant | Magic        | Consumer                    | Purpose                                                                       | Spec |
|---------|--------------|-----------------------------|-------------------------------------------------------------------------------|------|
| 0       | `0x184D2A50` | lsm-tree wire format v1     | MetadataFrame (AEAD header for encrypted blocks); spec phase LSM-T1           | [lsm-tree #250](https://github.com/structured-world/coordinode-lsm-tree/issues/250) |
| 1       | `0x184D2A51` | lsm-tree wire format v1     | BodyFrame (encrypted payload); spec phase LSM-T1                              | [lsm-tree #250](https://github.com/structured-world/coordinode-lsm-tree/issues/250) |
| 2       | `0x184D2A52` | lsm-tree wire format v1     | EccFrame (reserved for future ECC layer); spec phase LSM-T5                   | [lsm-tree #250](https://github.com/structured-world/coordinode-lsm-tree/issues/250) |
| 3–15    | `0x184D2A53..=0x184D2A5F` | reserved by lsm-tree v1 | future versions / extensions                                                  | — |

`LSM-Tn` identifiers are spec-phase tags inside the parent lsm-tree wire-format issue; all three active variants are described in the same parent spec.

## Allocation policy

- A consumer wishing to use a previously-unallocated variant SHOULD file
  a PR against this table referencing the downstream wire-format spec.
- Variants allocated to a consumer are reserved as a contiguous range
  owned by that consumer (e.g. lsm-tree owns `0x184D2A50..=0x184D2A5F`
  in v1); the consumer may sub-allocate inside that range without
  coordinating with `structured-zstd`.
- If a consumer abandons or supersedes a variant allocation, file a PR
  to release it back to the unallocated pool.
- `structured-zstd` is the registry steward but does not validate or
  enforce semantics — that is the consumer's responsibility.
