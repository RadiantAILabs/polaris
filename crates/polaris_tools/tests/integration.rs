//! Integration tests for the `polaris_tools` crate.

use polaris_models::llm::ToolDefinition;
use polaris_system::param::Res;
use polaris_tools::registry::{ToolRegistry, ToolsPlugin};
use polaris_tools::tool::Tool;
use polaris_tools::{FunctionMetadata, ParameterInfo, ToolError, Toolset, tool, toolset};
use serde::{Deserialize, Serialize};
use std::future::Future;

// ─────────────────────────────────────────────────────────────────────
// 1. Tool trait manual impl
// ─────────────────────────────────────────────────────────────────────

struct ManualTool;

impl Tool for ManualTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manual_tool".to_string(),
            description: "A manually implemented tool.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "required": ["input"]
            }),
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>>
    {
        Box::pin(async move {
            let input = args
                .get("input")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::parameter_error("Missing 'input'"))?;
            Ok(serde_json::json!({ "result": format!("echo: {}", input) }))
        })
    }
}

#[tokio::test]
async fn manual_tool_definition_and_execute() {
    let tool = ManualTool;
    let def = tool.definition();
    assert_eq!(def.name, "manual_tool");
    assert_eq!(def.description, "A manually implemented tool.");

    let result = tool
        .execute(serde_json::json!({"input": "hello"}))
        .await
        .unwrap();
    assert_eq!(result["result"], "echo: hello");
}

// ─────────────────────────────────────────────────────────────────────
// 2. ToolRegistry
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn registry_register_get_has_names() {
    let mut registry = ToolRegistry::new();
    registry.register(ManualTool);

    assert!(registry.has("manual_tool"));
    assert!(!registry.has("nonexistent"));
    assert!(registry.get("manual_tool").is_some());
    assert!(registry.get("nonexistent").is_none());
    assert!(registry.names().contains(&"manual_tool"));
}

#[tokio::test]
async fn registry_definitions() {
    let mut registry = ToolRegistry::new();
    registry.register(ManualTool);

    let defs = registry.definitions();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "manual_tool");
}

#[tokio::test]
async fn registry_execute_by_name() {
    let mut registry = ToolRegistry::new();
    registry.register(ManualTool);

    let result = registry
        .execute("manual_tool", &serde_json::json!({"input": "test"}))
        .await
        .unwrap();
    assert_eq!(result["result"], "echo: test");
}

// ─────────────────────────────────────────────────────────────────────
// 3. ToolRegistry errors
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn registry_unknown_tool_error() {
    let registry = ToolRegistry::new();
    let result = registry
        .execute("nonexistent", &serde_json::json!({}))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Unknown tool"), "got: {}", err);
}

#[test]
#[should_panic(expected = "already registered")]
fn registry_duplicate_registration_panics() {
    let mut registry = ToolRegistry::new();
    registry.register(ManualTool);
    registry.register(ManualTool);
}

// ─────────────────────────────────────────────────────────────────────
// 4. ToolsPlugin lifecycle
// ─────────────────────────────────────────────────────────────────────

#[test]
fn tools_plugin_lifecycle() {
    use polaris_system::plugin::Plugin;
    use polaris_system::server::Server;

    let mut server = Server::new();
    let plugin = ToolsPlugin::default();

    // build: inserts mutable resource
    plugin.build(&mut server);
    {
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("should exist after build");
        registry.register(ManualTool);
    }

    // ready: moves to global
    plugin.ready(&mut server);

    // After ready, it should be a global resource
    let ctx = server.create_context();
    let res = Res::<ToolRegistry>::fetch(&ctx);
    assert!(res.is_ok());
    assert!(res.unwrap().has("manual_tool"));
}

// ─────────────────────────────────────────────────────────────────────
// 5. #[tool] standalone — basic
// ─────────────────────────────────────────────────────────────────────

#[tool]
/// Greet someone by name.
async fn greet(
    /// The person's name.
    name: String,
) -> Result<String, ToolError> {
    Ok(format!("Hello, {}!", name))
}

