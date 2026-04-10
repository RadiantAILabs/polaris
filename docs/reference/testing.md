# Testing

Testing in Polaris is structured by the same layered architecture that governs the framework itself. Each layer has different invariants, and tests at the wrong layer verify the wrong thing. This document defines what "tested" means at each layer, what contracts tests must verify, and where the boundaries between test levels lie.

## Why testing has structure

The philosophy states that Polaris is designed to "reduce the distance between a design hypothesis and an observable result." Tests are part of that distance. A test that requires a full server, graph, and plugin chain to verify a single system's output has placed unnecessary distance between the hypothesis ("this system produces the right value") and the result.

The layered architecture means each layer has its own invariants. Layer 1 invariants are about primitive correctness. Layer 2 invariants are about structural and behavioral properties. Layer 3 invariants are about contract compliance and replaceability. Mixing these in a single test obscures which invariant failed and why.

## Test levels

Three test levels apply across all layers:

| Level | What it verifies | First line of defense |
|-------|------------------|-----------------------|
| **Compile-time** | Type contracts, parameter validity, trait bounds | `cargo build` / `cargo clippy` |
| **Unit** | A single primitive in isolation, given controlled inputs | `cargo test` |
| **Integration** | Multiple primitives composed through the framework | `cargo test` |

The compiler is the first line of defense. The type system, `SystemParam` validation, and trait bounds catch entire categories of errors before any test runs. Tests fill what the compiler cannot check. Integration tests exist only when composition introduces behavior that unit tests on individual parts cannot cover.

## Layer 1: System Framework — Primitive Correctness

Layer 1 tests verify that primitives behave as specified in isolation.

### Systems

A system is a pure async function from resources to output. Its test contract: given a `SystemContext` with known resources, the system produces the expected output.

- Fallible systems must be tested on both `Ok` and `Err` paths.
- A system's correctness is independent of any graph, plugin, or server. If a system test requires a `Server`, it is testing at the wrong layer.
- The `SystemContext::new().with(T)` builder is the canonical way to provide test resources. It is intentionally lightweight.

### Plugins

A plugin's contract is registration: after its lifecycle methods run, the expected resources exist with the expected properties.

- A plugin must be testable with only its declared dependencies — never `DefaultPlugins`.
- If a plugin uses two-phase init (mutable in `build`, frozen in `ready`), both phases are part of the contract.
- Registration is the primitive being tested, not the resource's behavior. A plugin test that asserts on resource values is testing the resource, not the plugin.

### Macros

Macro correctness is a compile-time property. Invalid inputs must produce helpful diagnostics.

- Use `trybuild` with `compile_fail` tests and `.stderr` snapshots.
- Valid inputs must produce the expected trait implementations.
- Each error path the macro can take should have a corresponding `compile_fail` case.

## Layer 2: Graph Execution — Structural and Behavioral Properties

Layer 2 tests verify that graphs are structurally valid and that execution follows the topology.

### Structure

A graph's structural validity is decidable before execution. `validate()` is the first test — it must accept valid topologies and reject invalid ones.

- Structural tests verify node counts, edge connectivity, and entry/exit reachability.
- They do not require a `Server` or `SystemContext`. A graph's shape is independent of its runtime.
- Each node type (system, decision, switch, parallel, loop) and edge type has structural rules that validation must enforce.

### Execution

An execution test verifies that control flow follows the graph topology for a given set of inputs.

- The expected path through the graph should be deterministic given the same inputs.
- Parallel branches are the exception: assert on the *set* of results, never the order.
- Error handler and timeout handler routing are behavioral properties of the executor, not of individual systems.
- Always call `validate()` before `execute()`. Structural errors caught statically produce better diagnostics than runtime failures.

### Hooks

Hook tests verify that lifecycle events fire in the correct sequence and carry the correct data.

- The event sequence is determined by the topology and the execution path — it is a derived property, not an independent one.
- Hook tests should verify ordering relative to execution flow, not absolute timing.

### Composition properties

Graph composition operators have algebraic properties: identity, associativity, closure, idempotent-safety. These are mathematical invariants, not behavioral tests.

