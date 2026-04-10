//! [`TiktokenCounter`] — a [`TokenCounter`] backed by tiktoken BPE tokenizers.

use super::counter::{TokenCount, TokenCountError, TokenCounter};
use crate::llm::{
    AssistantBlock, LlmRequest, Message, ToolDefinition, ToolResult, ToolResultContent, UserBlock,
};
use std::collections::HashMap;
use std::sync::Arc;
use tiktoken_rs::CoreBPE;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Approximate characters-per-token ratio used by the heuristic fallback.
const CHARS_PER_TOKEN: usize = 4;

/// Tokens added per message for models using the `ChatML` format (GPT-3.5+).
///
/// Accounts for `<|start|>{role}\n`, `\n` separators, etc.
const MESSAGE_OVERHEAD_TOKENS: usize = 4;

/// Tokens added once at the end of a conversation for the assistant reply
/// priming (`<|start|>assistant\n`).
const REPLY_PRIMING_TOKENS: usize = 3;

// ─────────────────────────────────────────────────────────────────────────────
// Encoding families
// ─────────────────────────────────────────────────────────────────────────────

/// Known tokenizer encoding families.
///
/// Used with [`TiktokenCounter::with_aliases`] to map custom model names to a
/// specific BPE tokenizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum EncodingFamily {
    /// GPT-4o, GPT-4o-mini, and newer `OpenAI` models.
    O200kBase,
    /// GPT-4, GPT-3.5-turbo, text-embedding-ada-002, and similar.
    Cl100kBase,
}

// ─────────────────────────────────────────────────────────────────────────────
// TiktokenCounter
// ─────────────────────────────────────────────────────────────────────────────

/// A [`TokenCounter`] that uses tiktoken BPE tokenizers for exact counts and
/// falls back to a character-based heuristic for unknown models.
///
/// BPE tokenizer data tables are loaded eagerly at construction time so that
/// subsequent counting calls are allocation-free.
///
/// # Resolution chain
///
/// When counting tokens for a model name the counter resolves the tokenizer in
/// this order:
///
/// 1. **Direct match** — tiktoken-rs knows the model name directly.
/// 2. **Built-in prefix match** — well-known model-family prefixes (e.g.
///    `"gpt-5"`, `"claude"`) map to a known encoding.
/// 3. **User alias table** — custom mappings supplied via
///    [`with_aliases`](Self::with_aliases).
/// 4. **Heuristic fallback** — `ceil(chars / 4)` with `exact = false`.
///
/// # Panics
///
/// Construction ([`new`](Self::new), [`with_aliases`](Self::with_aliases),
/// [`Default::default`]) panics if the compiled-in BPE data tables cannot be
/// initialised. This is infallible in practice — the tables are static byte
/// slices embedded by tiktoken-rs at build time.
///
/// # Examples
///
/// ```
/// use polaris_models::tokenizer::{TokenCounter, TiktokenCounter};
///
/// let counter = TiktokenCounter::new();
///
/// // Known model — exact count
/// let exact = counter.count_text("gpt-5.4", "Hello!")?;
/// assert!(exact.exact);
///
/// // Unknown model — heuristic estimate
/// let est = counter.count_text("my-custom-model", "Hello!")?;
/// assert!(!est.exact);
/// # Ok::<(), polaris_models::tokenizer::TokenCountError>(())
/// ```
pub struct TiktokenCounter {
    /// User-supplied aliases mapping model name prefixes to encoding families.
    aliases: HashMap<String, EncodingFamily>,
    /// Pre-initialised `o200k_base` BPE tokenizer.
    o200k_bpe: Arc<CoreBPE>,
    /// Pre-initialised `cl100k_base` BPE tokenizer.
    cl100k_bpe: Arc<CoreBPE>,
}

impl std::fmt::Debug for TiktokenCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TiktokenCounter")
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

impl Clone for TiktokenCounter {
    fn clone(&self) -> Self {
        Self {
            aliases: self.aliases.clone(),
            o200k_bpe: Arc::clone(&self.o200k_bpe),
            cl100k_bpe: Arc::clone(&self.cl100k_bpe),
        }
    }
}

/// Initialises both BPE tokenizer families, panicking on failure.
///
/// BPE data tables are compiled into the binary by tiktoken-rs as static byte
/// slices. Initialisation failure would indicate a corrupt build artifact, not
/// a recoverable runtime condition.
fn init_bpe_tables() -> (Arc<CoreBPE>, Arc<CoreBPE>) {
    let o200k = tiktoken_rs::o200k_base().unwrap_or_else(|err| {
        panic!("tiktoken o200k_base init failed (corrupt binary data): {err}")
    });
    let cl100k = tiktoken_rs::cl100k_base().unwrap_or_else(|err| {
        panic!("tiktoken cl100k_base init failed (corrupt binary data): {err}")
    });
    (Arc::new(o200k), Arc::new(cl100k))
}

