//! Integration tests for `#[system(inspect(..))]` parameter capture.
//!
//! These live in an integration test because the macro generates code using
//! `::polaris_system::` paths, which only resolve when the crate is an external
//! dependency.

use polaris_system::param::inspect::{
    DEFAULT_MAX_BYTES, InspectParam, Inspection, InspectionSink, ParamKind, ParamMeta, Phase,
};
use polaris_system::param::{
    ErrOut, ErrorContext, Out, ParamError, ParentFilter, Res, ResMut, SystemContext, SystemParam,
};
use polaris_system::resource::LocalResource;
use polaris_system::system;
use polaris_system::system::{System, SystemError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct Memory {
    messages: Vec<String>,
}

impl LocalResource for Memory {}

#[derive(Debug)]
struct Config {
    multiplier: i32,
}

impl LocalResource for Config {}

#[derive(Debug)]
struct Upstream {
    value: i32,
}

#[derive(Debug)]
struct Reply {
    total: i32,
}

/// A resource whose `Debug` impl records that it ran.
///
/// Used to prove that a value is never formatted unless a sink actually asks
/// for it. A flag rather than a panic, because a panicking `Debug` is absorbed
/// by the render boundary — a panic-based tripwire would pass even if the
/// gating were broken.
struct Tripwire {
    rendered: Arc<AtomicBool>,
}

impl LocalResource for Tripwire {}

impl std::fmt::Debug for Tripwire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.rendered.store(true, Ordering::SeqCst);
        f.write_str("tripwire")
    }
}

/// A resource whose `Debug` impl panics — untrusted code at its worst.
struct Grenade;

impl LocalResource for Grenade {}

impl std::fmt::Debug for Grenade {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("deliberate panic from a buggy Debug impl");
    }
}

/// Sink that renders and stores every record it is offered.
#[derive(Default)]
struct Collecting {
    records: Mutex<Vec<(ParamMeta, Inspection)>>,
}

impl Collecting {
    fn records(&self) -> Vec<(ParamMeta, Inspection)> {
        self.records.lock().expect("sink mutex poisoned").clone()
    }

    /// Returns the rendered text for `param`, or `None` if it was not recorded.
    fn text(&self, param: &str) -> Option<String> {
        self.records().into_iter().find_map(|(meta, inspection)| {
            match (meta.param == param, inspection) {
                (true, Inspection::Text { value, .. }) => Some(value),
                _ => None,
            }
        })
    }

    fn params(&self) -> Vec<&'static str> {
        self.records()
            .into_iter()
            .map(|(meta, _)| meta.param)
            .collect()
    }
}

impl InspectionSink for Collecting {
    fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
        self.records
            .lock()
            .expect("sink mutex poisoned")
            .push((meta, render()));
    }
}

/// Sink that is offered records but never renders them.
#[derive(Default)]
struct Declining {
    offered: Mutex<Vec<ParamMeta>>,
}

impl InspectionSink for Declining {
    fn record(&self, meta: ParamMeta, _render: &dyn Fn() -> Inspection) {
        self.offered.lock().expect("sink mutex poisoned").push(meta);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Selection is per parameter
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(memory, upstream))]
async fn selective(
    config: Res<Config>,
    mut memory: ResMut<Memory>,
    upstream: Out<Upstream>,
) -> Reply {
    memory.messages.push("ran".into());
    Reply {
        total: upstream.value * config.multiplier,
    }
}

#[tokio::test]
async fn records_only_the_selected_params() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new()
        .with(Config { multiplier: 3 })
        .with(Memory {
            messages: vec!["seed".into()],
        });
    ctx.insert_output(Upstream { value: 7 });
    let _ = ctx.replace_inspection(sink.clone());

    let reply = selective().run(&ctx).await.expect("system must succeed");
    assert_eq!(reply.total, 21);

    // `config` was not named, so it must not appear.
    let mut params = sink.params();
    params.sort_unstable();
    assert_eq!(params, vec!["memory", "upstream"]);
}

#[tokio::test]
async fn records_carry_system_name_kind_and_type() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new()
        .with(Config { multiplier: 1 })
        .with(Memory { messages: vec![] });
    ctx.insert_output(Upstream { value: 1 });
    let _ = ctx.replace_inspection(sink.clone());

    selective().run(&ctx).await.expect("system must succeed");

    let records = sink.records();
    let (memory_meta, _) = records
        .iter()
        .find(|(meta, _)| meta.param == "memory")
        .expect("memory must be recorded");

    assert_eq!(memory_meta.system, "selective");
    assert_eq!(memory_meta.kind, ParamKind::ResMut);
    assert_eq!(memory_meta.phase, Phase::Before);
    // The recorded type is the inner resource, not the wrapper: the kind
    // already says `ResMut`, and the inner type is invariant across the
    // `ResMut<Memory>` / `ResMut<'_, Memory>` spellings.
    assert_eq!(memory_meta.type_name, "Memory");

    let (upstream_meta, _) = records
        .iter()
        .find(|(meta, _)| meta.param == "upstream")
        .expect("upstream must be recorded");
    assert_eq!(upstream_meta.kind, ParamKind::Out);
}