- They should hold for *any* graph, not just specific test cases.
- Property-based testing is the appropriate level. A single hardcoded graph proves one case; a generated graph proves the property.

## Layer 3: Plugins — Contract Compliance and Replaceability

Layer 3 tests verify that implementations fulfill their trait contracts and that the plugin system enables replaceability.

### Trait implementations

A trait implementation is tested against its trait contract, independent of any plugin or server.

- The implementation's correctness is a property of the type, not the framework.
- If the trait defines error conditions, edge cases, or accuracy semantics, the implementation must be tested against all of them.
- Framework wiring is not part of this concern. A `TokenCounter` implementation should be testable with `TiktokenCounter::new()` alone, no `Server` required.

### Plugin integration

A plugin integration test verifies that the plugin correctly wires its implementation into the framework.

- Resources are registered, trait objects are accessible, and the plugin works with only its declared dependencies.
- This is not a test of the implementation — it is a test of the registration contract.
- The minimal viable integration test: register plugin, call `finish()`, assert the expected global or local resources exist.

### Replaceability

The philosophy says every component is replaceable. This is a testable property.

- A mock implementation that satisfies the trait must be substitutable without changing consuming code.
- If a plugin accepts `Arc<dyn Trait>`, a test should verify that a mock implementation can be injected and that the framework functions identically.
- A plugin that only exposes `::default()` with a hardcoded implementation is not replaceable and does not meet this criterion.

### Tools

Tool tests verify schema correctness, input diversity, and error handling. Tools accept JSON input and produce structured output through the `ToolRegistry`.

- **Schema validation**: Tool macros generate JSON schemas from type definitions and doc comments. Tests should verify that doc comments produce correct descriptions, `#[default]` values appear in the schema, required vs optional fields are correct, and parameter types serialize as expected. Call `.definitions()` on the tool or toolset and assert on the JSON structure.
- **Input diversity**: Each distinct input shape exercises a different deserialization path. Test with: required fields only, all fields, optional fields as `null` or absent, default values, nested structs, and tagged enums if applicable. `.execute(serde_json::json!({...}))` with varying input shapes is the established pattern.
- **Error paths**: Missing required parameters, invalid parameter types, and unknown tool names in the registry must return `ToolError` variants, not panic. `ToolError::parameter_error()` for missing input; the registry returns `Err` for unknown tool names.
- Tool correctness is a property of the tool type, not the framework. A tool should be testable by constructing it directly and calling `.execute()` — no `Server` or plugin chain required.

## Test quality

Beyond structural correctness at each layer, tests must be *useful* — they must diagnose failures clearly, resist flakiness, and avoid coupling to implementation details.

### Assertion diagnostics

A failing test should say *what went wrong* without requiring a debugger. Non-trivial assertions need custom messages:

```rust
assert!(result.is_ok(), "execute failed: {result:?}");
```

Multiple `assert_eq!` calls in sequence on the same type should have distinguishing context. Assertion depth should be proportional to operation complexity — `.is_ok()` is sufficient for a simple constructor returning `Ok(())`, but not for an executor producing a complex result. Verify the value inside, not just the variant.

### Flakiness prevention

Flaky tests erode trust in the test suite and waste debugging time. Three categories of flakiness are preventable by convention:

- **Timing assumptions**: `sleep(Duration)` followed by an assertion that depends on the sleep completing "in time" is fragile. Use condition-based synchronization (channels, barriers, watch variables) or framework-level timeouts. The exception is tests where timing IS the behavior under test (e.g., `SlowSystem` + timeout edge).
- **Ordering assumptions**: `HashMap`/`HashSet` iteration order, async task scheduling order, and thread interleaving are non-deterministic. Tests that assert on sequences from inherently unordered operations will fail intermittently. Use sorted comparisons, set equality, or `contains` checks instead of positional assertions.
- **Undocumented ignores**: `#[ignore]` without a comment explaining *why* and *when it can be un-ignored* represents a known gap in coverage that may never be addressed. Every `#[ignore]` must have a reason.

### Test isolation

Each test must create its own state and clean up its side effects:

