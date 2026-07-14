# Pull Request

## Summary

<!-- What changed? Keep this focused on one logical change. -->

## Why

<!-- What problem does this solve? Link a GitHub issue or project ticket when applicable. -->

## Design and compatibility

<!-- Name the affected layers/crates and any public API, capability, persistence, migration, feature, security, or rollout implications. Use "None" when not applicable. -->

## Testing

<!-- List commands run and the important normal/error/edge cases covered. -->

## Checklist

- [ ] I followed the applicable guidance in [CONTRIBUTING.md](https://github.com/RadiantAILabs/polaris/blob/main/CONTRIBUTING.md) and, for complex changes, the [Pull Request Review Standards](https://github.com/RadiantAILabs/polaris/blob/main/docs/contributing/review-standards.md).
- [ ] This pull request contains one logical change.
- [ ] I added or updated tests for changed behavior and failure paths.
- [ ] I updated public rustdoc, reference docs, catalogs, and generated files where applicable.
- [ ] I assessed architecture, compatibility, migration, feature, and security implications.
- [ ] I ran the applicable local checks and listed them above; remaining full-gate checks may run in CI.
- [ ] Change-specific checks such as `cargo make test-features` or the examples check pass where applicable.