// ─────────────────────────────────────────────────────────────────────────────
// `ResMut` is captured through the live borrow
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(memory))]
async fn mutating(mut memory: ResMut<Memory>) {
    memory.messages.push("added".into());
}

#[tokio::test]
async fn resmut_is_recorded_before_the_body_and_mutation_still_applies() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Memory {
        messages: vec!["before".into()],
    });
    let _ = ctx.replace_inspection(sink.clone());

    mutating().run(&ctx).await.expect("system must succeed");

    // The record is the value going in, not the mutated one.
    let recorded = sink.text("memory").expect("memory must be recorded");
    assert!(
        recorded.contains("before") && !recorded.contains("added"),
        "expected the pre-body value, got {recorded:?}"
    );

    // Capturing must not have disturbed the mutation.
    let memory = ctx.get_resource::<Memory>().expect("memory must exist");
    assert_eq!(memory.messages, vec!["before".to_string(), "added".into()]);
}

#[test]
fn inspection_reads_a_resource_a_competing_reader_cannot() {
    let mut ctx = SystemContext::new();
    ctx.insert(Memory {
        messages: vec!["held".into()],
    });

    // Hold the write borrow, exactly as a system does for the duration of its body.
    let borrowed = <ResMut<Memory> as SystemParam>::fetch(&ctx).expect("fetch must succeed");

    // A separate reader is locked out at this moment — specifically by the
    // held borrow, not by the resource being absent — which is why a registry
    // that reads the resource store independently cannot see this value.
    assert!(
        matches!(
            ctx.get_resource::<Memory>(),
            Err(ParamError::BorrowConflict(_))
        ),
        "a competing reader must be blocked by the held write borrow"
    );

    // Inspection reads through the borrow already held, so it still sees it.
    let Inspection::Text { value, truncated } = InspectParam::inspect(&borrowed) else {
        panic!("a Debug resource must render as text");
    };
    assert!(!truncated);
    assert!(value.contains("held"), "got {value:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Return value capture
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(return))]
async fn producing(config: Res<Config>) -> Reply {
    Reply {
        total: config.multiplier * 2,
    }
}

#[tokio::test]
async fn records_the_return_value() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Config { multiplier: 5 });
    let _ = ctx.replace_inspection(sink.clone());

    let reply = producing().run(&ctx).await.expect("system must succeed");
    assert_eq!(reply.total, 10);

    let records = sink.records();
    let (meta, inspection) = records.first().expect("the return value must be recorded");
    assert_eq!(meta.param, "return");
    assert_eq!(meta.kind, ParamKind::Return);
    assert_eq!(meta.phase, Phase::After);
    assert_eq!(meta.type_name, "Reply");
    let Inspection::Text { value, .. } = inspection else {
        panic!("expected rendered text");
    };
    assert!(value.contains("10"), "got {value:?}");
}

#[system(inspect(return))]
async fn fallible_producing(config: Res<Config>) -> Result<Reply, SystemError> {
    // `?` inside a fallible body must still typecheck around the binding the
    // return-capture introduces — including a `?` that needs a genuine `From`
    // conversion (`ParamError` -> `SystemError`), which relies on the block's
    // ascribed error type.
    let extra: i32 = Ok::<i32, ParamError>(0)?;
    let doubled = i32::checked_mul(config.multiplier, 2)
        .ok_or_else(|| SystemError::ExecutionError("overflow".into()))?;
    Ok(Reply {
        total: doubled + extra,
    })
}

#[tokio::test]
async fn records_the_return_value_of_a_fallible_system() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Config { multiplier: 4 });
    let _ = ctx.replace_inspection(sink.clone());

    let reply = fallible_producing()
        .run(&ctx)
        .await
        .expect("system must succeed");
    assert_eq!(reply.total, 8);

    let recorded = sink.text("return").expect("return must be recorded");
    assert!(recorded.contains('8'), "got {recorded:?}");

    // The recorded type is the extracted success type, not the declared
    // `Result<Reply, SystemError>`.
    let records = sink.records();
    let (meta, _) = records
        .iter()
        .find(|(meta, _)| meta.param == "return")
        .expect("return must be recorded");
    assert_eq!(meta.type_name, "Reply");
}