impl TiktokenCounter {
    /// Creates a counter with default model-family mappings and no custom aliases.
    ///
    /// Both BPE tokenizer families (`o200k_base` and `cl100k_base`) are loaded
    /// eagerly. See [struct-level docs](Self) for panic behaviour.
    #[must_use]
    pub fn new() -> Self {
        let (o200k_bpe, cl100k_bpe) = init_bpe_tables();
        Self {
            aliases: HashMap::new(),
            o200k_bpe,
            cl100k_bpe,
        }
    }

    /// Creates a counter with additional model-name aliases.
    ///
    /// Each key is a model name or prefix that should map to a known
    /// [`EncodingFamily`]. Invalid family names are rejected at compile time
    /// rather than silently falling through at count time.
    ///
    /// Both BPE tokenizer families are loaded eagerly. See
    /// [struct-level docs](Self) for panic behaviour.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use polaris_models::tokenizer::{TokenCounter, TiktokenCounter, EncodingFamily};
    ///
    /// let aliases = HashMap::from([
    ///     ("my-org/custom-model".to_owned(), EncodingFamily::Cl100kBase),
    /// ]);
    /// let counter = TiktokenCounter::with_aliases(aliases);
    /// let count = counter.count_text("my-org/custom-model", "test")?;
    /// assert!(count.exact);
    /// # Ok::<(), polaris_models::tokenizer::TokenCountError>(())
    /// ```
    #[must_use]
    pub fn with_aliases(aliases: HashMap<String, EncodingFamily>) -> Self {
        let (o200k_bpe, cl100k_bpe) = init_bpe_tables();
        Self {
            aliases,
            o200k_bpe,
            cl100k_bpe,
        }
    }

    // ── internal helpers ─────────────────────────────────────────────────

    /// Returns the [`CoreBPE`] tokenizer for the given encoding family.
    fn bpe(&self, family: EncodingFamily) -> &CoreBPE {
        match family {
            EncodingFamily::O200kBase => &self.o200k_bpe,
            EncodingFamily::Cl100kBase => &self.cl100k_bpe,
        }
    }

    /// Resolves a model name to an [`EncodingFamily`], or `None` if unknown.
    fn resolve_family(&self, model: &str) -> Option<EncodingFamily> {
        // 1. Try tiktoken-rs direct lookup. If it succeeds, figure out which
        //    family the model belongs to so we can cache/optimise later.
        if tiktoken_rs::get_bpe_from_model(model).is_ok() {
            return Some(family_from_tiktoken_model(model));
        }

        // 2. Built-in prefix table.
        if let Some(family) = builtin_prefix_match(model) {
            return Some(family);
        }

        // 3. User alias table (exact match first, then longest-prefix).
        if let Some(family) = self.aliases.get(model) {
            return Some(*family);
        }
        let mut best: Option<(usize, EncodingFamily)> = None;
        for (prefix, family) in &self.aliases {
            if model.starts_with(prefix.as_str()) && best.is_none_or(|(len, _)| prefix.len() > len)
            {
                best = Some((prefix.len(), *family));
            }
        }
        if let Some((_, family)) = best {
            return Some(family);
        }

        None
    }

    /// Counts tokens using an exact BPE or the heuristic fallback.
    fn count_str(&self, model: &str, text: &str) -> Result<TokenCount, TokenCountError> {
        match self.resolve_family(model) {
            Some(family) => {
                let bpe = self.bpe(family);
                let tokens = bpe.encode_with_special_tokens(text).len();
                Ok(TokenCount::exact(tokens))
            }
            None => Ok(heuristic_count(text)),
        }
    }
}

impl Default for TiktokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TokenCounter impl
// ─────────────────────────────────────────────────────────────────────────────

impl TokenCounter for TiktokenCounter {
    fn count_text(&self, model: &str, text: &str) -> Result<TokenCount, TokenCountError> {
        self.count_str(model, text)
    }

    fn count_message(&self, model: &str, message: &Message) -> Result<TokenCount, TokenCountError> {
        let text = extract_message_text(message);
        let content_count = self.count_str(model, &text)?;
        Ok(TokenCount {
            tokens: content_count.tokens + MESSAGE_OVERHEAD_TOKENS,
            exact: content_count.exact,
        })
    }

    fn count_messages(
        &self,
        model: &str,
        messages: &[Message],
    ) -> Result<TokenCount, TokenCountError> {
        let mut total: usize = 0;
        let mut all_exact = true;

        for message in messages {
            let count = self.count_message(model, message)?;
            total = total.saturating_add(count.tokens);
            all_exact = all_exact && count.exact;
        }

        // Reply priming overhead.
        total = total.saturating_add(REPLY_PRIMING_TOKENS);

        Ok(TokenCount {
            tokens: total,
            exact: all_exact,
        })
    }

