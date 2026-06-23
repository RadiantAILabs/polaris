//! Tool registry and plugin.
//!
//! The [`ToolRegistry`] stores registered tools and provides lookup/execution.
//! The [`ToolsPlugin`] manages the registry lifecycle using the two-phase
//! initialization pattern (mutable during `build()`, frozen to `GlobalResource`
//! in `ready()`).
//!
//! See the [crate-level documentation](crate) for a full usage example.

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::permission::ToolPermission;
use crate::tool::Tool;
use crate::toolset::Toolset;
use indexmap::IndexMap;
use polaris_models::llm::ToolDefinition;
use polaris_system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris_system::resource::GlobalResource;
use polaris_system::server::Server;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Registry of available tools.
///
/// Stores tools by name and provides lookup, execution, and definition listing,
/// with per-tool permission, strict-mode, and exposure overrides applied at build
/// time.
///
/// # Examples
///
/// ```
/// use polaris_tools::{ToolRegistry, Tool, ToolContext, ToolError};
/// use polaris_models::llm::ToolDefinition;
/// use serde_json::{json, Value};
/// use std::pin::Pin;
/// use std::future::Future;
///
/// struct GreetTool;
/// impl Tool for GreetTool {
///     fn definition(&self) -> ToolDefinition {
///         ToolDefinition::new("greet", "Say hello", json!({"type": "object", "properties": {}}))
///     }
///     fn execute<'ctx>(&'ctx self, _args: Value, _ctx: &'ctx ToolContext) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'ctx>> {
///         Box::pin(async { Ok(json!("hello")) })
///     }
/// }
///
/// let mut registry = ToolRegistry::new();
/// registry.register(GreetTool);
/// assert!(registry.has("greet"));
/// assert_eq!(registry.definitions().len(), 1);
/// ```
#[derive(Default)]
pub struct ToolRegistry {
    tools: IndexMap<String, Arc<dyn Tool>>,
    permission_overrides: IndexMap<String, ToolPermission>,
    /// Per-tool overrides of the author-declared `strict` flag, set by the agent
    /// designer via [`set_strict`](Self::set_strict). Absent = use the tool's default.
    strict_overrides: IndexMap<String, bool>,
    /// Per-tool exposure overrides, set via [`set_exposed`](Self::set_exposed).
    /// Absent = exposed. A tool whose effective permission is
    /// [`ToolPermission::Deny`] is treated as unexposed regardless of this map.
    exposed_overrides: IndexMap<String, bool>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .field("permission_overrides", &self.permission_overrides)
            .field("strict_overrides", &self.strict_overrides)
            .field("exposed_overrides", &self.exposed_overrides)
            .finish()
    }
}

impl GlobalResource for ToolRegistry {}