#[system(inspect(return))]
async fn early_returning(config: Res<Config>) -> Result<Reply, SystemError> {
    // A `return` inside a closure is the closure's own exit, never the
    // system's — it must not disturb the capture.
    let clamp = |value: i32| -> i32 {
        if value > 100 {
            return 100;
        }
        value
    };

    // An explicit `return Ok(..)` — the capture must still see this value
    // rather than being skipped by the early exit.
    if config.multiplier >= 0 {
        return Ok(Reply {
            total: clamp(config.multiplier),
        });
    }
    Ok(Reply { total: 0 })
}

#[tokio::test]
async fn an_early_return_ok_is_still_recorded() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Config { multiplier: 6 });
    let _ = ctx.replace_inspection(sink.clone());

    let reply = early_returning()
        .run(&ctx)
        .await
        .expect("system must succeed");
    assert_eq!(reply.total, 6);

    let recorded = sink
        .text("return")
        .expect("an early `return Ok(..)` must be recorded, not silently skipped");
    assert!(recorded.contains('6'), "got {recorded:?}");
}

#[system(inspect(return))]
async fn failing() -> Result<Reply, SystemError> {
    Err(SystemError::ExecutionError("deliberate".into()))
}

#[tokio::test]
async fn a_failing_system_records_no_return_value() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new();
    let _ = ctx.replace_inspection(sink.clone());

    let error = failing().run(&ctx).await.expect_err("system must fail");
    assert!(matches!(error, SystemError::ExecutionError(_)));

    // A failure produces no output, so there is nothing to record. The error
    // itself travels the graph's own error flow, not the inspection path.
    assert!(sink.records().is_empty());
}

#[system(inspect(return))]
async fn question_mark_failing(config: Res<Config>) -> Result<Reply, SystemError> {
    // Fails through `?` rather than an explicit `Err(..)` tail — the other
    // spelling of the error path around the capture site.
    let total = i32::checked_mul(config.multiplier, i32::MAX)
        .ok_or_else(|| SystemError::ExecutionError("overflow".into()))?;
    Ok(Reply { total })
}

#[tokio::test]
async fn a_question_mark_failure_records_no_return_value() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Config { multiplier: 2 });
    let _ = ctx.replace_inspection(sink.clone());

    let error = question_mark_failing()
        .run(&ctx)
        .await
        .expect_err("system must fail");
    assert!(matches!(error, SystemError::ExecutionError(_)));

    assert!(sink.records().is_empty());
}

#[system(inspect(config, return))]
async fn body_failing(config: Res<Config>) -> Result<Reply, SystemError> {
    let _ = &config;
    Err(SystemError::ExecutionError("deliberate".into()))
}

#[tokio::test]
async fn params_are_recorded_even_when_the_body_fails() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Config { multiplier: 1 });
    let _ = ctx.replace_inspection(sink.clone());

    let error = body_failing()
        .run(&ctx)
        .await
        .expect_err("system must fail");
    assert!(matches!(error, SystemError::ExecutionError(_)));

    // `Phase::Before` records mean "this value went in" — they survive a body
    // failure. Only the return record is tied to success. A sink must not
    // infer "records exist, therefore the system succeeded".
    assert_eq!(sink.params(), vec!["config"]);
}

#[system(inspect(config))]
async fn config_then_memory(config: Res<Config>, mut memory: ResMut<Memory>) {
    memory.messages.push(config.multiplier.to_string());
}

#[tokio::test]
async fn an_earlier_param_is_recorded_when_a_later_fetch_fails() {
    let sink = Arc::new(Collecting::default());

    // `config` resolves and is captured; the `memory` fetch that follows it
    // fails. The Before-phase record for `config` legitimately exists — the
    // value did go in — even though the system never ran.
    let mut ctx = SystemContext::new().with(Config { multiplier: 3 });
    let _ = ctx.replace_inspection(sink.clone());

    let error = config_then_memory()
        .run(&ctx)
        .await
        .expect_err("the memory fetch must fail");
    let SystemError::ParamError(ParamError::ResourceNotFound(missing)) = error else {
        panic!("expected ResourceNotFound, got {error:?}");
    };
    assert!(
        missing.contains("Memory"),
        "the failed fetch must be the later `memory` parameter, got {missing:?}"
    );

    assert_eq!(sink.params(), vec!["config"]);
}

#[derive(Debug)]
struct CustomError;

#[system(inspect(return))]
async fn value_level_result(config: Res<Config>) -> Result<i32, CustomError> {
    // The error type is not `SystemError`, so this system is *infallible* to
    // the macro: the whole `Result` is its output value.
    if config.multiplier > 0 {
        Ok(config.multiplier)
    } else {
        Err(CustomError)
    }
}