#[tokio::test]
async fn tool_standalone_basic() {
    let tool = greet();
    let def = tool.definition();
    assert_eq!(def.name, "greet");
    assert_eq!(def.description, "Greet someone by name.");
    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("name"));
    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("name")));

    let result = tool
        .execute(serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("Hello, Alice!"));
}

// ─────────────────────────────────────────────────────────────────────
// 6. Toolset with captured state
// ─────────────────────────────────────────────────────────────────────

struct GreetingTools {
    prefix: String,
}

#[toolset]
impl GreetingTools {
    #[tool]
    /// Greet with a configurable prefix.
    async fn greet_with_config(
        &self,
        /// The person's name.
        name: String,
    ) -> Result<String, ToolError> {
        Ok(format!("{} {}!", self.prefix, name))
    }
}

#[tokio::test]
async fn toolset_with_captured_state() {
    let tools = GreetingTools {
        prefix: "Hi".to_string(),
    }
    .tools();
    assert_eq!(tools.len(), 1);

    let tool = &tools[0];

    // Schema should only include name
    let def = tool.definition();
    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("name"));

    let result = tool
        .execute(serde_json::json!({"name": "Bob"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("Hi Bob!"));
}

// ─────────────────────────────────────────────────────────────────────
// 7. Toolset with mutable captured state
// ─────────────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicI32, Ordering};

struct CounterTools {
    count: AtomicI32,
}

#[toolset]
impl CounterTools {
    #[tool]
    /// Increment a counter.
    async fn increment(
        &self,
        /// Amount to add.
        #[default(1)]
        amount: i32,
    ) -> Result<i32, ToolError> {
        let new_val = self.count.fetch_add(amount, Ordering::SeqCst) + amount;
        Ok(new_val)
    }
}

#[tokio::test]
async fn toolset_with_mutable_captured_state() {
    let tools = CounterTools {
        count: AtomicI32::new(0),
    }
    .tools();
    let tool = &tools[0];

    // Schema should include amount but not count
    let def = tool.definition();
    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("amount"));

    let result = tool
        .execute(serde_json::json!({"amount": 5}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!(5));

    let result = tool
        .execute(serde_json::json!({"amount": 3}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!(8));
}

// ─────────────────────────────────────────────────────────────────────
// 8. #[tool] with no params
// ─────────────────────────────────────────────────────────────────────

#[tool]
/// Returns the current time.
async fn get_time() -> Result<String, ToolError> {
    Ok("2025-01-01T00:00:00Z".to_string())
}

#[tokio::test]
async fn tool_no_params() {
    let tool = get_time();
    let def = tool.definition();
    assert_eq!(def.name, "get_time");
    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.is_empty());

    let result = tool.execute(serde_json::json!({})).await.unwrap();
    assert_eq!(result, serde_json::json!("2025-01-01T00:00:00Z"));
}

// ─────────────────────────────────────────────────────────────────────
// 9. #[tool] with #[default(value)]
// ─────────────────────────────────────────────────────────────────────

#[tool]
/// List items with optional limit.
async fn list_items(
    /// Category to list.
    category: String,
    /// Maximum items to return.
    #[default(100)]
    limit: usize,
) -> Result<String, ToolError> {
    Ok(format!("{}: limit {}", category, limit))
}

#[tokio::test]
async fn tool_with_default() {
    let tool = list_items();
    let def = tool.definition();

    // limit should not be in required
    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("category")));
    assert!(!required.contains(&serde_json::json!("limit")));

    // Default should appear in schema
    let limit_schema = &def.parameters["properties"]["limit"];
    assert_eq!(limit_schema["default"], serde_json::json!(100));

    // Without limit — uses default
    let result = tool
        .execute(serde_json::json!({"category": "books"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("books: limit 100"));

    // With limit — uses provided
    let result = tool
        .execute(serde_json::json!({"category": "books", "limit": 5}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("books: limit 5"));
}

// ─────────────────────────────────────────────────────────────────────
// 10. #[tool] with Option<T>
// ─────────────────────────────────────────────────────────────────────

#[tool]
/// Search with optional filter.
async fn search(
    /// Search query.
    query: String,
    /// Optional filter.
    filter: Option<String>,
) -> Result<String, ToolError> {
    match filter {
        Some(f) => Ok(format!("query={}, filter={}", query, f)),
        None => Ok(format!("query={}", query)),
    }
}

#[tokio::test]
async fn tool_with_option() {
    let tool = search();
    let def = tool.definition();

    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("query")));
    assert!(!required.contains(&serde_json::json!("filter")));

    // Without filter
    let result = tool
        .execute(serde_json::json!({"query": "rust"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("query=rust"));

    // With filter
    let result = tool
        .execute(serde_json::json!({"query": "rust", "filter": "recent"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("query=rust, filter=recent"));
}

// ─────────────────────────────────────────────────────────────────────
// 11. #[toolset] — basic
// ─────────────────────────────────────────────────────────────────────

struct MathTools;

#[toolset]
impl MathTools {
    #[tool]
    /// Add two numbers.
    async fn add(
        &self,
        /// First number.
        a: f64,
        /// Second number.
        b: f64,
    ) -> Result<f64, ToolError> {
        Ok(a + b)
    }

    #[tool]
    /// Multiply two numbers.
    async fn multiply(
        &self,
        /// First number.
        a: f64,
        /// Second number.
        b: f64,
    ) -> Result<f64, ToolError> {
        Ok(a * b)
    }
}

#[tokio::test]
async fn toolset_basic() {
    let tools = MathTools.tools();
    assert_eq!(tools.len(), 2);

    let names: Vec<String> = tools.iter().map(|t| t.definition().name.clone()).collect();
    assert!(names.contains(&"add".to_string()));
    assert!(names.contains(&"multiply".to_string()));

    // Find and execute add
    let add_tool = tools.iter().find(|t| t.definition().name == "add").unwrap();
    let result = add_tool
        .execute(serde_json::json!({"a": 2.0, "b": 3.0}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!(5.0));

    // Find and execute multiply
    let mul_tool = tools
        .iter()
        .find(|t| t.definition().name == "multiply")
        .unwrap();
    let result = mul_tool
        .execute(serde_json::json!({"a": 4.0, "b": 5.0}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!(20.0));
}

// ─────────────────────────────────────────────────────────────────────
// 12. #[toolset] with captured config
// ─────────────────────────────────────────────────────────────────────

struct ConfiguredTools {
    prefix: String,
}

#[toolset]
impl ConfiguredTools {
    #[tool]
    /// Format a message with prefix.
    async fn format_message(
        &self,
        /// The message text.
        message: String,
    ) -> Result<String, ToolError> {
        Ok(format!("[{}] {}", self.prefix, message))
    }
}

#[tokio::test]
async fn toolset_with_captured_config() {
    let tools = ConfiguredTools {
        prefix: "INFO".to_string(),
    }
    .tools();
    assert_eq!(tools.len(), 1);

    let tool = &tools[0];
    let def = tool.definition();
    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("message"));

    let result = tool
        .execute(serde_json::json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("[INFO] hello"));
}

// ─────────────────────────────────────────────────────────────────────
// 13. Schema correctness
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn schema_doc_comments_become_descriptions() {
    let tool = greet();
    let def = tool.definition();
    let name_schema = &def.parameters["properties"]["name"];
    assert_eq!(
        name_schema["description"].as_str().unwrap(),
        "The person's name."
    );
}

#[tokio::test]
async fn schema_default_appears() {
    let tool = list_items();
    let def = tool.definition();
    let limit_schema = &def.parameters["properties"]["limit"];
    assert_eq!(limit_schema["default"], serde_json::json!(100));
}

#[tokio::test]
async fn schema_no_root_metadata_in_properties() {
    let tool = list_items();
    let def = tool.definition();
    let props = def.parameters["properties"].as_object().unwrap();
    for (name, schema) in props {
        assert!(
            schema.get("$schema").is_none(),
            "property '{name}' should not contain $schema"
        );
        assert!(
            schema.get("title").is_none(),
            "property '{name}' should not contain title"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// 14. Struct param
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct SearchParams {
    /// The query string.
    query: String,
    /// Maximum results.
    limit: usize,
}

#[tool]
/// Search with structured params.
async fn structured_search(params: SearchParams) -> Result<String, ToolError> {
    Ok(format!("query={}, limit={}", params.query, params.limit))
}

#[tokio::test]
async fn tool_struct_param_flat_mode() {
    let tool = structured_search();
    let def = tool.definition();

    // In flat mode, the struct becomes a named "params" property
    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("params"));

    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("params")));

    let result = tool
        .execute(serde_json::json!({"params": {"query": "rust", "limit": 10}}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("query=rust, limit=10"));
}

// ─────────────────────────────────────────────────────────────────────
// 15. Tagged enum param
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "type")]
enum QueryMode {
    /// Simple search.
    Simple { query: String },
    /// Advanced search with filters.
    Advanced { query: String, filters: Vec<String> },
}

#[tool]
/// Search with flexible mode.
async fn flexible_search(mode: QueryMode) -> Result<String, ToolError> {
    match mode {
        QueryMode::Simple { query } => Ok(format!("simple: {}", query)),
        QueryMode::Advanced { query, filters } => {
            Ok(format!("advanced: {} (filters: {:?})", query, filters))
        }
    }
}

#[tokio::test]
async fn tool_tagged_enum_flat_mode() {
    let tool = flexible_search();
    let def = tool.definition();

    // In flat mode, the enum is a named "mode" property
    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("mode"));

    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("mode")));

    // Simple mode — wrapped in the "mode" parameter
    let result = tool
        .execute(serde_json::json!({"mode": {"type": "Simple", "query": "rust"}}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("simple: rust"));

    // Advanced mode
    let result = tool
        .execute(serde_json::json!({"mode": {"type": "Advanced", "query": "rust", "filters": ["recent"]}}))
        .await
        .unwrap();
    assert!(result.as_str().unwrap().contains("advanced: rust"));
}

// ─────────────────────────────────────────────────────────────────────
// Registry integration with toolset
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn registry_with_toolset() {
    let mut registry = ToolRegistry::new();
    registry.register_toolset(MathTools);

    assert!(registry.has("add"));
    assert!(registry.has("multiply"));

    let result = registry
        .execute("add", &serde_json::json!({"a": 1.0, "b": 2.0}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!(3.0));
}

// ─────────────────────────────────────────────────────────────────────
// FunctionMetadata and ParameterInfo
// ─────────────────────────────────────────────────────────────────────

#[test]
fn function_metadata_builder() {
    let meta = FunctionMetadata::new("test")
        .with_description("A test function.")
        .add_parameter({
            let mut p = ParameterInfo::new("name", serde_json::json!({"type": "string"}));
            p.description = Some("The name.".to_string());
            p
        })
        .add_parameter({
            let mut p = ParameterInfo::new("count", serde_json::json!({"type": "integer"}));
            p.required = false;
            p.default_value = Some(serde_json::json!(10));
            p
        });

    let def = meta.to_tool_definition();
    assert_eq!(def.name, "test");
    assert_eq!(def.description, "A test function.");

    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("name")));
    assert!(!required.contains(&serde_json::json!("count")));

    let count_schema = &def.parameters["properties"]["count"];
    assert_eq!(count_schema["default"], serde_json::json!(10));
}

// ─────────────────────────────────────────────────────────────────────
// 16. Struct param + primitive param
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct FilterConfig {
    /// Minimum relevance score.
    min_score: f64,
    /// Tags to filter by.
    tags: Vec<String>,
}

#[tool]
/// Search with a filter config and a query string.
async fn filtered_search(
    /// The search query.
    query: String,
    /// Filter configuration.
    config: FilterConfig,
) -> Result<String, ToolError> {
    Ok(format!(
        "query={}, min_score={}, tags={:?}",
        query, config.min_score, config.tags
    ))
}

#[tokio::test]
async fn tool_struct_and_primitive_params() {
    let tool = filtered_search();
    let def = tool.definition();

    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("query"));
    assert!(props.contains_key("config"));

    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("query")));
    assert!(required.contains(&serde_json::json!("config")));

    let result = tool
        .execute(serde_json::json!({
            "query": "rust",
            "config": {"min_score": 0.5, "tags": ["programming"]}
        }))
        .await
        .unwrap();
    assert!(result.as_str().unwrap().contains("query=rust"));
    assert!(result.as_str().unwrap().contains("min_score=0.5"));
}

