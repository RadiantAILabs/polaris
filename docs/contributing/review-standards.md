# Pull Request Review Standards

Use these standards to self-review a contribution and to evaluate pull requests consistently. Apply only the checks relevant to the change, and focus on defects introduced or materially worsened by the pull request.

## Authority and interpretation

Apply repository guidance in this order:

1. [`docs/philosophy.md`](../philosophy.md) — architectural purpose and hard principles
2. [`docs/taxonomy.md`](../taxonomy.md) — layer responsibilities and dependency direction
3. [`docs/reference/*.md`](../reference/) — detailed behavioral contracts
4. This document — cross-cutting review expectations
5. A linked design document — intended change-specific design

A design document guides implementation but cannot override the philosophy or taxonomy. A matching implementation can still contain a design flaw. A justified implementation may diverge from a stale design document, but the document should then be updated.

Distinguish requirements from preferences. Raise a blocking finding only when evidence shows a violated invariant, incorrect behavior, unsafe outcome, unsupported compatibility break, or missing required contract.

## Finding quality and severity

For every substantive finding, provide:

- **Invariant** — what must remain true
- **Counterexample** — the concrete condition that violates it
- **Evidence** — relevant code and authoritative documentation
- **Impact** — the observable architectural, user, security, or maintenance consequence
- **Correction** — the smallest sound change

Use these severities:

| Severity | Meaning |
|---|---|
| **Blocker** | Violates philosophy, safety, security, or a foundational invariant; corrupts/leaks state; deadlocks; permits unbounded incorrect behavior; or makes supported composition impossible. |
| **Major** | Breaks a credible supported path, public contract, lifecycle, or compatibility expectation. Fix before merge. |
| **Follow-up** | A lower-risk edge, documentation gap, or accepted tradeoff that is explicitly recorded and can be handled after merge. |
| **Nit** | Non-blocking polish. Never present a preference as a correctness issue. |

## Architecture and design

### Philosophy gate

Check every affected layer:

- **Layer placement:** Keep agent-specific, domain-specific, and optional behavior out of Layer 1. Keep concrete agent patterns and Layer 3 capabilities out of Layer 2. Prevent dependencies from pointing upward.
- **Primitive admission:** Add a Layer 1 or Layer 2 primitive only when existing composition cannot express the behavior cleanly, the primitive applies across unrelated agent patterns, and Layer 3 registration or composition is insufficient.
- **ECS separation:** Keep state inspectable in appropriately scoped resources, behavior in systems, and data dependencies explicit through typed parameters and outputs. Typed provider resources may expose behavior; opaque service locators and working-state containers must not hide dependencies.
- **Graph visibility:** Keep material agent-level routing, retry, relaxation, escalation, and termination inspectable in graph topology or typed decision nodes. Local computation may branch inside a system, and a model may produce a typed decision that the graph routes.
- **Plugin composition:** Deliver optional capabilities through narrow plugins and public capability contracts. Avoid concrete provider requirements, hidden registration order, and access to another plugin's internals.

For a new Layer 1 or Layer 2 primitive, require the proposal and implementation to define applicable context, output, error, timeout, nesting, validation, hook, and middleware semantics. Treat a new enum variant or match arm as a prompt for this analysis, not an automatic failure.

### Design soundness

Trace the affected design across its full lifecycle:

1. Construction → validation → execution → cleanup
2. Input → ownership/scope → mutation → merge or persistence
3. Failure → classification → retry/routing → terminal outcome
4. Registration → dependency resolution → replacement → removal

Evaluate:

| Area | Questions |
|---|---|
| Responsibility | Does each abstraction have a coherent owner and stable boundary? Does the change create duplicate authority or a central chokepoint? |
| State and data flow | Does each value have the correct server/session/turn/scope/system lifetime? Are shadowing, output overwrite, child isolation, persistence, and parallel merge intentional? |
| Composition | Can consumers combine components and replace providers without editing unrelated code? Are capability cardinality, dependencies, defaults, ordering, and extension points explicit? |
| Lifecycle | Are partial initialization, ready/freeze transitions, repeated calls, re-entry, cleanup ordering, and owned external resources defined? |
| Graph semantics | Are validation, execution, outputs, errors, timeouts, loops, scopes, dynamic graphs, hooks, middleware, and nesting coherent for the affected behavior? |
| Failure and cancellation | Are normal terminal outcomes distinct from errors? Is structured classification preserved? Are unknowns fail-closed, retries bounded, repeated effects safe, and partial mutation understood? |
| Concurrency | Is the consistency model defined for shared state, parallel branches, competing mutations, ordering, merge conflicts, and re-entry? |
| Evolution | Are downstream consumers, contract versions, persisted data, feature combinations, migrations, rollout, and rollback accounted for? |
| Observability and testability | Can contributors inspect provider resolution, selected topology, routing decisions, and failure provenance? Can dependencies be substituted to isolate invariants? |
| Structural cost | Are loops, recursion, fan-out, queues, retained state, and cleanup work bounded? Does a new central path scale with graph, plugin, session, or input size as intended? |

Stress the design with missing, empty, duplicate, nested, parallel, repeated, partially failed, cancelled, version-mismatched, upgraded, and alternate-provider cases. Report a design issue only when a credible case produces an invariant violation.

## Rust API quality

Use the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) for public Rust interfaces.

Review new and changed public items for:

- Conventional naming, getters, conversions, iterators, and constructors
- Newtypes and enums where bare primitives, `bool`, or loosely typed values would erase meaning
- Clear ownership, borrowing, and generic boundaries
- `Send`, `Sync`, object safety, and applicable common trait implementations
- Meaningful error types, early validation, and documented failure behavior
- Predictable conversions, defaults, implicit allocation, cloning, and drop behavior
- Cancellation safety, blocking work, guards across `await`, and runtime assumptions
- Builder and composition behavior that preserves validation and does not panic on foreseeable input
- Private representation details and a viable evolution path for downstream users
- Complete rustdoc examples and `# Errors`, `# Panics`, and `# Safety` sections

Apply Polaris-specific contracts:

- Match `Out<T>` producers with typed downstream consumers, predicates, and discriminators.
- Confirm `Res<T>` and `ResMut<T>` semantics across nested and parallel contexts.
- Add validation for new graph primitives and invalid compositions.
- Declare plugin capability relationships and pure ordering dependencies explicitly.
- Preserve object safety for core framework traits intended for dynamic dispatch.

Assess compatibility using Cargo's [SemVer compatibility guidance](https://doc.rust-lang.org/cargo/reference/semver.html). Treat changes to public items, feature availability, error behavior, serialization, capability versions, and the MSRV as compatibility-sensitive. Document intentional breaking or possibly breaking changes and provide a migration path when practical.

## Test quality

Follow the [testing strategy](../reference/testing.md) and test the invariant at the layer that owns it.

Review tests for:

- Observable behavior rather than private implementation details
- Assertions deep enough to distinguish the intended result or error variant
- Normal, error, boundary, missing-resource, cancellation, and concurrency paths affected by the change
- Self-contained setup and cleanup of tasks, files, ports, environment, and mutable state
- Deterministic assertions that do not depend on timing, scheduling, or unordered iteration
- Explicit graph validation and verification of which path executed
- Both sides of branches and each material switch/error/timeout route
- Plugin build, ready, cleanup, dependency ordering, isolation, and replaceability when applicable
- Parent-child resource resolution and borrow conflicts when scoping changes
- `trybuild` success and failure diagnostics for macro changes
- Runnable doctests or justified `no_run`; documented reasons for every `#[ignore]`

Do not require a full server or graph to test a system or trait implementation that can be exercised directly. Do not mock lightweight framework primitives when the implementation boundary can be mocked instead.