#[tokio::test]
async fn a_value_level_result_records_its_err_as_the_return_value() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Config { multiplier: 0 });
    let _ = ctx.replace_inspection(sink.clone());

    // Fallibility detection is syntactic — only the literal
    // `Result<T, SystemError>` spelling makes a system fallible. Any other
    // `Result` is an ordinary output, so its `Err` arm is a *successful*
    // return and is recorded like any other value.
    let output = value_level_result()
        .run(&ctx)
        .await
        .expect("the system itself succeeds; the Result is its output");
    assert!(output.is_err(), "the output value is the Err arm");

    let records = sink.records();
    let (meta, inspection) = records.first().expect("the return must be recorded");
    assert_eq!(meta.type_name, "Result<i32, CustomError>");
    let Inspection::Text { value, .. } = inspection else {
        panic!("expected rendered text");
    };
    assert!(value.contains("CustomError"), "got {value:?}");
}

#[system(inspect(return))]
async fn unit_returning() {}

#[tokio::test]
async fn a_unit_return_records_as_unit() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new();
    let _ = ctx.replace_inspection(sink.clone());

    unit_returning()
        .run(&ctx)
        .await
        .expect("system must succeed");

    let records = sink.records();
    let (meta, inspection) = records.first().expect("the unit return must be recorded");
    assert_eq!(meta.type_name, "()");
    assert_eq!(
        *inspection,
        Inspection::Text {
            value: "()".into(),
            truncated: false,
        }
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The combined form: parameters and `return` together
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(memory, upstream, return))]
async fn combined(
    config: Res<Config>,
    mut memory: ResMut<Memory>,
    upstream: Out<Upstream>,
) -> Reply {
    memory.messages.push("combined".into());
    Reply {
        total: upstream.value + config.multiplier,
    }
}

#[tokio::test]
async fn params_and_return_can_be_selected_together() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new()
        .with(Config { multiplier: 3 })
        .with(Memory { messages: vec![] });
    ctx.insert_output(Upstream { value: 4 });
    let _ = ctx.replace_inspection(sink.clone());

    let reply = combined().run(&ctx).await.expect("system must succeed");
    assert_eq!(reply.total, 7);

    let mut params = sink.params();
    params.sort_unstable();
    assert_eq!(params, vec!["memory", "return", "upstream"]);

    let records = sink.records();
    let kind_of = |param: &str| {
        records
            .iter()
            .find(|(meta, _)| meta.param == param)
            .map(|(meta, _)| meta.kind)
            .expect("record must exist")
    };
    assert_eq!(kind_of("memory"), ParamKind::ResMut);
    assert_eq!(kind_of("upstream"), ParamKind::Out);
    assert_eq!(kind_of("return"), ParamKind::Return);

    let recorded = sink.text("return").expect("return must be recorded");
    assert!(recorded.contains('7'), "got {recorded:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// A failed fetch records nothing
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_resource_propagates_and_records_nothing() {
    let sink = Arc::new(Collecting::default());

    // `mutating` inspects `memory`, but no `Memory` was inserted: the fetch
    // fails before the capture site, so the error propagates and the sink must
    // receive no partial record.
    let mut ctx = SystemContext::new();
    let _ = ctx.replace_inspection(sink.clone());

    let error = mutating().run(&ctx).await.expect_err("fetch must fail");
    assert!(matches!(
        error,
        SystemError::ParamError(ParamError::ResourceNotFound(_))
    ));

    assert!(sink.records().is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// Cost: nothing renders unless a sink asks
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(tripwire))]
async fn tripwire_system(tripwire: Res<Tripwire>) {
    let _ = &tripwire;
}

#[tokio::test]
async fn no_sink_means_no_rendering() {
    // No sink installed: the capture must short-circuit before formatting, so
    // `Tripwire`'s `Debug` is never reached.
    let rendered = Arc::new(AtomicBool::new(false));
    let ctx = SystemContext::new().with(Tripwire {
        rendered: rendered.clone(),
    });

    tripwire_system()
        .run(&ctx)
        .await
        .expect("system must succeed without a sink");

    assert!(
        !rendered.load(Ordering::SeqCst),
        "Debug ran although no sink was installed"
    );
}

#[tokio::test]
async fn a_sink_that_declines_pays_no_formatting() {
    let sink = Arc::new(Declining::default());
    let rendered = Arc::new(AtomicBool::new(false));

    let mut ctx = SystemContext::new().with(Tripwire {
        rendered: rendered.clone(),
    });
    let _ = ctx.replace_inspection(sink.clone());

    tripwire_system()
        .run(&ctx)
        .await
        .expect("system must succeed");

    // The record was offered — the sink simply chose not to render it, and
    // that choice is what saves the formatting cost.
    let offered = sink.offered.lock().expect("sink mutex poisoned");
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].param, "tripwire");
    assert!(
        !rendered.load(Ordering::SeqCst),
        "Debug ran although the sink declined the record"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// A panicking `Debug` cannot take down the system
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(bomb))]
async fn grenade_system(bomb: Res<Grenade>) {
    let _ = &bomb;
}