    fn count_request(
        &self,
        model: &str,
        request: &LlmRequest,
    ) -> Result<TokenCount, TokenCountError> {
        let mut total: usize = 0;
        let mut all_exact = true;

        // System prompt.
        if let Some(system) = &request.system {
            let count = self.count_str(model, system)?;
            total = total.saturating_add(count.tokens);
            all_exact = all_exact && count.exact;
        }

        // Messages.
        let msgs = self.count_messages(model, &request.messages)?;
        total = total.saturating_add(msgs.tokens);
        all_exact = all_exact && msgs.exact;

        // Tool definitions — count the JSON schema text.
        if let Some(tools) = &request.tools {
            let tools_text = tool_definitions_text(tools);
            let count = self.count_str(model, &tools_text)?;
            total = total.saturating_add(count.tokens);
            all_exact = all_exact && count.exact;
        }

        Ok(TokenCount {
            tokens: total,
            exact: all_exact,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Heuristic token estimate: `ceil(chars / CHARS_PER_TOKEN)`.
fn heuristic_count(text: &str) -> TokenCount {
    let chars = text.chars().count();
    let tokens = chars.div_ceil(CHARS_PER_TOKEN);
    TokenCount::estimate(tokens)
}

/// Determines the encoding family for a model name that tiktoken-rs already
/// recognises. Falls back to [`Cl100kBase`](EncodingFamily::Cl100kBase) when
/// the model is known but not in our prefix table (conservative default).
fn family_from_tiktoken_model(model: &str) -> EncodingFamily {
    let lower = model.to_lowercase();
    if lower.starts_with("gpt-4o")
        || lower.starts_with("gpt-4.1")
        || lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        EncodingFamily::O200kBase
    } else {
        EncodingFamily::Cl100kBase
    }
}

/// Matches well-known model-family prefixes to encoding families.
///
/// Old models (e.g. `gpt-4-turbo`, `gpt-3.5-turbo`) are handled by tiktoken-rs
/// direct lookup in step 1 of the resolution chain, so this table is biased
/// toward the newer `o200k_base` family for broad prefixes like `gpt-5`.
fn builtin_prefix_match(model: &str) -> Option<EncodingFamily> {
    let lower = model.to_lowercase();

    // OpenAI — o200k (GPT-4o, GPT-4.x, GPT-5, o-series).
    // Only match prefixes known to use o200k. Legacy gpt-4-turbo / gpt-4-32k
    // (cl100k) are caught by tiktoken-rs direct lookup in step 1. Bare
    // gpt-4-* variants unknown to tiktoken-rs fall through to the heuristic
    // rather than being silently assigned the wrong tokenizer.
    if lower.starts_with("gpt-4o")
        || lower.starts_with("gpt-4.")
        || lower.starts_with("gpt-5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        return Some(EncodingFamily::O200kBase);
    }

    // OpenAI — cl100k (older models not covered by tiktoken-rs direct lookup)
    if lower.starts_with("gpt-3.5") || lower.starts_with("text-embedding") {
        return Some(EncodingFamily::Cl100kBase);
    }

    // Anthropic models — cl100k is a close approximation, not the actual
    // Claude tokenizer. Counts are returned with `exact = true` because they
    // are BPE-produced (not heuristic), but callers should be aware that
    // token counts may differ slightly from Anthropic's internal tokenizer.
    // See [`TokenCount`] Accuracy docs.
    if lower.starts_with("claude") {
        return Some(EncodingFamily::Cl100kBase);
    }

    // Amazon Bedrock cross-region inference prefixes (e.g. "us.anthropic.claude-...").
    // Same cl100k approximation caveat as above.
    if lower.contains("anthropic.claude") {
        return Some(EncodingFamily::Cl100kBase);
    }

    None
}

/// Extracts all countable text from a message.
///
/// Non-text blocks (images, audio, documents) are not counted — their token
/// cost is provider-specific and cannot be determined by BPE alone.
fn extract_message_text(message: &Message) -> String {
    let mut parts: Vec<&str> = Vec::new();

    match message {
        Message::User { content } => {
            for block in content {
                match block {
                    UserBlock::Text(text_block) => parts.push(&text_block.text),
                    UserBlock::ToolResult(tool_result) => {
                        push_tool_result_text(&mut parts, tool_result);
                    }
                    // Images, audio, documents — skip.
                    UserBlock::Image(_) | UserBlock::Audio(_) | UserBlock::Document(_) => {}
                }
            }
        }
        Message::Assistant { content, .. } => {
            for block in content {
                match block {
                    AssistantBlock::Text(text_block) => parts.push(&text_block.text),
                    AssistantBlock::ToolCall(call) => {
                        parts.push(&call.function.name);
                        if let Some(args_str) = call.function.arguments.as_str() {
                            parts.push(args_str);
                        } else {
                            // Serialise non-string JSON arguments.
                            // We push the string representation below via the
                            // collected owned buffer approach.
                        }
                    }
                    AssistantBlock::Reasoning(reasoning) => {
                        for thought in &reasoning.reasoning {
                            parts.push(thought);
                        }
                    }
                }
            }
        }
    }

    // For tool call arguments that are not plain strings, we need owned
    // serialisations. Collect them separately to avoid lifetime issues.
    let mut owned_parts: Vec<String> = Vec::new();
    if let Message::Assistant { content, .. } = message {
        for block in content {
            if let AssistantBlock::ToolCall(call) = block
                && !call.function.arguments.is_string()
            {
                owned_parts.push(call.function.arguments.to_string());
            }
        }
    }

    let borrowed_text = parts.join(" ");
    if owned_parts.is_empty() {
        borrowed_text
    } else {
        let owned_text = owned_parts.join(" ");
        format!("{borrowed_text} {owned_text}")
    }
}

/// Pushes text from a tool result into the parts buffer.
fn push_tool_result_text<'a>(parts: &mut Vec<&'a str>, result: &'a ToolResult) {
    match &result.content {
        ToolResultContent::Text(text) => parts.push(text),
        ToolResultContent::Image(_) => {}
    }
}

/// Serialises tool definitions to a single string for token counting.
///
/// The exact serialisation format does not need to match any provider's wire
/// format — it just needs to produce a representative token count for the
/// schema overhead.
fn tool_definitions_text(tools: &[ToolDefinition]) -> String {
    let mut parts = Vec::with_capacity(tools.len());
    for tool in tools {
        parts.push(format!(
            "{}: {} {}",
            tool.name, tool.description, tool.parameters
        ));
    }
    parts.join("\n")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::llm::{TextBlock, ToolCall, ToolChoice, ToolFunction};

    fn counter() -> TiktokenCounter {
        TiktokenCounter::new()
    }

    // ── count_text ───────────────────────────────────────────────────────

    #[test]
    fn count_text_known_model_is_exact() {
        let count = counter().count_text("gpt-5.4", "Hello, world!").unwrap();
        assert_eq!(
            count,
            TokenCount::exact(4),
            "gpt-5.4 o200k should produce exactly 4 tokens"
        );
    }

    #[test]
    fn count_text_unknown_model_is_estimate() {
        // "Hello, world!" = 13 chars → ceil(13/4) = 4 heuristic tokens
        let count = counter()
            .count_text("totally-unknown-model", "Hello, world!")
            .unwrap();
        assert_eq!(
            count,
            TokenCount::estimate(4),
            "unknown model should use heuristic: ceil(13/4) = 4"
        );
    }

    #[test]
    fn count_text_empty_string() {
        let count = counter().count_text("gpt-5.4", "").unwrap();
        assert_eq!(
            count,
            TokenCount::exact(0),
            "empty string should yield 0 tokens"
        );
    }

    #[test]
    fn count_text_claude_model_is_exact() {
        let count = counter().count_text("claude-opus-4-6", "Hello!").unwrap();
        assert_eq!(
            count,
            TokenCount::exact(2),
            "claude model via cl100k should produce 2 tokens"
        );
    }

    #[test]
    fn count_text_bedrock_claude_is_exact() {
        let count = counter()
            .count_text("us.anthropic.claude-sonnet-4-20250514-v1:0", "Hello!")
            .unwrap();
        assert_eq!(
            count,
            TokenCount::exact(2),
            "bedrock claude via cl100k should produce 2 tokens"
        );
    }

    // ── alias table ──────────────────────────────────────────────────────

    #[test]
    fn alias_table_resolves_custom_model() {
        let aliases = HashMap::from([("my-model".to_owned(), EncodingFamily::Cl100kBase)]);
        let counter = TiktokenCounter::with_aliases(aliases);
        let count = counter.count_text("my-model", "Hello!").unwrap();
        assert!(count.exact, "exact-match alias should produce exact count");
    }

    #[test]
    fn alias_table_prefix_match() {
        let aliases = HashMap::from([("custom/".to_owned(), EncodingFamily::O200kBase)]);
        let counter = TiktokenCounter::with_aliases(aliases);
        let count = counter.count_text("custom/v1", "Hello!").unwrap();
        assert!(count.exact, "prefix-match alias should produce exact count");
    }

    #[test]
    fn alias_table_longest_prefix_wins() {
        let aliases = HashMap::from([
            ("custom/".to_owned(), EncodingFamily::O200kBase),
            ("custom/v2/".to_owned(), EncodingFamily::Cl100kBase),
        ]);
        let counter = TiktokenCounter::with_aliases(aliases);

        // "custom/v2/latest" should match the longer "custom/v2/" prefix.
        let o200k_count = counter.count_text("custom/v1", "Hello!").unwrap();
        let cl100k_count = counter.count_text("custom/v2/latest", "Hello!").unwrap();

        // o200k and cl100k produce different token counts for the same input,
        // so verify the longer prefix selected the correct encoding.
        let reference = TiktokenCounter::with_aliases(HashMap::from([(
            "ref".to_owned(),
            EncodingFamily::Cl100kBase,
        )]));
        let expected = reference.count_text("ref", "Hello!").unwrap();
        assert_eq!(
            cl100k_count.tokens, expected.tokens,
            "longer prefix 'custom/v2/' should select Cl100kBase"
        );
        assert!(
            o200k_count.exact,
            "shorter prefix should still produce exact count"
        );
    }

    // ── heuristic ────────────────────────────────────────────────────────

    #[test]
    fn heuristic_fallback_approximation() {
        // 12 characters → ceil(12/4) = 3 tokens
        let count = heuristic_count("Hello world!");
        assert_eq!(count.tokens, 3);
        assert!(!count.exact);
    }

    #[test]
    fn heuristic_empty_string() {
        let count = heuristic_count("");
        assert_eq!(count.tokens, 0);
        assert!(!count.exact);
    }

    // ── count_message ────────────────────────────────────────────────────

    #[test]
    fn count_user_text_message() {
        let c = counter();
        let msg = Message::user("Hello, how are you?");
        let content_count = c.count_text("gpt-5.4", "Hello, how are you?").unwrap();
        let msg_count = c.count_message("gpt-5.4", &msg).unwrap();
        assert!(msg_count.exact, "known model should produce exact count");
        assert_eq!(
            msg_count.tokens,
            content_count.tokens + MESSAGE_OVERHEAD_TOKENS,
            "message count should equal content tokens ({}) + overhead ({})",
            content_count.tokens,
            MESSAGE_OVERHEAD_TOKENS,
        );
    }

    #[test]
    fn count_assistant_text_message() {
        let c = counter();
        let msg = Message::assistant("I'm doing well, thanks!");
        let content_count = c.count_text("gpt-5.4", "I'm doing well, thanks!").unwrap();
        let msg_count = c.count_message("gpt-5.4", &msg).unwrap();
        assert!(msg_count.exact, "known model should produce exact count");
        assert_eq!(
            msg_count.tokens,
            content_count.tokens + MESSAGE_OVERHEAD_TOKENS,
            "message count should equal content tokens ({}) + overhead ({})",
            content_count.tokens,
            MESSAGE_OVERHEAD_TOKENS,
        );
    }

    #[test]
    fn count_message_with_tool_call() {
        let msg = Message::Assistant {
            id: None,
            content: vec![AssistantBlock::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                call_id: None,
                function: ToolFunction {
                    name: "get_weather".to_owned(),
                    arguments: json!({"city": "London"}),
                },
                signature: None,
                additional_params: None,
            })],
        };
        let count = counter().count_message("gpt-5.4", &msg).unwrap();
        assert!(
            count.exact,
            "tool call message with known model should be exact"
        );
        assert!(
            count.tokens > MESSAGE_OVERHEAD_TOKENS,
            "tool call should contribute tokens beyond overhead, got {}",
            count.tokens,
        );
    }

