# persistence_macros

Derive macros for `polaris_core_plugins` persistence.

## Overview

Provides `#[derive(Storable)]` for marking resources as eligible for persistence. Generates an implementation of the `Storable` trait with a stable storage key and schema version.

## Usage

```rust
use serde::{Serialize, Deserialize};
use polaris_core_plugins::persistence::Storable;

#[derive(Serialize, Deserialize, Storable)]
#[storable(key = "ConversationMemory", schema_version = "2.0.0")]
struct ConversationMemory {
    messages: Vec<String>,
}
```

### Attributes

| Attribute | Required | Default | Description |
|-----------|----------|---------|-------------|
| `key` | Yes | — | Stable storage key for the resource |
| `schema_version` | No | `"1.0.0"` | Schema version string |

## License

Apache-2.0