#[tokio::test]
async fn a_panicking_debug_cannot_take_down_the_system() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Grenade);
    let _ = ctx.replace_inspection(sink.clone());

    // The sink renders eagerly, so `Grenade`'s Debug panics mid-record. The
    // render boundary must absorb it: the system succeeds and the record says
    // why the value is missing.
    grenade_system()
        .run(&ctx)
        .await
        .expect("a panicking Debug impl must not fail the system");

    let records = sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].1,
        Inspection::Opaque("Debug implementation panicked")
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The byte cap applies through the macro capture path
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_large_value_is_truncated_through_the_macro_capture_path() {
    let sink = Arc::new(Collecting::default());

    // Twice the cap: the record must arrive truncated, closing the seam
    // between the unit-tested render funnel and the generated capture site.
    let mut ctx = SystemContext::new().with(Memory {
        messages: vec!["x".repeat(DEFAULT_MAX_BYTES * 2)],
    });
    let _ = ctx.replace_inspection(sink.clone());

    mutating().run(&ctx).await.expect("system must succeed");

    let records = sink.records();
    let (_, inspection) = records.first().expect("memory must be recorded");
    let Inspection::Text { value, truncated } = inspection else {
        panic!("expected rendered text, got {inspection:?}");
    };
    assert!(truncated, "a value past the cap must record as truncated");
    assert!(
        value.len() <= DEFAULT_MAX_BYTES,
        "cap exceeded: {} bytes retained",
        value.len()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Un-annotated systems capture nothing
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn unannotated(tripwire: Res<Tripwire>) {
    let _ = &tripwire;
}

#[tokio::test]
async fn an_unannotated_system_records_nothing_even_with_a_sink() {
    let sink = Arc::new(Collecting::default());
    let rendered = Arc::new(AtomicBool::new(false));

    let mut ctx = SystemContext::new().with(Tripwire {
        rendered: rendered.clone(),
    });
    let _ = ctx.replace_inspection(sink.clone());

    // Selection is compile-time: with no `inspect(..)`, no capture is emitted at
    // all, so even a rendering sink never sees `Tripwire`.
    unannotated()
        .run(&ctx)
        .await
        .expect("system must succeed unannotated");

    assert!(sink.records().is_empty());
    assert!(!rendered.load(Ordering::SeqCst));
}

// ─────────────────────────────────────────────────────────────────────────────
// Bare `inspect` selects every parameter
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect)]
async fn inspect_everything(config: Res<Config>, mut memory: ResMut<Memory>) {
    memory.messages.push(config.multiplier.to_string());
}

#[tokio::test]
async fn bare_inspect_selects_every_param() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new()
        .with(Config { multiplier: 2 })
        .with(Memory { messages: vec![] });
    let _ = ctx.replace_inspection(sink.clone());

    inspect_everything()
        .run(&ctx)
        .await
        .expect("system must succeed");

    let mut params = sink.params();
    params.sort_unstable();
    assert_eq!(params, vec!["config", "memory"]);
}

#[system(inspect)]
async fn inspect_everything_with_return(config: Res<Config>) -> Reply {
    Reply {
        total: config.multiplier,
    }
}

#[tokio::test]
async fn bare_inspect_does_not_select_the_return_value() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Config { multiplier: 5 });
    let _ = ctx.replace_inspection(sink.clone());

    inspect_everything_with_return()
        .run(&ctx)
        .await
        .expect("system must succeed");

    // Bare `inspect` selects every *parameter*; the return value is not a
    // parameter and must be named explicitly (`inspect(.., return)`). Pinned
    // so a future "bare inspect includes return" change is a deliberate one.
    assert_eq!(sink.params(), vec!["config"]);
}

// ─────────────────────────────────────────────────────────────────────────────
// Raw identifiers
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(memory))]
async fn raw_spelled(mut r#memory: ResMut<Memory>) {
    r#memory.messages.push("raw".into());
}

