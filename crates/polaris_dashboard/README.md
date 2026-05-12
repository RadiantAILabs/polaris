# polaris_dashboard

Cross-plugin contribution registry for Polaris dashboards and observability UIs.

This crate provides:

- `DashboardRegistry` — an `API` plugins call in their `build()` to
  contribute nav items, sections, and panels.
- `DashboardPlugin` — registers the registry, freezes a snapshot in
  `ready()`, and exposes `GET /v1/dashboard/manifest`.
- Descriptor types (`NavItem`, `Section`, `Panel`, `Transport`) and the
  `RegistryEvent` broadcast channel.

The crate is **registry-only** — it ships zero frontend code, by design.
UI consumers (the external `polaris-dashboard` Svelte app, terminal TUIs,
custom admin UIs) read the manifest and project it however they like.

`DashboardPlugin` is **opt-in** and not part of `DefaultPlugins`.

See the crate-level Rust docs for the seed `kind` vocabulary and a usage
example.