- No shared `static` mutables that leak between tests (even with `AtomicU32` — read the usage pattern).
- Temp files created but not cleaned up, `tokio::spawn` without join/abort, bound ports not released (use ephemeral port 0) — anything persisting beyond the test function risks contaminating parallel or subsequent tests.
- `lazy_static` or `once_cell` for read-only fixtures is acceptable.

## Plugin testability — Dependency isolation

The philosophy says every component is testable in isolation. For plugins with dependencies, this requires that testing PluginC does not require manually registering the full chain of PluginA and PluginB.

Two mechanisms support this:

### Injectable constructors

If a plugin wraps a trait object, it must expose a constructor that accepts the trait object. A test can inject a mock without registering upstream plugins at all. This is a design convention, not a framework feature — but it is a hard requirement for plugin design. The trait object boundary is the testability boundary.

### Auto-registration of default dependencies

Plugins can provide default instances of their dependencies. When opted in at server configuration or plugin registration time, the server auto-registers missing dependencies using these defaults, walking the chain recursively. Already-registered plugins are not replaced.

This is opt-in. The strict mode (panic on missing dependency) remains the default. Auto-registration is primarily a testing convenience but also reduces boilerplate for applications that prefer convention over configuration.

The injectable constructor is always the first mechanism to reach for. Auto-registration is a complement for cases where the full dependency chain is needed but the test does not care about the specific implementations in that chain.

## Doctests — Executable documentation

Doctests are not tests in the verification sense. They are executable documentation that prevents API examples from rotting. Their purpose is to keep the documented API surface honest.

**Precedence:** Runnable > `no_run` > `ignore`. A doctest should run unless it genuinely cannot — network calls, filesystem side effects, or runtime setup unavailable in the doctest harness. `no_run` means "compiles but cannot execute here." `ignore` is a last resort when the snippet cannot compile in a doctest context.

**Style conventions:**
- `?` over `.unwrap()`, with a hidden `Ok::<(), ErrorType>(())` trailer.
- `tracing` over `println!` (workspace `print_stdout` lint).
- Hide boilerplate with `#` prefix lines.
- Systems in doctests cannot return `Result`; use `if let Ok(...)` instead.

## Boundaries

Testing at the wrong layer is the most common testing anti-pattern in a layered framework. This table maps concerns to their correct layer:

| Concern | Wrong layer | Right layer |
|---------|-------------|-------------|
| System produces correct output | Graph execution test | System unit test |
| Graph follows correct path | System unit test | Graph execution test |
| Plugin registers resources | Graph execution test | Plugin registration test |
| Resource accessible in context | Isolated resource test | Plugin or graph integration test |
| Macro rejects invalid input | Runtime test | Compile-time test (`trybuild`) |
| Composition is associative | Single-case execution test | Property-based test |
| Parallel branches complete | Ordered assertion | Set assertion |
| Tool schema matches spec | Plugin integration test | Tool unit test |
| Tool handles missing params | Graph execution test | Tool unit test |

## Anti-patterns

**Testing at the wrong layer.** A system unit test that constructs a full `Server` and `Graph` is testing framework wiring, not system logic. A graph test that hardcodes system output is testing the system, not the graph.

**Mocking framework types.** `SystemContext`, `Server`, and `Graph` are designed to be lightweight in tests. Mock the *implementation* (trait objects), not the *framework*.

**Asserting parallel order.** Parallel branches have no guaranteed execution order. Assert on result sets, not sequences.

**Skipping structural validation.** `graph.validate()` catches errors statically. Skipping it shifts errors to runtime, increasing the distance between hypothesis and result.

**Testing private internals.** Test the public contract. If an internal helper needs its own tests, it likely belongs at `pub(crate)` visibility.

**Bare `.is_ok()` / `.is_err()` on complex operations.** A graph executor returning `Ok` does not mean it ran the right path. An error-producing function returning `Err` does not mean it produced the *right* error. Assert on the value or variant, not just the discriminant.

**Undocumented `#[ignore]`.** An ignored test with no explanation is a silent coverage gap. Document why it is ignored and under what conditions it can be re-enabled.