#[tokio::test]
async fn a_raw_identifier_param_is_selected_by_its_plain_spelling() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Memory {
        messages: vec!["rawhide".into()],
    });
    let _ = ctx.replace_inspection(sink.clone());

    raw_spelled().run(&ctx).await.expect("system must succeed");

    // Rust resolves `r#memory` and `memory` to the same name, so selection
    // matches across the spellings; the recorded name stays as written.
    assert_eq!(sink.params(), vec!["r#memory"]);
    let recorded = sink.text("r#memory").expect("memory must render");
    assert!(recorded.contains("rawhide"), "got {recorded:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Sink inheritance
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn child_contexts_inherit_the_sink() {
    let sink = Arc::new(Collecting::default());

    let mut parent = SystemContext::new();
    let _ = parent.replace_inspection(sink.clone());

    let mut child = parent.child();
    child.insert(Memory {
        messages: vec!["child".into()],
    });

    mutating().run(&child).await.expect("system must succeed");

    let recorded = sink.text("memory").expect("memory must be recorded");
    assert!(recorded.contains("child"), "got {recorded:?}");
}

#[tokio::test]
async fn filtered_children_inherit_the_sink() {
    let sink = Arc::new(Collecting::default());

    let mut parent = SystemContext::new();
    let _ = parent.replace_inspection(sink.clone());

    // A scope boundary filters parent *resources*, not the sink: inspection
    // still covers systems running inside the scope.
    let mut child = parent.child_filtered(ParentFilter::allow_only([]));
    child.insert(Memory {
        messages: vec!["scoped".into()],
    });

    mutating().run(&child).await.expect("system must succeed");

    let recorded = sink.text("memory").expect("memory must be recorded");
    assert!(recorded.contains("scoped"), "got {recorded:?}");
}

#[tokio::test]
async fn replacing_the_sink_redirects_subsequent_records() {
    let first = Arc::new(Collecting::default());
    let second = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Memory {
        messages: vec!["seed".into()],
    });

    let _ = ctx.replace_inspection(first.clone());
    mutating().run(&ctx).await.expect("system must succeed");

    // Installing a sink replaces any previous one.
    let _ = ctx.replace_inspection(second.clone());
    mutating().run(&ctx).await.expect("system must succeed");

    assert_eq!(first.records().len(), 1);
    assert_eq!(second.records().len(), 1);
}

#[tokio::test]
async fn a_sink_replaced_on_a_child_stays_local_to_that_child() {
    let parent_sink = Arc::new(Collecting::default());
    let child_sink = Arc::new(Collecting::default());

    let mut parent = SystemContext::new().with(Memory {
        messages: vec!["parent".into()],
    });
    let _ = parent.replace_inspection(parent_sink.clone());

    {
        // Inheritance is a copy taken at creation: replacing the sink on the
        // child redirects the child's records without touching the parent's.
        let mut child = parent.child();
        let _ = child.replace_inspection(child_sink.clone());
        child.insert(Memory {
            messages: vec!["child".into()],
        });
        mutating().run(&child).await.expect("system must succeed");
    }

    mutating().run(&parent).await.expect("system must succeed");

    assert_eq!(child_sink.records().len(), 1);
    assert_eq!(
        parent_sink.records().len(),
        1,
        "the parent's sink must see only the parent's own run"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Chaining onto an installed sink (SC-3307)
// ─────────────────────────────────────────────────────────────────────────────

/// Forwards every record to both sinks — the composition `inspection_arc` plus
/// the displacing `replace_inspection` make expressible.
struct Tee(Arc<dyn InspectionSink>, Arc<dyn InspectionSink>);

impl InspectionSink for Tee {
    fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
        self.0.record(meta, render);
        self.1.record(meta, render);
    }
}

/// Chains `addition` onto whatever `ctx` already carries, as a plugin would.
///
/// Returns the handle installed, which is what a caller needs to keep in order
/// to make re-application idempotent.
fn chain_onto(
    ctx: &mut SystemContext<'_>,
    addition: Arc<dyn InspectionSink>,
) -> Arc<dyn InspectionSink> {
    let installed: Arc<dyn InspectionSink> = match ctx.inspection_arc() {
        Some(existing) => Arc::new(Tee(existing, addition)),
        None => addition,
    };
    let _ = ctx.replace_inspection(Arc::clone(&installed));
    installed
}

#[tokio::test]
async fn a_chained_sink_records_through_the_real_capture_path() {
    let caller_sink = Arc::new(Collecting::default());
    let plugin_sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Memory {
        messages: vec!["seed".into()],
    });
    let _ = ctx.replace_inspection(caller_sink.clone());

    chain_onto(&mut ctx, plugin_sink.clone());

    mutating().run(&ctx).await.expect("system must succeed");

    // Not a hand-driven `record()` call: the value travels the macro-generated
    // capture path, so both sinks see the rendered resource.
    let from_caller = caller_sink.text("memory").expect("caller must be recorded");
    let from_plugin = plugin_sink.text("memory").expect("plugin must be recorded");
    assert!(from_caller.contains("seed"), "got {from_caller:?}");
    assert_eq!(from_caller, from_plugin);
}