/// The contract version at which [`ToolRegistry`] is exposed as a capability. Extender
/// plugins (e.g. `ShellPlugin`) declare a requirement against this version; bump it when
/// the registry's public surface changes incompatibly.
impl Contract for ToolRegistry {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl ToolRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: IndexMap::new(),
            permission_overrides: IndexMap::new(),
            strict_overrides: IndexMap::new(),
            exposed_overrides: IndexMap::new(),
        }
    }

    /// Registers a tool.
    ///
    /// # Panics
    ///
    /// Panics if a tool with the same name is already registered.
    pub fn register(&mut self, tool: impl Tool) {
        let name = tool.definition().name;
        assert!(
            !self.tools.contains_key(&name),
            "Tool '{name}' is already registered"
        );
        self.tools.insert(name, Arc::new(tool));
    }

    /// Registers all tools from a toolset.
    ///
    /// # Panics
    ///
    /// Panics if any tool name conflicts with an already-registered tool.
    pub fn register_toolset(&mut self, toolset: impl Toolset) {
        for tool in toolset.tools() {
            let name = tool.definition().name;
            assert!(
                !self.tools.contains_key(&name),
                "Tool '{name}' is already registered"
            );
            self.tools.insert(name, Arc::from(tool));
        }
    }

    /// Sets a permission override for a registered tool.
    ///
    /// Applied during the build phase before the registry is frozen to a global
    /// resource. Both narrowing (Allow → Confirm → Deny) and widening
    /// (Deny → Allow) are permitted to support runtime permission grants.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::RegistryError`] if no tool with `name` is registered.
    pub fn set_permission(
        &mut self,
        name: &str,
        permission: ToolPermission,
    ) -> Result<&mut Self, ToolError> {
        if !self.tools.contains_key(name) {
            return Err(ToolError::registry_error(format!(
                "tool '{name}' not in registry"
            )));
        }
        self.permission_overrides
            .insert(name.to_string(), permission);
        Ok(self)
    }

    /// Returns the effective permission for a tool.
    ///
    /// Returns the override if set, otherwise the tool's declared default.
    /// Returns `None` if the tool is not registered.
    #[must_use]
    pub fn permission(&self, name: &str) -> Option<ToolPermission> {
        self.permission_overrides
            .get(name)
            .copied()
            .or_else(|| self.tools.get(name).map(|t| t.permission()))
    }

    /// Overrides whether a tool requests provider strict-mode enforcement.
    ///
    /// Applied during the build phase before the registry is frozen. Overrides
    /// the tool author's declared [`ToolDefinition::strict`] preference. The
    /// override is honored by [`definitions`](Self::definitions) and
    /// [`definitions_for`](Self::definitions_for); the model provider still
    /// applies its own cap on the number of strict tools per request.
    ///
    /// [`ToolDefinition::strict`]: polaris_models::llm::ToolDefinition::strict
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::{ToolError, ToolRegistry, tool};
    ///
    /// #[tool]
    /// /// Look something up.
    /// async fn search(query: String) -> Result<String, ToolError> { Ok(query) }
    ///
    /// let mut registry = ToolRegistry::new();
    /// registry.register(search());
    /// registry.set_strict("search", false)?;
    /// assert!(!registry.definitions()[0].strict);
    /// # Ok::<(), polaris_tools::ToolError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::RegistryError`] if no tool with `name` is registered.
    pub fn set_strict(&mut self, name: &str, strict: bool) -> Result<&mut Self, ToolError> {
        if !self.tools.contains_key(name) {
            return Err(ToolError::registry_error(format!(
                "tool '{name}' not in registry"
            )));
        }
        self.strict_overrides.insert(name.to_string(), strict);
        Ok(self)
    }

    /// Overrides whether a tool is exposed to the model.
    ///
    /// An unexposed tool is omitted from [`definitions`](Self::definitions) and
    /// [`definitions_for`](Self::definitions_for) — the model never sees it (and
    /// it consumes neither context nor a strict-tool slot). The tool remains
    /// registered and directly invocable via
    /// [`execute_with`](Self::execute_with).
    ///
    /// Applied during the build phase before the registry is frozen.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::{ToolError, ToolRegistry, tool};
    ///
    /// #[tool]
    /// /// Internal debug helper, not advertised to the model.
    /// async fn debug_dump() -> Result<String, ToolError> { Ok(String::new()) }
    ///
    /// let mut registry = ToolRegistry::new();
    /// registry.register(debug_dump());
    /// registry.set_exposed("debug_dump", false)?;
    /// assert!(registry.definitions().is_empty()); // hidden from the model …
    /// assert!(registry.has("debug_dump")); // … but still registered and invocable
    /// # Ok::<(), polaris_tools::ToolError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::RegistryError`] if no tool with `name` is registered.
    pub fn set_exposed(&mut self, name: &str, exposed: bool) -> Result<&mut Self, ToolError> {
        if !self.tools.contains_key(name) {
            return Err(ToolError::registry_error(format!(
                "tool '{name}' not in registry"
            )));
        }
        self.exposed_overrides.insert(name.to_string(), exposed);
        Ok(self)
    }

    /// Returns whether a tool is exposed to the model.
    ///
    /// A tool is exposed unless it was hidden via [`set_exposed`](Self::set_exposed)
    /// or its effective [`permission`](Self::permission) is
    /// [`ToolPermission::Deny`] (a denied tool can never run, so it is never
    /// advertised). Returns `false` for an unregistered tool.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::ToolRegistry;
    ///
    /// let registry = ToolRegistry::new();
    /// // An unregistered tool is never exposed.
    /// assert!(!registry.is_exposed("missing"));
    /// ```
    #[must_use]
    pub fn is_exposed(&self, name: &str) -> bool {
        if !self.tools.contains_key(name) {
            return false;
        }
        if self.permission(name) == Some(ToolPermission::Deny) {
            return false;
        }
        self.exposed_overrides.get(name).copied().unwrap_or(true)
    }

    /// Executes a tool by name with JSON arguments and an empty context.
    ///
    /// This is a convenience wrapper around [`execute_with`](Self::execute_with)
    /// that passes an empty [`ToolContext`]. Use `execute_with` when tools need
    /// per-invocation state supplied by the calling system.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::UnknownTool`] if no tool with `name` is registered.
    /// Propagates any error returned by the tool's `execute` implementation
    /// (see [`Tool::execute`] for common variants). Tools with required
    /// `#[context]` parameters will return [`ToolError::ResourceNotFound`]
    /// because this wrapper supplies an empty context — use
    /// [`execute_with`](Self::execute_with) for those tools.
    pub fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'a>> {
        let tool = self.tools.get(name).cloned();
        let args = args.clone();
        Box::pin(async move {
            let tool = tool.ok_or_else(|| ToolError::unknown_tool(name))?;
            let ctx = ToolContext::new();
            tool.execute(args, &ctx).await
        })
    }

    /// Executes a tool by name with JSON arguments and per-invocation context.
    ///
    /// The [`ToolContext`] carries per-invocation state from the calling system
    /// into tool functions. Tools declare context dependencies with the
    /// `#[context]` attribute on parameters.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::UnknownTool`] if no tool with `name` is registered.
    /// Returns [`ToolError::ResourceNotFound`] if a tool's `#[context]` parameter
    /// is not present in the context. Propagates any error returned by the
    /// tool's `execute` implementation (see [`Tool::execute`] for common variants).
    pub fn execute_with<'a>(
        &'a self,
        name: &'a str,
        args: &serde_json::Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'a>> {
        let tool = self.tools.get(name).cloned();
        let args = args.clone();
        Box::pin(async move {
            let tool = tool.ok_or_else(|| ToolError::unknown_tool(name))?;
            tool.execute(args, ctx).await
        })
    }

    /// Returns tool definitions for the exposed tools, in registration order.
    ///
    /// Excludes tools hidden via [`set_exposed`](Self::set_exposed) or denied via
    /// permission (see [`is_exposed`](Self::is_exposed)), and applies any
    /// [`set_strict`](Self::set_strict) overrides. This is the set the agent
    /// advertises to the model by default; use
    /// [`definitions_for`](Self::definitions_for) to narrow it further per request,
    /// or [`all_definitions`](Self::all_definitions) to ignore exposure entirely.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions_for(|_| true)
    }

    /// Returns exposed tool definitions whose name satisfies `select`.
    ///
    /// The request-time companion to [`definitions`](Self::definitions): an agent
    /// can narrow the advertised toolset per turn (e.g. to the tools relevant to
    /// the current goal) without mutating the registry. A tool is included iff it
    /// [`is_exposed`](Self::is_exposed) **and** `select(name)` returns `true`.
    /// Registration order — and therefore the provider's strict-cap priority — is
    /// preserved. [`set_strict`](Self::set_strict) overrides are applied.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::{ToolError, ToolRegistry, tool};
    ///
    /// #[tool]
    /// /// Read a file.
    /// async fn fs_read(path: String) -> Result<String, ToolError> { Ok(path) }
    ///
    /// #[tool]
    /// /// Send a chat message.
    /// async fn chat_send(text: String) -> Result<String, ToolError> { Ok(text) }
    ///
    /// let mut registry = ToolRegistry::new();
    /// registry.register(fs_read());
    /// registry.register(chat_send());
    ///
    /// // Advertise only the filesystem tools this turn, without mutating the registry.
    /// let defs = registry.definitions_for(|name| name.starts_with("fs_"));
    /// assert_eq!(defs.len(), 1);
    /// assert_eq!(defs[0].name, "fs_read");
    /// ```
    #[must_use]
    pub fn definitions_for<F>(&self, select: F) -> Vec<ToolDefinition>
    where
        F: Fn(&str) -> bool,
    {
        self.tools
            .iter()
            .filter(|(name, _)| self.is_exposed(name) && select(name))
            .map(|(name, tool)| self.effective_definition(name, tool.as_ref()))
            .collect()
    }

    /// Returns definitions for *every* registered tool, ignoring exposure.
    ///
    /// [`set_strict`](Self::set_strict) overrides are still applied. Intended for
    /// administrative views (e.g. the `GET /v1/tools` snapshot) that should list
    /// all tools regardless of whether they are advertised to the model.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::ToolRegistry;
    ///
    /// let registry = ToolRegistry::new();
    /// assert!(registry.all_definitions().is_empty());
    /// ```
    #[must_use]
    pub fn all_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|(name, tool)| self.effective_definition(name, tool.as_ref()))
            .collect()
    }

    /// Builds a tool's definition with the registry's `strict` override applied.
    fn effective_definition(&self, name: &str, tool: &dyn Tool) -> ToolDefinition {
        let mut def = tool.definition();
        if let Some(&strict) = self.strict_overrides.get(name) {
            def.strict = strict;
        }
        def
    }

    /// Returns a reference to a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(AsRef::as_ref)
    }

    /// Returns a shared handle to a tool by name.
    ///
    /// This is the primary way decorator plugins (e.g., `TracingPlugin`) access
    /// tools when rebuilding a registry with wrapped implementations.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::ToolRegistry;
    ///
    /// let registry = ToolRegistry::new();
    /// assert!(registry.to_arc("nonexistent").is_none());
    /// ```
    #[must_use]
    pub fn to_arc(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Returns the permission overrides set via [`set_permission`](Self::set_permission).
    ///
    /// Used by decorator plugins to preserve user-configured permissions
    /// when rebuilding a registry with wrapped tool implementations.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::ToolRegistry;
    ///
    /// let registry = ToolRegistry::new();
    /// assert!(registry.permission_overrides().is_empty());
    /// ```
    #[must_use]
    pub fn permission_overrides(&self) -> &IndexMap<String, ToolPermission> {
        &self.permission_overrides
    }

    /// Returns whether a tool with the given name is registered.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Returns the names of all registered tools.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }
}

