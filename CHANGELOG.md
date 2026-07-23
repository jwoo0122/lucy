# Changelog

All notable user-facing changes are documented here from the next release onward. For earlier releases, see the [GitHub Releases](https://github.com/jwoo0122/lucy/releases) page.

This project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.8.0](https://github.com/jwoo0122/lucy/compare/v1.7.0...v1.8.0) - 2026-07-23

### Added

- add Codex subscription authentication ([#22](https://github.com/jwoo0122/lucy/pull/22))

### Other

- add logo to readme ([#21](https://github.com/jwoo0122/lucy/pull/21))

## [1.7.0](https://github.com/jwoo0122/lucy/compare/v1.6.1...v1.7.0) - 2026-07-21

### Added

- *(context)* add cwd and README.md to boot context ([#20](https://github.com/jwoo0122/lucy/pull/20))

## [1.6.1](https://github.com/jwoo0122/lucy/compare/v1.6.0...v1.6.1) - 2026-07-21

### Fixed

- enable Shift+Enter detection inside tmux via modifyOtherKeys ([#19](https://github.com/jwoo0122/lucy/pull/19))

## [1.6.0](https://github.com/jwoo0122/lucy/compare/v1.5.5...v1.6.0) - 2026-07-21

### Added

- show text-based logo with gradient on greeting screen ([#18](https://github.com/jwoo0122/lucy/pull/18))

## [1.5.5](https://github.com/jwoo0122/lucy/compare/v1.5.4...v1.5.5) - 2026-07-21

### Fixed

- gate greeting image behind LUCY_GREETING_IMAGE env flag ([#17](https://github.com/jwoo0122/lucy/pull/17))

### Other

- add site ([#16](https://github.com/jwoo0122/lucy/pull/16))

## [1.5.4](https://github.com/jwoo0122/lucy/compare/v1.5.3...v1.5.4) - 2026-07-21

### Fixed

- hide cursor before flush to prevent flicker across glow region ([#15](https://github.com/jwoo0122/lucy/pull/15))

### Other

- isolate test configuration path ([#14](https://github.com/jwoo0122/lucy/pull/14))

## [1.5.3](https://github.com/jwoo0122/lucy/compare/v1.5.2...v1.5.3) - 2026-07-20

### Fixed

- add greeting image

### Other

- change sample image

## [1.5.2](https://github.com/jwoo0122/lucy/compare/v1.5.1...v1.5.2) - 2026-07-20

### Fixed

- change glow design

### Other

- change sample image

## [1.5.1](https://github.com/jwoo0122/lucy/compare/v1.5.0...v1.5.1) - 2026-07-19

### Fixed

- refine ux

### Other

- add sample image ([#13](https://github.com/jwoo0122/lucy/pull/13))

## [1.5.0](https://github.com/jwoo0122/lucy/compare/v1.4.1...v1.5.0) - 2026-07-19

### Added

- refine ux 3
- subprocess lifecycle

### Fixed

- tui

## [1.4.1](https://github.com/jwoo0122/lucy/compare/v1.4.0...v1.4.1) - 2026-07-18

### Fixed

- minor design change

### Changed

- Added pull-request quality gates for formatting, linting, and tests.
- Added license and changelog documentation.
- Generate release notes in `CHANGELOG.md` during the version-bump workflow.