#[tokio::test]
async fn a_chained_sink_survives_child_inheritance() {
    let caller_sink = Arc::new(Collecting::default());
    let plugin_sink = Arc::new(Collecting::default());

    let mut parent = SystemContext::new();
    let _ = parent.replace_inspection(caller_sink.clone());
    chain_onto(&mut parent, plugin_sink.clone());

    // The chain is a plain `Arc` in the sink slot, so `child()` inherits the
    // whole `Tee` rather than only the sink installed first.
    let mut child = parent.child();
    child.insert(Memory {
        messages: vec!["child".into()],
    });

    mutating().run(&child).await.expect("system must succeed");

    for (label, sink) in [("caller", &caller_sink), ("plugin", &plugin_sink)] {
        let recorded = sink
            .text("memory")
            .unwrap_or_else(|| panic!("{label} sink must be recorded"));
        assert!(recorded.contains("child"), "{label} got {recorded:?}");
    }
}

#[tokio::test]
async fn chaining_onto_an_empty_slot_installs_the_sink_outright() {
    let plugin_sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Memory {
        messages: vec!["bare".into()],
    });
    assert!(ctx.inspection_arc().is_none());

    // The `else` half of the recipe: with nothing to chain onto there is no
    // `Tee`, and the plugin's sink is what ends up installed.
    let installed = chain_onto(&mut ctx, plugin_sink.clone());
    assert!(Arc::ptr_eq(
        &installed,
        &(plugin_sink.clone() as Arc<dyn InspectionSink>)
    ));

    mutating().run(&ctx).await.expect("system must succeed");

    let recorded = plugin_sink.text("memory").expect("memory must be recorded");
    assert!(recorded.contains("bare"), "got {recorded:?}");
}

#[tokio::test]
async fn a_chained_sink_renders_what_the_sink_it_wraps_withheld() {
    // The withholding half of a policy sink, modelled by the sink that declines
    // every record: `Declining` never calls `render`, so on its own nothing is
    // ever formatted (`a_sink_that_declines_pays_no_formatting` pins that).
    let policy_sink = Arc::new(Declining::default());
    let plugin_sink = Arc::new(Collecting::default());
    let rendered = Arc::new(AtomicBool::new(false));

    let mut ctx = SystemContext::new().with(Tripwire {
        rendered: rendered.clone(),
    });
    let _ = ctx.replace_inspection(policy_sink.clone());

    chain_onto(&mut ctx, plugin_sink.clone());

    tripwire_system()
        .run(&ctx)
        .await
        .expect("system must succeed");

    // Policy does not travel down the chain. The declining sink withheld the
    // value, and the sink chained beside it rendered it anyway — a sink that
    // withholds only withholds for itself, so chaining onto one is a redaction
    // bypass, not a filtered feed.
    assert!(
        rendered.load(Ordering::SeqCst),
        "the chained sink must have rendered a value the wrapped sink withheld"
    );
    assert!(
        plugin_sink.text("tripwire").is_some(),
        "the chained sink must hold the rendered value"
    );

    // The wrapped sink still saw the record; it simply declined it, exactly as
    // it would have with no chain in place.
    let offered = policy_sink.offered.lock().expect("sink mutex poisoned");
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].param, "tripwire");
}

// ─────────────────────────────────────────────────────────────────────────────
// The recorded type name drops wrapper and lifetime spellings
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(config))]
async fn lifetime_spelled(config: Res<'_, Config>) {
    let _ = &config;
}

#[tokio::test]
async fn type_name_is_the_inner_type_regardless_of_spelling() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new()
        .with(Config { multiplier: 1 })
        .with(Memory { messages: vec![] });
    let _ = ctx.replace_inspection(sink.clone());

    // Same resource, two legal spellings: `Res<'_, Config>` here and
    // `Res<Config>` in `inspect_everything`. Records must group as one type.
    lifetime_spelled().run(&ctx).await.expect("must succeed");
    inspect_everything().run(&ctx).await.expect("must succeed");

    let config_records: Vec<(&'static str, ParamKind)> = sink
        .records()
        .into_iter()
        .filter(|(meta, _)| meta.param == "config")
        .map(|(meta, _)| (meta.type_name, meta.kind))
        .collect();
    assert_eq!(
        config_records,
        vec![("Config", ParamKind::Res), ("Config", ParamKind::Res)]
    );
}

mod nested {
    //! A resource referred to by a qualified path, so the recorded type name
    //! exercises the path-separator compaction (`nested::Deep`, not
    //! `nested :: Deep`).

