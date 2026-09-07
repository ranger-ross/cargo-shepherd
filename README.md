# Cargo Storage

[![CI Status](https://github.com/ranger-ross/cargo-storage/workflows/Test/badge.svg)](https://github.com/ranger-ross/cargo-storage/actions)
[![Crates.io](https://img.shields.io/crates/v/cargo-storage.svg)](https://crates.io/crates/cargo-storage)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/ranger-ross/reqwest-metrics/blob/master/LICENSE)

Cargo storage management CLI/TUI

<img src="assets/demo.gif" width="800" alt="demo" />

## Features

* Very fast directory discovery
* Management TUI with realtime updates
* Headless mode for listing and cleaning
* `clean` command with reasonable defaults
* Support for `.cargo/config.toml` overrides. (best effort)
* Support for both Cargo `build-dir` and `target-dir`


## Installation

```shell
cargo install cargo-storage
```

## Usage

```shell
# Open the management TUI
cargo storage

# List the target directories w/o the TUI
cargo storage list

# Clean the target directories (defaults to older than 30d AND at least 100MB)
cargo storage clean

# Run --help to see more options
cargo storage --help
```

## Motivation / Scope

This project was motivated primarily by me needing a way to monitor directory sizes while working on a
cross workspace cache for Cargo. Other solutions existed but they primarily focused on cleaning up unused `target` directories.
I set out just to solve my problem but added on some additional features like cleaning up old target dirs since it was fairly easy
to do so once the core was in place. In the future I'd like to make this tool a general management solution for Cargo's on disc files.
If you have an idea or issue, please raise it on the GitHub issue tracker.
