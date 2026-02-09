# Contributing to Fighters Paradise

Thank you for your interest in contributing! This document covers the conventions and process for contributing to the project.

## Getting Started

1. Fork the repository
2. Clone your fork and create a feature branch
3. Install prerequisites: Rust (edition 2021) and SDL2
4. Run `cargo test --workspace` to make sure everything passes

## Code Conventions

### Rust Style

- **Edition 2021**, resolver v2
- `#![warn(missing_docs)]` on every crate — all public items need `///` doc comments
- Module-level `//!` docs in every `lib.rs` explaining the crate's role
- Use `thiserror` for error types; all errors are `FpError` variants, never `panic!`
- Use `tracing` for logging (`tracing::info!`, `tracing::warn!`), not `println!`
- Dependencies are declared at workspace level in root `Cargo.toml` and inherited via `.workspace = true`

### Error Philosophy

MUGEN community content is messy. Parsers must:

- Return `FpResult<T>` (never panic)
- Log warnings with `tracing::warn!` for recoverable issues
- Substitute safe defaults (missing sprite = invisible, bad expression = 0)
- Only return `Err` when loading truly cannot continue

### Testing

- Unit tests in each module via `#[cfg(test)] mod tests`
- Test binary parsers with synthetic byte arrays constructed inline
- `cargo test --workspace` and `cargo clippy --workspace` must both pass clean

## Before Submitting

```bash
cargo test --workspace           # All tests pass
cargo clippy --workspace         # Zero warnings
```

## Pull Request Process

1. Create a branch from `main` with a descriptive name
2. Make your changes, following the code conventions above
3. Add tests for new functionality
4. Ensure all tests pass and clippy is clean
5. Write a clear PR description explaining what and why

## Architecture

See [docs/architecture.md](docs/architecture.md) for an overview of the crate structure and design decisions. The [CLAUDE.md](CLAUDE.md) file contains detailed technical reference for the codebase.