## Documentation and discoverability

Require documentation in the same pull request when behavior, configuration, features, public APIs, or integration patterns change.

For every new or materially changed exported item:

- Document purpose, when to use it, lifecycle, failure behavior, and a realistic example.
- Apply the [plugin](../reference/plugins.md#documentation-standard), [API](../reference/api.md#documentation-standard), or [resource](../reference/resources.md#documentation-standard) standard when applicable.
- Add plugins, APIs, and consumer-facing resources to the corresponding catalog under `src/docs/`.
- Update the [integration guide](../reference/guide.md) when the change adds a new way to accomplish a downstream goal.
- Use intra-doc links for workspace types and ensure rustdoc builds with warnings denied.
- Check related documents for terminology, behavior, and example drift.

Review design-document alignment semantically. Do not flag incidental field order or spelling unless the document commits to it as a compatibility boundary. Record material missing decisions, sound implementation divergence, and unjustified implementation drift separately.

## Security and dependency risk

Apply security review wherever data or behavior crosses a trust boundary.

- Keep unsafe code minimal, encapsulated behind a safe contract, and supported by a valid safety argument.
- Validate and bound HTTP, tool, model, file, and deserialized input before use.
- Preserve authentication and authorization on stateful or sensitive routes.
- Prevent shell injection, path traversal, environment leakage, and check/use races.
- Avoid panics, unbounded allocation, recursion, fan-out, and retention driven by untrusted input.
- Do not expose secrets, credentials, private data, internal paths, or debug representations in code, fixtures, logs, or client errors.
- Justify new dependencies, features, sources, licenses, and transitive attack surface.
- Use structured errors without leaking sensitive internals across public boundaries.

Treat exploitability and unsafe soundness as blockers. Keep general architecture findings distinct from security findings, even when the same root cause affects both.

## Change-specific checklist

| Change | Required review focus |
|---|---|
| Layer 1 or Layer 2 primitive | Philosophy gate, primitive admission, all execution semantics, validation, compatibility, focused tests, and design discussion before implementation |
| Graph or context behavior | Scope/output flow, nesting, validation, failure/cancellation, determinism, execution-path tests, and reference docs |
| Plugin, API, or resource | Capability relationships, lifecycle, replaceability, isolation tests, full rustdoc standard, catalogs, and integration guide |
| Public Rust API | Rust API Guidelines, SemVer impact, MSRV, examples, errors, downstream call sites, and re-exports |
| HTTP, shell, file, model, or deserialization boundary | Validation, authentication, injection, bounds, sensitive errors/logs, and adversarial tests |
| Macro | Generated visibility and attributes, flexible input syntax, compile-pass/fail tests, diagnostics, and generated public docs |
| Cargo dependency or feature | Necessity, source/license/security, MSRV, isolated feature compilation, umbrella propagation, and compatibility |
| Persisted or serialized type | Backward/forward compatibility, migration, partial/corrupt data, versioning, rollback, and round-trip tests |
| TypeScript-derived type | Rust/TypeScript contract, regenerated bindings, barrel exports, and drift checks |
| Documentation only | Markdown lint, links, terminology, examples, and consistency with higher-precedence architecture docs |

## Before approval

Confirm that:

- The pull request contains one logical change and uses the correct diff base.
- Every blocking or major finding is resolved or explicitly superseded by an accepted design decision.
- Required tests and documentation are present at the correct layer.
- Compatibility, migration, feature, and generated-artifact implications are recorded.
- `cargo make test` passes, plus change-specific feature or example checks.
- The pull request description explains why the final design is sound, not only what files changed.

## External standards

- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [Cargo SemVer compatibility](https://doc.rust-lang.org/cargo/reference/semver.html)
- [Cargo `rust-version` and MSRV](https://doc.rust-lang.org/cargo/reference/rust-version.html)
- [Clippy documentation](https://doc.rust-lang.org/clippy/)
