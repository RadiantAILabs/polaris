//! Opt-in pricing table consulted by the usage aggregation endpoints.
//!
//! [`TracingPlugin`](super::TracingPlugin) registers an empty
//! [`UsagePricing`] as a server API when the `dashboard` feature is on.
//! Consumer plugins populate it during their own `build()` phase; the
//! usage aggregator picks up whatever rates are present at query time.
//! With no entries registered the responses still surface token counts
//! and `cost_usd` stays `None`.
//!
//! Pricing is intentionally consumer-owned — provider rate cards change
//! quarterly and would rot in-tree.

use parking_lot::RwLock;
use polaris_system::api::API;
use std::collections::HashMap;
use std::sync::Arc;

/// Per-`(provider, model)` USD rates. Defined by `polaris_models` so the
/// same type backs both the [`LlmProvider`](polaris_models::llm::LlmProvider)
/// pricing contract and this consumer-owned override table.
pub use polaris_models::llm::ModelPricing;

/// Opt-in pricing table the usage-rollup endpoints consult to turn token
/// counts into `cost_usd`.
///
/// Reach for this when a plugin knows the USD rates for a provider's
/// models and wants the dashboard's usage endpoints to report cost
/// alongside token counts. With no rates registered the endpoints still
/// return token counts; `cost_usd` simply stays `None`.
///
/// Cheaply cloneable — all clones share the same backing map.
///
/// # Provided by
///
/// [`TracingPlugin`](super::TracingPlugin), which calls
/// [`Server::insert_api`](polaris_system::server::Server::insert_api) with
/// an empty table during `build()` when the `dashboard` feature is
/// enabled. No table exists without that feature.
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`new`](Self::new) | Creates a new, empty pricing table. |
/// | [`set`](Self::set) | Registers (or replaces) the [`ModelPricing`] rate for one `(provider, model)` pair. |
/// | [`get`](Self::get) | Returns the registered rate for a `(provider, model)` pair, if one exists. |
/// | [`is_empty`](Self::is_empty) | Reports whether any rates have been registered. |
///
/// # Lifecycle
///
/// [`set`](Self::set) is intended to be called from a consumer plugin's
/// `build()` phase. The backing `RwLock` accepts writes at any time, so a
/// late `set` (in `ready()` or at runtime) is not an error — but rates
/// registered after a usage query has been served are not applied
/// retroactively. The aggregator reads whatever rates are present at
/// query time, so [`get`](Self::get) is always valid.
///
/// # Composition
///
/// **Open extension** — any plugin may call [`set`](Self::set) through
/// `&self`; the table uses an `RwLock` for interior mutability. Clones
/// share the same backing map, so a rate registered through one clone is
/// visible through every other.
///
/// # Example consumers
///
/// No plugin in this workspace seeds rates by default — pricing is
/// intentionally consumer-owned because provider rate cards change
/// quarterly and would rot in-tree. A downstream application registers
/// the rates it cares about from its own plugin's `build()`.
///
/// # Example
///
/// [`TracingPlugin`](super::TracingPlugin) provides the table; a consumer
/// plugin resolves it and seeds rates:
///
/// ```no_run
/// use polaris_core_plugins::{ModelPricing, ServerInfoPlugin, TracingPlugin, UsagePricing};
/// use polaris_system::plugin::{Plugin, PluginId, Version};
/// use polaris_system::server::Server;
///
/// // Consumer: seeds rates during its own `build()`.
/// struct PricingSeedPlugin;
///
/// impl Plugin for PricingSeedPlugin {
///     const ID: &'static str = "example::pricing_seed";
///     const VERSION: Version = Version::new(0, 0, 1);
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<TracingPlugin>()]
///     }
///
///     fn build(&self, server: &mut Server) {
///         let pricing = server
///             .api::<UsagePricing>()
///             .expect("TracingPlugin with `dashboard` feature provides UsagePricing");
///         pricing.set(
///             "anthropic",
///             "claude-opus-4-7",
///             ModelPricing::new(15.0, 75.0),
///         );
///     }
/// }
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(polaris_app::AppPlugin::new(
///         polaris_app::AppConfig::new().with_host("127.0.0.1"),
///     ))
///     .add_plugins(polaris_models::ModelsPlugin)
///     .add_plugins(polaris_tools::ToolsPlugin)
///     // Provider: TracingPlugin inserts an empty `UsagePricing` API.
///     .add_plugins(TracingPlugin::new())
///     .add_plugins(PricingSeedPlugin);
/// server.run().await.unwrap();
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct UsagePricing {
    inner: Arc<RwLock<HashMap<(String, String), ModelPricing>>>,
}

impl API for UsagePricing {}

impl UsagePricing {
    /// Creates a new, empty pricing table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) the rate for one `(provider, model)` pair.
    pub fn set(&self, provider: &str, model: &str, rate: ModelPricing) {
        self.inner
            .write()
            .insert((provider.to_owned(), model.to_owned()), rate);
    }

    /// Returns the registered rate, when present.
    #[must_use]
    pub fn get(&self, provider: &str, model: &str) -> Option<ModelPricing> {
        self.inner
            .read()
            .get(&(provider.to_owned(), model.to_owned()))
            .copied()
    }

    /// Reports whether any rates have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_round_trips_a_rate() {
        let pricing = UsagePricing::new();
        pricing.set(
            "anthropic",
            "claude-sonnet-4-6",
            ModelPricing::new(3.0, 15.0),
        );
        let rate = pricing
            .get("anthropic", "claude-sonnet-4-6")
            .expect("rate registered");
        assert!((rate.input_per_million_usd - 3.0).abs() < f64::EPSILON);
        assert!((rate.output_per_million_usd - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clones_share_storage() {
        let a = UsagePricing::new();
        let b = a.clone();
        b.set("openai", "gpt-5", ModelPricing::new(1.0, 2.0));
        assert!(a.get("openai", "gpt-5").is_some());
    }

    #[test]
    fn is_empty_reflects_registration_state() {
        let pricing = UsagePricing::new();
        assert!(pricing.is_empty());
        pricing.set("openai", "gpt-5", ModelPricing::new(1.0, 2.0));
        assert!(!pricing.is_empty());
    }
}