    #[derive(Debug)]
    pub struct Deep {
        pub value: i32,
    }

    impl polaris_system::resource::LocalResource for Deep {}
}

#[system(inspect(deep))]
async fn qualified(deep: Res<nested::Deep>) -> i32 {
    deep.value
}

#[tokio::test]
async fn a_path_qualified_type_records_a_compact_type_name() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(nested::Deep { value: 11 });
    let _ = ctx.replace_inspection(sink.clone());

    let value = qualified().run(&ctx).await.expect("system must succeed");
    assert_eq!(value, 11);

    let records = sink.records();
    let (meta, _) = records.first().expect("deep must be recorded");
    assert_eq!(meta.type_name, "nested::Deep");
}

/// An alias hides the wrapper from the macro's syntactic classification: the
/// kind degrades to `Other` and the type name falls back to the declared
/// spelling — while value rendering stays correct, because trait resolution
/// sees through the alias.
type MemoryRef<'a> = ResMut<'a, Memory>;

#[system(inspect(memory))]
async fn aliased(mut memory: MemoryRef<'_>) {
    memory.messages.push("via alias".into());
}

#[tokio::test]
async fn an_aliased_wrapper_degrades_to_other_but_still_renders() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new().with(Memory {
        messages: vec!["aliased".into()],
    });
    let _ = ctx.replace_inspection(sink.clone());

    aliased().run(&ctx).await.expect("system must succeed");

    let records = sink.records();
    let (meta, _) = records.first().expect("memory must be recorded");
    assert_eq!(meta.kind, ParamKind::Other);
    assert_eq!(meta.type_name, "MemoryRef<'_>");

    let recorded = sink.text("memory").expect("memory must render");
    assert!(recorded.contains("aliased"), "got {recorded:?}");
}

#[test]
fn a_context_without_a_sink_reports_none() {
    let ctx = SystemContext::new();
    assert!(ctx.inspection().is_none());

    let ctx = ctx.with_inspection(Arc::new(Collecting::default()));
    assert!(ctx.inspection().is_some());

    // Children inherit, so installing once at the root is enough.
    assert!(ctx.child().inspection().is_some());
}

// ─────────────────────────────────────────────────────────────────────────────
// `Option<Out<T>>`
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(upstream))]
async fn optional_upstream(upstream: Option<Out<Upstream>>) -> i32 {
    upstream.map_or(0, |value| value.value)
}

#[tokio::test]
async fn absent_optional_output_records_as_opaque() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new();
    let _ = ctx.replace_inspection(sink.clone());

    let total = optional_upstream()
        .run(&ctx)
        .await
        .expect("system must succeed");
    assert_eq!(total, 0);

    let records = sink.records();
    let (meta, inspection) = records.first().expect("upstream must be recorded");
    // Both the `Option` layer and the wrapper unwrap down to the resource type;
    // the optionality shows up in the rendered value instead.
    assert_eq!(meta.kind, ParamKind::Out);
    assert_eq!(meta.type_name, "Upstream");
    assert_eq!(*inspection, Inspection::Opaque("output absent"));
}

#[tokio::test]
async fn present_optional_output_records_the_value() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new();
    ctx.insert_output(Upstream { value: 9 });
    let _ = ctx.replace_inspection(sink.clone());

    let total = optional_upstream()
        .run(&ctx)
        .await
        .expect("system must succeed");
    assert_eq!(total, 9);

    let recorded = sink.text("upstream").expect("upstream must be recorded");
    assert!(recorded.contains('9'), "got {recorded:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// `ErrOut<T>`: inspecting error context
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FailureInfo {
    message: String,
}

impl ErrorContext for FailureInfo {}

#[system(inspect(failure))]
async fn handling(failure: ErrOut<FailureInfo>) -> usize {
    failure.message.len()
}

#[tokio::test]
async fn error_context_records_through_errout() {
    let sink = Arc::new(Collecting::default());

    let mut ctx = SystemContext::new();
    ctx.insert_output(FailureInfo {
        message: "boom".into(),
    });
    let _ = ctx.replace_inspection(sink.clone());

    let length = handling().run(&ctx).await.expect("system must succeed");
    assert_eq!(length, 4);

    let records = sink.records();
    let (meta, _) = records.first().expect("failure must be recorded");
    assert_eq!(meta.kind, ParamKind::ErrOut);
    assert_eq!(meta.type_name, "FailureInfo");
    assert_eq!(meta.phase, Phase::Before);

    let recorded = sink.text("failure").expect("failure must render");
    assert!(recorded.contains("boom"), "got {recorded:?}");
}