/// Plugin that provides the [`ToolRegistry`] global resource.
///
/// Registers an empty [`ToolRegistry`] during `build()` as a mutable resource so
/// other plugins can register tools in their own `build()` phase, then freezes
/// it into a [`GlobalResource`] during
/// `ready()` for read-only access by systems via `Res<ToolRegistry>`.
///
/// Use this plugin whenever an agent needs a `ToolRegistry` — either to expose
/// tools to an LLM or to invoke tools directly via
/// [`ToolRegistry::execute_with`].
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`ToolRegistry`] | Global | Registry of tools keyed by name, with per-tool permission, strict-mode, and exposure overrides |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`ToolsSnapshot`](crate::dashboard::ToolsSnapshot) *(feature `dashboard`)* | Frozen tools snapshot consumed by `GET /v1/tools`. |
///
/// # Dependencies
///
/// - [`AppPlugin`](polaris_app::AppPlugin) — only when the `dashboard` feature is enabled.
///
/// # Routes Provided
///
/// Mounted only when the `dashboard` feature is enabled, against the
/// [`HttpRouter`](polaris_app::HttpRouter) owned by `AppPlugin`.
///
/// | Method | Path | Description |
/// |--------|------|-------------|
/// | `GET` | `/v1/tools` | Frozen snapshot of registered tool definitions, effective permissions, and per-tool exposure. Takes no parameters — the handler reads only its axum `State`. |
///
/// # Lifecycle
///
/// - **`build()`** — inserts an empty [`ToolRegistry`] as a mutable
///   resource so other plugins can register tools during their own
///   `build()`. With the `dashboard` feature on, also installs the
///   [`ToolsSnapshot`](crate::dashboard::ToolsSnapshot) API and the
///   `GET /v1/tools` route.
/// - **`ready()`** — moves the [`ToolRegistry`] from a mutable resource to
///   an immutable global for read-only system access. With the `dashboard`
///   feature on, freezes the tool snapshot from the now-globalized
///   registry.
/// - The `dashboard` feature gates the `AppPlugin` dependency, the
///   `ToolsSnapshot` API, and the route above.
/// - Registers no tick schedules.
///
/// # Extends
///
/// - [`HttpRouter`](polaris_app::HttpRouter) (from
///   [`AppPlugin`](polaris_app::AppPlugin)) *(feature `dashboard`)* —
///   mounts the `GET /v1/tools` snapshot route.
///
/// # Example
///
/// ```no_run
/// use polaris_system::server::Server;
/// use polaris_tools::ToolsPlugin;
///
/// let mut server = Server::new();
/// server.add_plugins(ToolsPlugin);
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ToolsPlugin;

