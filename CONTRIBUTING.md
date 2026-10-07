# Contributing to structured-zstd

Bug reports, fixes, performance work, documentation and new capabilities are
welcome.

## Development setup

```bash
git clone https://github.com/structured-world/structured-zstd.git
cd structured-zstd
cargo build -p structured-zstd --features hash,std,dict-builder
```

Tests run under [cargo-nextest](https://nexte.st/), which is not part of the
Rust toolchain:

```bash
cargo install cargo-nextest --locked
```

Before opening a pull request, run what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy -p structured-zstd --features hash,std,dict-builder -- -D warnings
cargo nextest run --profile ci -p structured-zstd --features hash,std,dict-builder
cargo test --doc -p structured-zstd --features hash,std,dict-builder
cargo clippy -p structured-zstd --no-default-features -- -D warnings
cargo clippy -p structured-zstd --no-default-features --features hash -- -D warnings
```

The parity and cross-validation tests against the C reference live in
`ffi-bench`:

```bash
cargo nextest run --profile ci -p ffi-bench --features bench-internals,dict-builder
```

The timing comparison against the C reference is built without
`bench-internals`, so that it measures the same build a user of the crate gets:

```bash
cargo bench -p ffi-bench --bench compare_ffi
```

## Pull requests

1. Create a branch from `main`.
2. Make the change, with tests. A bug fix comes with a test that fails without
   it.
3. Write commit messages and the pull request title in the
   [Conventional Commits](https://www.conventionalcommits.org/) form; pull
   requests are squash-merged, so the title becomes the commit on `main`.
4. A change to a hot path states what it measured, on which input, against the
   C reference; [AGENTS.md](AGENTS.md) lists what a performance change is
   reviewed for.

## Contributor License Agreement (CLA)

Before a first pull request can be merged, you sign the Structured World
Contributor License Agreement once, at <https://sw.foundation/cla>. It covers
every repository of the organisation and takes a minute: sign in with GitHub,
confirm your e-mail address, sign. The `CLA` status on your pull request then
turns green by itself.

You keep the copyright in your contribution. If you contribute as part of your
job, your employer may also need to sign the corporate agreement; the page
above explains when.

## Security

Do not report vulnerabilities in public issues; see [SECURITY.md](SECURITY.md).
