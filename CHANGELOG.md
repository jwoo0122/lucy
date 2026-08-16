# Changelog

All notable user-facing changes are documented here from the next release onward. For earlier releases, see the [GitHub Releases](https://github.com/jwoo0122/lucy/releases) page.

This project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.15.3](https://github.com/jwoo0122/lucy/compare/v1.15.2...v1.15.3) - 2026-08-16

### Fixed

- Reduce boot context and widen TUI ([#112](https://github.com/jwoo0122/lucy/pull/112))

## [1.15.2](https://github.com/jwoo0122/lucy/compare/v1.15.1...v1.15.2) - 2026-08-15

### Fixed

- bound auto-compaction fallback requests ([#94](https://github.com/jwoo0122/lucy/pull/94))

## [1.15.1](https://github.com/jwoo0122/lucy/compare/v1.15.0...v1.15.1) - 2026-08-15

### Fixed

- steer queued TUI messages into active turns ([#93](https://github.com/jwoo0122/lucy/pull/93))

## [1.15.0](https://github.com/jwoo0122/lucy/compare/v1.14.10...v1.15.0) - 2026-08-15

### Added

- lucy exec ([#92](https://github.com/jwoo0122/lucy/pull/92))

### Other

- update adr docs ([#91](https://github.com/jwoo0122/lucy/pull/91))

## [1.14.10](https://github.com/jwoo0122/lucy/compare/v1.14.9...v1.14.10) - 2026-08-14

### Fixed

- improve session resume and TUI interactions ([#90](https://github.com/jwoo0122/lucy/pull/90))

## [1.14.9](https://github.com/jwoo0122/lucy/compare/v1.14.8...v1.14.9) - 2026-08-14

### Fixed

- improve TUI rendering and text selection ([#89](https://github.com/jwoo0122/lucy/pull/89))

## [1.14.8](https://github.com/jwoo0122/lucy/compare/v1.14.7...v1.14.8) - 2026-08-12

### Fixed

- restore pre-v1.14.6 TUI behavior ([#88](https://github.com/jwoo0122/lucy/pull/88))

## [1.14.7](https://github.com/jwoo0122/lucy/compare/v1.14.6...v1.14.7) - 2026-08-08

### Fixed

- WSL Codex login browser launch ([#86](https://github.com/jwoo0122/lucy/pull/86))

## [1.14.6](https://github.com/jwoo0122/lucy/compare/v1.14.5...v1.14.6) - 2026-08-06

### Fixed

- Replace fullscreen TUI with inline terminal view ([#85](https://github.com/jwoo0122/lucy/pull/85))

## [1.14.5](https://github.com/jwoo0122/lucy/compare/v1.14.4...v1.14.5) - 2026-08-05

### Fixed

- *(session)* validate lifecycle integration

### Other

- build clean final reliability branch
- restack final reliability PR
- start reliability stack restack
- restack reliability PRs on main
- restore standard CI definition
- *(session)* isolate turn journals from transcripts
- trigger transcript fixture validation
- validate transcript fixture selection
- robustly patch early diagnostic pipe handling
- tolerate expected early diagnostic pipe closure
- scope provider environment assertion patch
- tolerate formatted integration assertions
- validate lifecycle integration against current security contract
- make wrapper compatibility patch robust
- apply wrapper compatibility fix in CI
- validate and apply current rustfmt
- *(session)* apply current rustfmt output
- *(session)* make lifecycle storage wrapper-compatible
- *(session)* wrap transcript storage for lifecycle extensions
- *(session)* persist lifecycle in private journal
- *(session)* split lifecycle storage module
- *(session)* add turn lifecycle model

## [1.14.4](https://github.com/jwoo0122/lucy/compare/v1.14.3...v1.14.4) - 2026-08-04

### Fixed

- *(tui)* pad background task indicator ([#80](https://github.com/jwoo0122/lucy/pull/80))
- *(tui)* show final agent message in turn notification ([#79](https://github.com/jwoo0122/lucy/pull/79))

## [1.14.3](https://github.com/jwoo0122/lucy/compare/v1.14.2...v1.14.3) - 2026-07-29

### Fixed

- *(security)* remove active provider credential from command child ([#59](https://github.com/jwoo0122/lucy/pull/59))

## [1.14.2](https://github.com/jwoo0122/lucy/compare/v1.14.1...v1.14.2) - 2026-07-28

### Fixed

- *(site)* eliminate horizontal scroll on narrow/mobile viewports ([#52](https://github.com/jwoo0122/lucy/pull/52))

### Other

- refine Lucy homepage ([#50](https://github.com/jwoo0122/lucy/pull/50))

## [1.14.1](https://github.com/jwoo0122/lucy/compare/v1.14.0...v1.14.1) - 2026-07-28

### Fixed

- follow symlinked skills ([#49](https://github.com/jwoo0122/lucy/pull/49))

## [1.14.0](https://github.com/jwoo0122/lucy/compare/v1.13.3...v1.14.0) - 2026-07-28

### Added

- *(provider)* enable OpenRouter prompt caching ([#48](https://github.com/jwoo0122/lucy/pull/48))

## [1.13.3](https://github.com/jwoo0122/lucy/compare/v1.13.2...v1.13.3) - 2026-07-28

### Fixed

- *(auth)* trust native roots for Codex OAuth ([#47](https://github.com/jwoo0122/lucy/pull/47))

## [1.13.2](https://github.com/jwoo0122/lucy/compare/v1.13.1...v1.13.2) - 2026-07-28

### Fixed

- remove capability discovery ([#46](https://github.com/jwoo0122/lucy/pull/46))

## [1.13.1](https://github.com/jwoo0122/lucy/compare/v1.13.0...v1.13.1) - 2026-07-28

### Fixed

- *(tui)* adapt colors to terminal background ([#44](https://github.com/jwoo0122/lucy/pull/44))

## [1.12.3](https://github.com/jwoo0122/lucy/compare/v1.12.2...v1.12.3) - 2026-07-26

### Fixed

- drop orphaned tool calls on session resume
- strip expired encrypted reasoning on Codex session resume

## [1.12.2](https://github.com/jwoo0122/lucy/compare/v1.12.1...v1.12.2) - 2026-07-25

### Fixed

- deliver background cmd completions as low-privilege observations

## [1.12.1](https://github.com/jwoo0122/lucy/compare/v1.12.0...v1.12.1) - 2026-07-25

### Fixed

- *(tui)* sort sessions with sort_by_key to satisfy clippy::unnecessary_sort_by

### Other

- Merge branch 'main' into feat/session-command

## [1.12.0](https://github.com/jwoo0122/lucy/compare/v1.11.0...v1.12.0) - 2026-07-25

### Added

- *(tui)* show a running background task indicator under the console

## [1.11.0](https://github.com/jwoo0122/lucy/compare/v1.10.1...v1.11.0) - 2026-07-25

### Added

- *(codex)* default to the model's max context window

## [1.10.1](https://github.com/jwoo0122/lucy/compare/v1.10.0...v1.10.1) - 2026-07-25

### Fixed

- drop unsigned thinking fragments before sending them back

### Other

- Merge origin/main into issue/8

## [1.10.0](https://github.com/jwoo0122/lucy/compare/v1.9.1...v1.10.0) - 2026-07-25

### Added

- support background commands

### Other

- Merge pull request #33 from jwoo0122/issue/29

## [1.9.1](https://github.com/jwoo0122/lucy/compare/v1.9.0...v1.9.1) - 2026-07-25

### Fixed

- *(tui)* keep scrollbar outside transcript

## [1.9.0](https://github.com/jwoo0122/lucy/compare/v1.8.4...v1.9.0) - 2026-07-25

### Added

- load Codex model metadata from server

## [1.8.4](https://github.com/jwoo0122/lucy/compare/v1.8.3...v1.8.4) - 2026-07-25

### Fixed

- *(tui)* add subtle prompt background
- *(tui)* remove prompt background effects

## [1.8.3](https://github.com/jwoo0122/lucy/compare/v1.8.2...v1.8.3) - 2026-07-25

### Fixed

- *(tui)* restore context usage graph
- *(tui)* narrow content area by ten columns
- *(tui)* remove context usage bar
- *(tui)* show transcript scrollbar and speed up tool fades
- *(tui)* align context status and place busy indicator
- *(tui)* slow model graph animation
- *(tui)* fade model graph trail with equal blocks
- *(tui)* animate model status only while busy

### Other

- Merge origin/main into fix/tui-style

## [1.8.2](https://github.com/jwoo0122/lucy/compare/v1.8.1...v1.8.2) - 2026-07-25

### Fixed

- move agent orchestration outside Lucy ([#24](https://github.com/jwoo0122/lucy/pull/24))

## [1.8.1](https://github.com/jwoo0122/lucy/compare/v1.8.0...v1.8.1) - 2026-07-25

### Fixed

- *(tui)* simplify console surface and animate model status ([#23](https://github.com/jwoo0122/lucy/pull/23))

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
