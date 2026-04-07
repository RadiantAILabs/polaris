# polaris_macro_utils

Shared utilities for Polaris procedural macro crates.

## Overview

Provides crate-path resolution so that generated code emits correct fully-qualified paths regardless of whether the consumer depends on an individual Polaris crate or the `polaris` umbrella re-export.

## Usage

```rust
use polaris_macro_utils::{PolarisCrate, resolve_crate_path};

// In a proc macro, resolve the path to polaris_system:
let path = resolve_crate_path(PolarisCrate::System);
// Produces `polaris_system`, `renamed_dep`, or `polaris::polaris_system`
// depending on how the consumer's Cargo.toml is configured.
```

### Supported Crates

- `PolarisCrate::System` — `polaris_system`
- `PolarisCrate::Tools` — `polaris_tools`
- `PolarisCrate::Models` — `polaris_models`
- `PolarisCrate::CorePlugins` — `polaris_core_plugins`

## License

Apache-2.0
