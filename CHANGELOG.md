# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-04-16

### Added

- **`ToolContext` and `#[context]` parameter injection** (`polaris_tools`) — per-invocation context propagation into `#[tool]` functions. `ToolContext` is a lightweight typed map that carries per-invocation state (session IDs, working directories, locales, dry-run flags, opaque backend handles, or any other caller-supplied value) from the calling system into tool execution. Values are stored behind `Arc`, so `ToolContext` is cheaply `Clone` regardless of whether individual value types are. Tools declare context dependencies with `#[context]` on parameters; these are extracted from `ToolContext` at runtime, do not appear in the LLM-facing JSON schema, and require `T: Clone`. `Option<T>` context params are `None` when absent instead of erroring.
- **`ToolRegistry::execute_with(name, args, ctx)`** (`polaris_tools`) — context-aware tool execution. The existing `execute(name, args)` remains as a convenience that passes an empty context.

### Changed

- **`Tool::execute` signature** (breaking) — now takes `&ToolContext` as a second parameter: `execute<'ctx>(&'ctx self, args: Value, ctx: &'ctx ToolContext) -> Pin<Box<...>>`. Existing manual `Tool` impls must add `_ctx: &'ctx ToolContext`. Macro-generated tools update automatically.
- **`#[context]` rejects nested `Option<Option<T>>`** — the `#[tool]` / `#[toolset]` macros now emit a compile-time error for `#[context]` parameters typed `Option<Option<T>>`. Use `Option<T>` for an optional context value; the outer `Option` already expresses absence.
- **`TracingPlugin::with_capture_genai_content` documentation** — doc comment now warns that tool arguments and results are captured verbatim on spans, so tools returning credentials, PII, or other secrets will have those values recorded when this flag is enabled.