impl Plugin for ToolsPlugin {
    const ID: &'static str = "polaris::tools";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Declares the `ToolRegistry` capability so extender plugins (e.g. `ShellPlugin`)
        // can depend on the registry type rather than naming `ToolsPlugin`. The registry
        // is inserted imperatively in `build()` below.
        PluginAccess::new().provides::<ToolRegistry>(ToolRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_resource(ToolRegistry::new());

        #[cfg(feature = "dashboard")]
        crate::dashboard::install(server);
    }

    async fn ready(&self, server: &mut Server) {
        let registry = server
            .remove_resource::<ToolRegistry>()
            .expect("ToolRegistry should exist from build phase");
        server.insert_global(registry);

        #[cfg(feature = "dashboard")]
        crate::dashboard::freeze(server);
    }

    #[cfg(feature = "dashboard")]
    fn dependencies(&self) -> Vec<polaris_system::plugin::PluginId> {
        vec![polaris_system::plugin::PluginId::of::<polaris_app::AppPlugin>()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::ToolPermission;

    struct StubTool {
        name: &'static str,
        permission: ToolPermission,
    }

    impl Tool for StubTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                self.name,
                String::new(),
                serde_json::json!({"type": "object"}),
            )
        }

        fn permission(&self) -> ToolPermission {
            self.permission
        }