    #[test]
    fn count_message_with_tool_result() {
        let msg = Message::tool_result("call_1", ToolResultContent::Text("Sunny, 22C".to_owned()));
        let count = counter().count_message("gpt-5.4", &msg).unwrap();
        assert!(
            count.exact,
            "tool result message with known model should be exact"
        );
        assert!(
            count.tokens > MESSAGE_OVERHEAD_TOKENS,
            "tool result should contribute tokens beyond overhead, got {}",
            count.tokens,
        );
    }

    // ── count_messages ───────────────────────────────────────────────────

    #[test]
    fn count_messages_includes_reply_priming() {
        let c = counter();
        let messages = vec![Message::user("Hi"), Message::assistant("Hello!")];
        // Compute expected: sum of per-message counts + reply priming
        let m1 = c.count_message("gpt-5.4", &messages[0]).unwrap();
        let m2 = c.count_message("gpt-5.4", &messages[1]).unwrap();
        let expected = m1.tokens + m2.tokens + REPLY_PRIMING_TOKENS;

        let count = c.count_messages("gpt-5.4", &messages).unwrap();
        assert!(count.exact, "known model should produce exact count");
        assert_eq!(
            count.tokens, expected,
            "messages count should be sum of per-message counts + reply priming"
        );
    }

    #[test]
    fn count_messages_empty() {
        let count = counter().count_messages("gpt-5.4", &[]).unwrap();
        assert_eq!(
            count,
            TokenCount::exact(REPLY_PRIMING_TOKENS),
            "empty message list should only contain reply priming tokens"
        );
    }

