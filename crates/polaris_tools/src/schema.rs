//! Schema types for tool parameter metadata.
//!
//! Provides [`ParameterInfo`] for individual parameter schemas and
//! [`FunctionMetadata`] for building complete tool definitions with
//! JSON Schema parameter specifications.

use polaris_models::llm::ToolDefinition;
use serde::{Deserialize, Serialize};

/// Schema information for a single tool parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterInfo {
    /// Parameter name.
    pub name: String,
    /// Parameter description (typically from doc comments).
    pub description: Option<String>,
    /// JSON Schema for this parameter's type.
    pub schema: serde_json::Value,
    /// Whether this parameter is required.
    pub required: bool,
    /// Custom name for the JSON schema property (overrides `name`).
    pub schema_name: Option<String>,
    /// Default value for optional parameters.
    pub default_value: Option<serde_json::Value>,
}

impl ParameterInfo {
    /// Creates a new required parameter with the given name and schema.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::schema::ParameterInfo;
    /// use serde_json::json;
    ///
    /// let param = ParameterInfo::new("city", json!({"type": "string"}));
    /// assert!(param.required);
    /// ```
    pub fn new(name: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            schema,
            required: true,
            schema_name: None,
            default_value: None,
        }
    }
}

/// Metadata describing a tool function's name, description, and parameters.
///
/// Used to build [`ToolDefinition`] instances with proper JSON Schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionMetadata {
    /// Function name.
    pub name: String,
    /// Function description.
    pub description: Option<String>,
    /// LLM-visible parameters.
    pub parameters: Vec<ParameterInfo>,
    /// Full JSON Schema derived from `parameters`. Use [`Self::schema()`] to read.
    schema: serde_json::Value,
}

impl FunctionMetadata {
    /// Creates new metadata with the given function name.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::schema::{FunctionMetadata, ParameterInfo};
    /// use serde_json::json;
    ///
    /// let meta = FunctionMetadata::new("get_weather")
    ///     .with_description("Get weather for a city")
    ///     .add_parameter(ParameterInfo::new("city", json!({"type": "string"})));
    ///
    /// let def = meta.to_tool_definition();
    /// assert_eq!(def.name, "get_weather");
    /// ```
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            parameters: Vec::new(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    /// Sets the function description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Adds a parameter and rebuilds the schema.
    #[must_use]
    pub fn add_parameter(mut self, param: ParameterInfo) -> Self {
        self.parameters.push(param);
        self.rebuild_schema();
        self
    }

    /// Returns the full JSON Schema for the function's parameters.
    #[must_use]
    pub fn schema(&self) -> &serde_json::Value {
        &self.schema
    }

    /// Converts this metadata into a [`ToolDefinition`].
    pub fn to_tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone().unwrap_or_default(),
            parameters: self.schema.clone(),
        }
    }

    fn rebuild_schema(&mut self) {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for param in &self.parameters {
            let mut param_schema = param.schema.clone();
            if let Some(desc) = &param.description
                && let Some(obj) = param_schema.as_object_mut()
            {
                obj.insert(
                    "description".to_string(),
                    serde_json::Value::String(desc.clone()),
                );
            }

            if let Some(default) = &param.default_value
                && let Some(obj) = param_schema.as_object_mut()
            {
                obj.insert("default".to_string(), default.clone());
            }

            let property_name = param.schema_name.as_ref().unwrap_or(&param.name);
            properties.insert(property_name.clone(), param_schema);

            if param.required && param.default_value.is_none() {
                required.push(property_name.clone());
            }
        }

        self.schema = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required
        });
    }
}