        fn execute<'ctx>(
            &'ctx self,
            _args: serde_json::Value,
            _ctx: &'ctx ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'ctx>>
        {
            Box::pin(async { Ok(serde_json::json!("ok")) })
        }
    }

    #[test]
    fn permission_returns_tool_default() {
        let mut registry = ToolRegistry::new();
        registry.register(StubTool {
            name: "confirm_tool",
            permission: ToolPermission::Confirm,
        });

        assert_eq!(
            registry.permission("confirm_tool"),
            Some(ToolPermission::Confirm)
        );
    }

    #[test]
    fn permission_returns_none_for_unknown_tool() {
        let registry = ToolRegistry::new();
        assert_eq!(registry.permission("nonexistent"), None);
    }

    #[test]
    fn set_permission_overrides_tool_default() {
        let mut registry = ToolRegistry::new();
        registry.register(StubTool {
            name: "my_tool",
            permission: ToolPermission::Allow,
        });

        registry
            .set_permission("my_tool", ToolPermission::Deny)
            .unwrap();

        assert_eq!(registry.permission("my_tool"), Some(ToolPermission::Deny));
    }

    #[test]
    fn set_permission_errors_for_unknown_tool() {
        let mut registry = ToolRegistry::new();
        let result = registry.set_permission("nonexistent", ToolPermission::Deny);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    fn registry_with(names: &[&'static str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in names {
            registry.register(StubTool {
                name,
                permission: ToolPermission::Allow,
            });
        }
        registry
    }

    fn names_of(defs: &[ToolDefinition]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn definitions_default_to_all_exposed_and_strict() {
        let registry = registry_with(&["a", "b"]);
        let defs = registry.definitions();
        assert_eq!(names_of(&defs), ["a", "b"]);
        // StubTool builds a raw ToolDefinition, which defaults to strict.
        assert!(defs.iter().all(|d| d.strict));
    }

    #[test]
    fn set_strict_override_is_applied_to_definitions() {
        let mut registry = registry_with(&["a", "b"]);
        registry.set_strict("a", false).unwrap();
        let defs = registry.definitions();
        assert!(!defs.iter().find(|d| d.name == "a").unwrap().strict);
        assert!(defs.iter().find(|d| d.name == "b").unwrap().strict);
    }

    #[test]
    fn set_exposed_false_hides_tool_from_definitions_but_keeps_it_registered() {
        let mut registry = registry_with(&["a", "b"]);
        registry.set_exposed("a", false).unwrap();
        assert!(!registry.is_exposed("a"));
        assert_eq!(names_of(&registry.definitions()), ["b"]);
        // Still registered and invocable.
        assert!(registry.has("a"));
        assert_eq!(names_of(&registry.all_definitions()), ["a", "b"]);
    }

    #[test]
    fn deny_permission_auto_unexposes() {
        let mut registry = registry_with(&["a", "b"]);
        registry.set_permission("a", ToolPermission::Deny).unwrap();
        assert!(!registry.is_exposed("a"));
        assert_eq!(names_of(&registry.definitions()), ["b"]);
        // all_definitions ignores exposure, so the denied tool still appears there.
        assert_eq!(names_of(&registry.all_definitions()), ["a", "b"]);
    }

    #[test]
    fn definitions_for_intersects_predicate_with_exposure() {
        let mut registry = registry_with(&["a", "b", "c"]);
        registry.set_exposed("b", false).unwrap();
        let defs = registry.definitions_for(|name| name != "c");
        // "c" excluded by predicate, "b" excluded by exposure → only "a".
        assert_eq!(names_of(&defs), ["a"]);
    }

    #[test]
    fn set_strict_errors_for_unknown_tool() {
        let mut registry = ToolRegistry::new();
        let strict_err = registry.set_strict("nope", false).unwrap_err();
        assert!(matches!(strict_err, ToolError::RegistryError(_)));
        assert!(strict_err.to_string().contains("nope"));
        let exposed_err = registry.set_exposed("nope", false).unwrap_err();
        assert!(matches!(exposed_err, ToolError::RegistryError(_)));
        assert!(exposed_err.to_string().contains("nope"));
    }

    #[test]
    fn is_exposed_is_false_for_unknown_tool() {
        let registry = ToolRegistry::new();
        assert!(!registry.is_exposed("nope"));
    }

    #[test]
    fn all_definitions_applies_strict_override_and_ignores_exposure() {
        let mut registry = registry_with(&["a", "b"]);
        registry.set_strict("a", false).unwrap();
        registry.set_exposed("a", false).unwrap();
        let defs = registry.all_definitions();
        // Exposure is ignored — the hidden tool still appears …
        assert_eq!(names_of(&defs), ["a", "b"]);
        // … but its documented strict override is still applied.
        assert!(!defs.iter().find(|d| d.name == "a").unwrap().strict);
        assert!(defs.iter().find(|d| d.name == "b").unwrap().strict);
    }

    #[test]
    fn deny_permission_overrides_explicit_set_exposed_true() {
        let mut registry = registry_with(&["a"]);
        // Even an explicit expose-true cannot un-hide a denied tool: the `Deny`
        // check in is_exposed precedes the exposure-override lookup.
        registry.set_exposed("a", true).unwrap();
        registry.set_permission("a", ToolPermission::Deny).unwrap();
        assert!(!registry.is_exposed("a"));
        assert!(names_of(&registry.definitions()).is_empty());
    }
}