    // ── count_request ────────────────────────────────────────────────────

    #[test]
    fn count_request_with_system_and_tools() {
        let c = counter();
        let request = LlmRequest {
            system: Some("You are a helpful assistant.".to_owned()),
            messages: vec![Message::user("What's the weather?")],
            tools: Some(vec![ToolDefinition {
                name: "get_weather".to_owned(),
                description: "Get weather for a city".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"}
                    },
                    "required": ["city"]
                }),
            }]),
            tool_choice: Some(ToolChoice::Auto),
            output_schema: None,
        };
        // Compute each component independently to verify composition.
        let system_count = c
            .count_str("gpt-5.4", "You are a helpful assistant.")
            .unwrap();
        let msgs_count = c.count_messages("gpt-5.4", &request.messages).unwrap();
        let tools_text = tool_definitions_text(request.tools.as_ref().unwrap());
        let tools_count = c.count_str("gpt-5.4", &tools_text).unwrap();
        let expected = system_count.tokens + msgs_count.tokens + tools_count.tokens;

        let count = c.count_request("gpt-5.4", &request).unwrap();
        assert!(count.exact, "known model request should be exact");
        assert_eq!(
            count.tokens, expected,
            "request count should be system + messages + tools"
        );
    }

    #[test]
    fn count_request_minimal() {
        let c = counter();
        let request = LlmRequest {
            system: None,
            messages: vec![Message::user("Hi")],
            tools: None,
            tool_choice: None,
            output_schema: None,
        };
        let msgs_count = c.count_messages("gpt-5.4", &request.messages).unwrap();
        let count = c.count_request("gpt-5.4", &request).unwrap();
        assert!(count.exact, "known model request should be exact");
        assert_eq!(
            count.tokens, msgs_count.tokens,
            "minimal request (no system/tools) should equal messages count"
        );
    }

    // ── exactness propagation ────────────────────────────────────────────

    #[test]
    fn unknown_model_propagates_inexact_through_request() {
        let request = LlmRequest {
            system: Some("System".to_owned()),
            messages: vec![Message::user("Hello")],
            tools: None,
            tool_choice: None,
            output_schema: None,
        };
        let count = counter()
            .count_request("unknown-model-xyz", &request)
            .unwrap();
        assert!(!count.exact);
    }

    // ── extract_message_text ─────────────────────────────────────────────

    #[test]
    fn extract_text_from_user_message() {
        let msg = Message::User {
            content: vec![
                UserBlock::Text(TextBlock::from("first")),
                UserBlock::Text(TextBlock::from("second")),
            ],
        };
        let text = extract_message_text(&msg);
        assert!(
            text.contains("first"),
            "first text block should be included"
        );
        assert!(
            text.contains("second"),
            "second text block should be included"
        );
    }

    #[test]
    fn extract_text_skips_images() {
        let msg = Message::User {
            content: vec![
                UserBlock::text("visible"),
                UserBlock::image_base64("data", crate::llm::ImageMediaType::PNG),
            ],
        };
        let text = extract_message_text(&msg);
        assert!(text.contains("visible"), "text block should be included");
        assert!(!text.contains("data"), "image data should be excluded");
    }

    // ── extract_message_text: reasoning & tool argument paths ─────────

    #[test]
    fn extract_text_includes_reasoning() {
        let msg = Message::Assistant {
            id: None,
            content: vec![
                AssistantBlock::reasoning("let me think about this"),
                AssistantBlock::text("The answer is 42."),
            ],
        };
        let text = extract_message_text(&msg);
        assert!(
            text.contains("let me think about this"),
            "reasoning block text should be extracted"
        );
        assert!(
            text.contains("The answer is 42."),
            "text block should be extracted"
        );
    }

    #[test]
    fn extract_text_includes_string_tool_arguments() {
        let msg = Message::Assistant {
            id: None,
            content: vec![AssistantBlock::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                call_id: None,
                function: ToolFunction {
                    name: "search".to_owned(),
                    arguments: json!("raw string query"),
                },
                signature: None,
                additional_params: None,
            })],
        };
        let text = extract_message_text(&msg);
        assert!(text.contains("search"), "tool name should be extracted");
        assert!(
            text.contains("raw string query"),
            "string-typed tool arguments should be extracted via as_str() path"
        );
    }

    #[test]
    fn extract_text_skips_image_tool_result() {
        let img = crate::llm::ImageBlock {
            data: crate::llm::DocumentSource::Base64("abc".to_owned()),
            media_type: crate::llm::ImageMediaType::PNG,
            additional_params: None,
        };
        let msg = Message::User {
            content: vec![UserBlock::ToolResult(ToolResult {
                id: "call_1".to_owned(),
                call_id: None,
                content: ToolResultContent::Image(img),
                status: crate::llm::ToolResultStatus::Success,
            })],
        };
        let text = extract_message_text(&msg);
        assert!(
            text.trim().is_empty(),
            "image tool result should produce no countable text, got: {text:?}"
        );
    }

    // ── model prefix resolution ─────────────────────────────────────────

    #[test]
    fn builtin_prefix_resolves_o200k_families() {
        for model in [
            "gpt-5.4",
            "gpt-5-preview",
            "gpt-4.1-turbo",
            "o1-mini",
            "o3-medium",
            "o4-large",
        ] {
            let count = counter().count_text(model, "test").unwrap();
            assert!(
                count.exact,
                "model {model} should resolve to an encoding family"
            );
        }
    }

    #[test]
    fn builtin_prefix_resolves_cl100k_families() {
        for model in ["gpt-3.5-turbo-instruct", "text-embedding-3-small"] {
            let count = counter().count_text(model, "test").unwrap();
            assert!(
                count.exact,
                "model {model} should resolve to cl100k encoding family"
            );
        }
    }

    // ── error path coverage ─────────────────────────────────────────────

    /// A mock counter that always returns `EncodingFailed`.
    struct FailingCounter;

    impl TokenCounter for FailingCounter {
        fn count_text(&self, _model: &str, _text: &str) -> Result<TokenCount, TokenCountError> {
            Err(TokenCountError::EncodingFailed("mock failure".to_owned()))
        }

        fn count_message(
            &self,
            model: &str,
            _message: &Message,
        ) -> Result<TokenCount, TokenCountError> {
            self.count_text(model, "")
        }

        fn count_messages(
            &self,
            model: &str,
            _messages: &[Message],
        ) -> Result<TokenCount, TokenCountError> {
            self.count_text(model, "")
        }

        fn count_request(
            &self,
            model: &str,
            _request: &LlmRequest,
        ) -> Result<TokenCount, TokenCountError> {
            self.count_text(model, "")
        }
    }

    #[test]
    fn error_variant_is_returned_by_failing_counter() {
        let err = FailingCounter.count_text("any-model", "test").unwrap_err();
        match err {
            TokenCountError::EncodingFailed(msg) => {
                assert_eq!(msg, "mock failure", "error payload should propagate");
            }
        }
    }

    #[test]
    fn error_propagates_through_request() {
        let request = LlmRequest {
            system: Some("System".to_owned()),
            messages: vec![Message::user("Hello")],
            tools: None,
            tool_choice: None,
            output_schema: None,
        };
        let err = FailingCounter
            .count_request("any-model", &request)
            .unwrap_err();
        assert!(
            matches!(err, TokenCountError::EncodingFailed(_)),
            "error should propagate through count_request"
        );
    }

    // ── Tokenizer delegation ────────────────────────────────────────────

    #[test]
    fn tokenizer_wrapper_delegates_count_text() {
        use crate::tokenizer::Tokenizer;

        let inner = counter();
        let expected = inner.count_text("gpt-5.4", "Hello, world!").unwrap();

        let wrapper = Tokenizer::new(Arc::new(counter()));
        let actual = wrapper.count_text("gpt-5.4", "Hello, world!").unwrap();
        assert_eq!(
            actual, expected,
            "Tokenizer wrapper should delegate to inner counter"
        );
    }

    #[test]
    fn tokenizer_wrapper_delegates_count_request() {
        use crate::tokenizer::Tokenizer;

        let request = LlmRequest {
            system: Some("System".to_owned()),
            messages: vec![Message::user("Hello")],
            tools: None,
            tool_choice: None,
            output_schema: None,
        };

        let inner = counter();
        let expected = inner.count_request("gpt-5.4", &request).unwrap();

        let wrapper = Tokenizer::new(Arc::new(counter()));
        let actual = wrapper.count_request("gpt-5.4", &request).unwrap();
        assert_eq!(
            actual, expected,
            "Tokenizer wrapper should delegate count_request to inner counter"
        );
    }

    // ── plugin integration ──────────────────────────────────────────────

    #[tokio::test]
    async fn tokenizer_plugin_registers_resource() {
        use polaris_system::server::Server;

        use crate::tokenizer::{Tokenizer, TokenizerPlugin};

        let mut server = Server::new();
        server.add_plugins(TokenizerPlugin::default());
        server.finish().await;

        let ctx = server.create_context();
        assert!(
            ctx.contains_resource::<Tokenizer>(),
            "TokenizerPlugin should register Tokenizer resource"
        );
    }

    #[tokio::test]
    async fn tokenizer_plugin_with_custom_counter() {
        use polaris_system::server::Server;

        use crate::tokenizer::{Tokenizer, TokenizerPlugin};

        /// A counter that always returns a fixed count for verification.
        struct FixedCounter;

        impl TokenCounter for FixedCounter {
            fn count_text(&self, _model: &str, _text: &str) -> Result<TokenCount, TokenCountError> {
                Ok(TokenCount::exact(999))
            }

            fn count_message(
                &self,
                _model: &str,
                _message: &Message,
            ) -> Result<TokenCount, TokenCountError> {
                Ok(TokenCount::exact(999))
            }

            fn count_messages(
                &self,
                _model: &str,
                _messages: &[Message],
            ) -> Result<TokenCount, TokenCountError> {
                Ok(TokenCount::exact(999))
            }

            fn count_request(
                &self,
                _model: &str,
                _request: &LlmRequest,
            ) -> Result<TokenCount, TokenCountError> {
                Ok(TokenCount::exact(999))
            }
        }

        let custom = Arc::new(FixedCounter);
        let mut server = Server::new();
        server.add_plugins(TokenizerPlugin::new(custom));
        server.finish().await;

        let ctx = server.create_context();
        assert!(
            ctx.contains_resource::<Tokenizer>(),
            "TokenizerPlugin::new() should register Tokenizer resource"
        );

        let tokenizer = ctx
            .get_resource::<Tokenizer>()
            .expect("Tokenizer resource should be retrievable");
        let count = tokenizer
            .count_text("any-model", "test")
            .expect("FixedCounter should not fail");
        assert_eq!(
            count,
            TokenCount::exact(999),
            "custom FixedCounter should return 999 through the plugin wiring"
        );
    }

    // ── Tokenizer delegation: count_message & count_messages ────────

    #[test]
    fn tokenizer_wrapper_delegates_count_message() {
        use crate::tokenizer::Tokenizer;

        let msg = Message::user("Hello!");
        let inner = counter();
        let expected = inner.count_message("gpt-5.4", &msg).unwrap();

        let wrapper = Tokenizer::new(Arc::new(counter()));
        let actual = wrapper.count_message("gpt-5.4", &msg).unwrap();
        assert_eq!(
            actual, expected,
            "Tokenizer wrapper should delegate count_message to inner counter"
        );
    }

    #[test]
    fn tokenizer_wrapper_delegates_count_messages() {
        use crate::tokenizer::Tokenizer;

        let messages = vec![Message::user("Hi"), Message::assistant("Hello!")];
        let inner = counter();
        let expected = inner.count_messages("gpt-5.4", &messages).unwrap();

        let wrapper = Tokenizer::new(Arc::new(counter()));
        let actual = wrapper.count_messages("gpt-5.4", &messages).unwrap();
        assert_eq!(
            actual, expected,
            "Tokenizer wrapper should delegate count_messages to inner counter"
        );
    }

    // ── error propagation: count_message & count_messages ───────────

    #[test]
    fn error_propagates_through_count_message() {
        let msg = Message::user("Hello");
        let err = FailingCounter.count_message("any-model", &msg).unwrap_err();
        assert!(
            matches!(err, TokenCountError::EncodingFailed(_)),
            "error should propagate through count_message"
        );
    }

    #[test]
    fn error_propagates_through_count_messages() {
        let messages = vec![Message::user("Hello")];
        let err = FailingCounter
            .count_messages("any-model", &messages)
            .unwrap_err();
        assert!(
            matches!(err, TokenCountError::EncodingFailed(_)),
            "error should propagate through count_messages"
        );
    }

    // ── heuristic: unicode ──────────────────────────────────────────

    #[test]
    fn heuristic_counts_unicode_chars_not_bytes() {
        // "héllo" = 5 chars but 6 bytes (é is 2 bytes in UTF-8)
        let count = heuristic_count("héllo");
        assert_eq!(
            count.tokens, 2,
            "heuristic should count chars (5), not bytes (6): ceil(5/4) = 2"
        );
        assert!(!count.exact, "heuristic should not be exact");

        // CJK: "你好世界" = 4 chars, 12 bytes
        let count = heuristic_count("你好世界");
        assert_eq!(
            count.tokens, 1,
            "heuristic should count CJK chars (4), not bytes (12): ceil(4/4) = 1"
        );
    }

    // ── Display impl ────────────────────────────────────────────────

    #[test]
    fn token_count_display_exact() {
        let count = TokenCount::exact(42);
        assert_eq!(format!("{count}"), "42 tokens", "exact count display");
    }

    #[test]
    fn token_count_display_estimate() {
        let count = TokenCount::estimate(10);
        assert_eq!(format!("{count}"), "~10 tokens", "estimate count display");
    }

    // ── TokenCountError Display ─────────────────────────────────────

    #[test]
    fn token_count_error_display() {
        let err = TokenCountError::EncodingFailed("test failure".to_owned());
        assert_eq!(
            format!("{err}"),
            "encoding failed: test failure",
            "error Display should include variant message"
        );
    }
}