// ─────────────────────────────────────────────────────────────────────
// 17. Option<UserStruct> param
// ─────────────────────────────────────────────────────────────────────

#[tool]
/// Search with optional structured filter.
async fn search_with_optional_config(
    /// The search query.
    query: String,
    /// Optional filter configuration.
    config: Option<FilterConfig>,
) -> Result<String, ToolError> {
    match config {
        Some(c) => Ok(format!(
            "query={}, filtered(min_score={})",
            query, c.min_score
        )),
        None => Ok(format!("query={}, no filter", query)),
    }
}

#[tokio::test]
async fn tool_optional_struct_param() {
    let tool = search_with_optional_config();
    let def = tool.definition();

    let props = def.parameters["properties"].as_object().unwrap();
    assert!(props.contains_key("query"));
    assert!(props.contains_key("config"));

    let required = def.parameters["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("query")));
    assert!(!required.contains(&serde_json::json!("config")));

    // Without config
    let result = tool
        .execute(serde_json::json!({"query": "rust"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("query=rust, no filter"));

    // With config
    let result = tool
        .execute(serde_json::json!({
            "query": "rust",
            "config": {"min_score": 0.8, "tags": ["lang"]}
        }))
        .await
        .unwrap();
    assert!(result.as_str().unwrap().contains("filtered"));
}

// ─────────────────────────────────────────────────────────────────────
// 18. Generic #[toolset]
// ─────────────────────────────────────────────────────────────────────

struct FormatterTools<F: Fn(&str) -> String + Send + Sync + 'static> {
    format_fn: F,
}

#[toolset]
impl<F: Fn(&str) -> String + Send + Sync + 'static> FormatterTools<F> {
    #[tool]
    /// Format a message using the formatter.
    async fn format(
        &self,
        /// The message to format.
        message: String,
    ) -> Result<String, ToolError> {
        Ok((self.format_fn)(&message))
    }
}

#[tokio::test]
async fn toolset_generic_impl() {
    let tools = FormatterTools {
        format_fn: |msg: &str| format!("[UPPER] {}", msg.to_uppercase()),
    }
    .tools();
    assert_eq!(tools.len(), 1);

    let tool = &tools[0];
    let def = tool.definition();
    assert_eq!(def.name, "format");

    let result = tool
        .execute(serde_json::json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(result, serde_json::json!("[UPPER] HELLO"));
}

// ─────────────────────────────────────────────────────────────────────
// 19. Generic #[toolset] with where clause
// ─────────────────────────────────────────────────────────────────────

struct WrapperTools<T> {
    value: T,
}

#[toolset]
impl<T> WrapperTools<T>
where
    T: std::fmt::Display + Send + Sync + 'static,
{
    #[tool]
    /// Display the wrapped value.
    async fn display(&self) -> Result<String, ToolError> {
        Ok(format!("{}", self.value))
    }
}

#[tokio::test]
async fn toolset_generic_with_where_clause() {
    let tools = WrapperTools { value: 42i32 }.tools();
    assert_eq!(tools.len(), 1);

    let result = tools[0].execute(serde_json::json!({})).await.unwrap();
    assert_eq!(result, serde_json::json!("42"));
}

// ─────────────────────────────────────────────────────────────────────
// Use SystemParam::fetch helper
// ─────────────────────────────────────────────────────────────────────

use polaris_system::param::SystemParam;
