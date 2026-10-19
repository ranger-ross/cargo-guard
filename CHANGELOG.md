# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/ranger-ross/cargo-guard/compare/v0.1.1...v0.2.0) - 2026-10-19

### Fixed

- [**breaking**] Dropped support for fingerprint based approvals

## [0.1.1](https://github.com/ranger-ross/cargo-guard/compare/v0.1.0...v0.1.1) - 2026-10-19

### Other

- added limitation about proc-macros
- *(deps)* update rust crate vt100 to 0.18.0
- Updated README with binstall and motivations
- Enable cargo-guard in this repo
- Approved builds from windows
- Approved builds on linux
- Approved builds from macos
- Update limitations section in README

## [0.1.0](https://github.com/ranger-ross/cargo-guard/compare/v0.0.1...v0.1.0) - 2026-10-18

### Added

- [**breaking**] Added support for workspace autodiscovery

### Fixed

- Cleaned up error messages
- Fixed cargo alias support
- *(tests)* send complete ConPTY cursor responses
- *(tests)* compare approval destinations across platform path aliases

### Other

- Update README
- Added e2e test for fingerprint survival
- Added e2e test for cargo directives
- Added review e2e test
