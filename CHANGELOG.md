# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1](https://github.com/ranger-ross/cargo-storage/compare/v0.4.0...v0.4.1) - 2026-09-09

### Fixed

- Enable 60fps during deletes so loading is smooth

## [0.4.0](https://github.com/ranger-ross/cargo-storage/compare/v0.3.1...v0.3.2) - 2026-09-08

### Fixed

- Fixed tests on windows
- Fixed linting errors
- fix the thing
- fixed a thing
- fixed race condition
- fixed Deleting... text from not being removed once complete
- fixed typo

### Other

- reduce cpu usage a bit
- Spend some time optimizing and made it even faster
- setup release-plz and trusted publishing
- Made the deleting styling match the scanning styling
- clean up
- correct selection adjustments
- implement asynchronous deletion
- rename to is_being_deleted
- update Delete action handler
- track deletion state in TargetEntry
- extend App to keep entries sorted by target_path
- unify filtering and sorting
- Added config file with figment
