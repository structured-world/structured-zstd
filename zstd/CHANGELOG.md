# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.3](https://github.com/structured-world/structured-zstd/compare/v0.0.2...v0.0.3) - 2026-03-26

### Added

- *(encoder)* FSE table reuse and offset history optimization ([#33](https://github.com/structured-world/structured-zstd/pull/33))
- large literals block support (>262KB) ([#30](https://github.com/structured-world/structured-zstd/pull/30))

### Fixed

- *(encoding)* implement default compression level ([#34](https://github.com/structured-world/structured-zstd/pull/34))
- use local Readme.md for crate readme

## [0.0.2](https://github.com/structured-world/structured-zstd/compare/v0.0.1...v0.0.2) - 2026-03-25

### Added

- managed fork setup — README, FUNDING, crate rename to structured-zstd ([#2](https://github.com/structured-world/structured-zstd/pull/2))
