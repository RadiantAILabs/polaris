Cross-plugin contribution registry for dashboards and observability UIs.

This module provides the framework primitive that lets any plugin contribute
nav items, sections, and panels to a single dashboard manifest. It is
**registry-only** — there is no HTML, no JS bundle, no asset pipeline here.
Actual UI is owned by external consumers (the `polaris-dashboard` Svelte app,
custom admin UIs, terminal TUIs) that read the manifest and render it.

# `DashboardPlugin` and `DashboardRegistry`

`DashboardPlugin` is **opt-in** — it is not part of `DefaultPlugins`. Add it
explicitly when you want a dashboard surface, then add the matching
`*DashboardPlugin` from each core plugin you want to expose.

```no_run
use polaris_ai::system::server::Server;
use polaris_ai::app::{AppConfig, AppPlugin};
use polaris_ai::dashboard::DashboardPlugin;

let mut server = Server::new();
server
    .add_plugins(AppPlugin::new(AppConfig::default()))
    .add_plugins(DashboardPlugin::default());
```

`DashboardPlugin::ready()` freezes the registry, broadcasts a
`RegistryEvent::Ready` snapshot, and serves the snapshot at
`GET /v1/dashboard/manifest`.

# Contributing from a Plugin

Plugins call `DashboardRegistry::add_nav_item` / `add_section` / `add_panel`
during `build()`, pointing at endpoints they own:

```ignore
use polaris_ai::dashboard::{DashboardRegistry, NavItem, Panel, Section, Transport};
use polaris_ai::system::plugin::{Plugin, PluginId, Version};
use polaris_ai::system::server::Server;

struct MyDashboardContribution;

impl Plugin for MyDashboardContribution {
    const ID: &'static str = "myapp::dashboard";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server.api::<DashboardRegistry>()
            .expect("DashboardPlugin must be added first")
            .add_nav_item(NavItem::new("cost", "Cost Analysis"))
            .add_section(Section::new("cost-overview", "cost", "Overview"))
            .add_panel(
                Panel::new("cost-list", "By session", "list", "/v1/cost", Transport::Rest)
                    .with_section("cost-overview"),
            );
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<polaris_ai::dashboard::DashboardPlugin>()]
    }
}
```

# Seed `kind` Vocabulary

`Panel::kind` is a free-form string so plugins can introduce new panel types
without touching this crate. The Svelte reference dashboard ships a component
per *seed kind*; plugin-declared kinds fall through to a schema-driven
generic renderer.

| `kind`           | Intended use                                       |
|------------------|----------------------------------------------------|
| `list`           | Tabular listing of records                         |
| `detail`         | Single-record detail view                          |
| `kv`             | Key/value inspector for resource snapshots         |
| `log`            | Append-only log stream                             |
| `timeseries`     | Numeric time-series for live metrics               |
| `polaris-graph`  | Graph topology visualization for an agent          |
| `otel-trace`     | OpenTelemetry-formatted trace tree                 |
| `external`       | Iframe pointing at `metadata.url` (escape hatch)   |

# Suppressing Upstream Contributions

`DashboardRegistry` exposes `remove_nav_item(id)` / `remove_section(id)` /
`remove_panel(id)` for build-time suppression. This lets a downstream plugin
hide specific items from upstream contributions without forking. Suppression
is silently no-op for missing ids, so ordering is forgiving.

# Core Plugin Contributions

Each core plugin gates its dashboard contributions behind an opt-in
`dashboard` feature. Without the feature, the plugin has no dependency on
`polaris_dashboard`. Enable per-crate or via the umbrella `dashboard`
feature on `polaris-ai`:

| Feature | Plugin | Contribution |
|---------|--------|--------------|
| `sessions-dashboard` | `sessions::SessionsDashboardPlugin` | Sessions nav, list + detail panels |
| `tools-dashboard` | `tools::ToolsDashboardPlugin` | Tools nav, registry list panel |
| `models-dashboard` | `models::ModelsDashboardPlugin` | Models nav, registry list panel |
| `tracing-dashboard` | `plugins::TracingDashboardPlugin` | Traces nav, span log panel |
| `dashboard` | All of the above | — |

# Related

- [App](crate::app) — `AppPlugin` lifecycle and route registration
- [Sessions](crate::sessions) — session endpoints consumed by the Sessions panel
- [Plugins](crate::system) — `Plugin` trait used to register contributions
