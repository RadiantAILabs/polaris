# Contributing to Polaris

Thank you for contributing. This guide covers the common path from a proposed change to a pull request. Apply only the guidance relevant to your change.

## Choose a contribution path

Small fixes, documentation improvements, and isolated tests may go directly to a pull request.

Open a GitHub issue before implementation when a change:

- Adds or substantially changes a Layer 1 or Layer 2 primitive
- Introduces a significant public API or capability contract
- Changes persisted data, compatibility behavior, or migration requirements
- Adds a crate or cross-cutting dependency
- Requires coordinated rollout or affects multiple architecture layers

Describe the problem, important constraints, and expected behavior. If you are unsure whether discussion is needed, opening an early pull request is also welcome.

## Development setup

Polaris uses Rust 2024 and has a minimum supported Rust version (MSRV) of 1.97.0.

```bash
rustup toolchain install 1.97.0 --component rustfmt --component clippy
cargo install cargo-make rumdl
```

Fork and clone the repository, then build it:

```bash
git clone https://github.com/<your-user>/polaris.git
cd polaris
rustup override set 1.97.0
cargo build
```

Install `cargo-hack` only when changing Cargo features or feature propagation:

```bash
cargo install cargo-hack
```

## Make a focused change

Keep each pull request to one logical change and avoid unrelated cleanup.

Record notable changes concisely under the [`Unreleased`](CHANGELOG.md#unreleased) section of the changelog.

For architecture changes, read the [core philosophy](docs/philosophy.md) and [taxonomy](docs/taxonomy.md). In particular:

- Keep Layer 1 domain-neutral and Layer 2 pattern-neutral.
- Put optional integrations and capabilities in Layer 3 plugins.
- Keep state in resources, behavior in systems, and material routing visible in graph topology.
- Declare plugin capabilities, dependencies, and ordering explicitly.

Prefer one crate and one architecture layer per pull request. Wider changes are welcome when splitting them would make the implementation less coherent or safe.

Follow the workspace's enforced Rust conventions: format with rustfmt, resolve Clippy warnings, document public items, use `tracing` instead of direct printing, and explain any lint exceptions or unsafe invariants.

## Test the change

During development, run the narrowest checks that exercise the changed behavior. Features and fixes should include tests at the layer that owns the affected invariant.

Common checks include:

```bash
cargo fmt -- --check
cargo test -p <affected-crate>
cargo clippy -p <affected-crate> --all-targets --all-features -- -D warnings
```

Run the complete gate when practical before final review:

```bash
cargo make test
```

CI runs the full required gate, including formatting, Clippy, Markdown, rustdoc, doctests, workspace tests, examples, and generated binding drift. A pull request may be opened before the complete local gate finishes.

For Cargo feature changes, also run:

```bash
cargo make test-features
cargo run -p feature-parity
```

Regenerate TypeScript bindings with `cargo make gen-bindings` when changing a `ts-rs`-derived public type.

## Update public documentation

Update documentation in the same pull request when behavior, public APIs, configuration, features, or integration patterns change.

Exported plugins, APIs, and consumer-facing resources have specific documentation and catalog requirements:

- [Plugin documentation standard](docs/reference/plugins.md#documentation-standard)
- [API documentation standard](docs/reference/api.md#documentation-standard)
- [Resource documentation standard](docs/reference/resources.md#documentation-standard)

Update the [integration guide](docs/reference/guide.md) when the change introduces a new way for downstream users to accomplish a goal.

## Submit the pull request

Complete the pull request template with:

- The problem and why the change is needed
- The resulting behavior or contract
- Tests run and important cases covered
- Relevant architecture, compatibility, migration, feature, security, or operational effects
- Documentation or generated files updated
- A linked issue when prior discussion was needed

Use an imperative, typed title such as `feat: add scoped session cleanup`. Individual commit formatting is not enforced because maintainers may squash commits.

For complex or foundational changes, self-review against the detailed [Pull Request Review Standards](docs/contributing/review-standards.md). They help contributors and maintainers evaluate architecture, Rust APIs, tests, documentation, compatibility, and security consistently.

## Questions

Open a GitHub issue or draft pull request when the expected architecture or contribution path is unclear. Early discussion is especially useful for Layer 1, Layer 2, persistence, compatibility, and security changes.
