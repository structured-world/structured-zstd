# Contributing to structured-zstd

Bug reports, fixes, performance work, documentation and new capabilities are
welcome.

## Development setup

```bash
git clone https://github.com/structured-world/structured-zstd.git
cd structured-zstd
cargo build -p structured-zstd --features hash,std,dict-builder
```

Before opening a pull request, run what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy -p structured-zstd --features hash,std,dict-builder -- -D warnings
cargo nextest run -p structured-zstd --features hash,std,dict-builder
cargo test --doc -p structured-zstd --features hash,std,dict-builder
cargo clippy -p structured-zstd --no-default-features -- -D warnings
```

The comparison against the C reference (`ffi-bench`) needs
`--features bench-internals,dict-builder`.

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
