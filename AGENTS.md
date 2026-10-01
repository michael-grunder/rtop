# rtop Agent Guide

## Goals

* Prefer modular, reusable code over duplication unless there is a clear
  performance reason not to.
* Prefer defensive programming. Do not ignore fallible return values unless
  there is a deliberate and documented reason.
* Use idiomatic Rust abstractions when they improve the design, especially
  traits, generics, and well-scoped types.
* Prefer established Cargo crates over custom implementations unless there is a
  strong reason to build in-house.
* Keep the architecture clean when adding features. If a new feature exposes a
  structural problem, redesign the relevant area instead of scattering special
  cases through the codebase.

## Required Before Completion

These are mandatory completion steps, not optional guidance.

* Run `cargo test`.
* Run `cargo clippy` and address the reported issues.
* Run `cargo fmt` and address the reported issues.
* Add or update tests when the change affects behavior.
* Update `README.md` when user-facing behavior or documented workflows change.
* Update `CHANGELOG.md` under `## Unreleased`.

## Changelog Format

Group unreleased changes under one or more of these headings as applicable:

* `### Added`
* `### Changed`
* `### Fixed`
* `### Deprecated`
* `### Removed`
