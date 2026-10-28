//! Integration tests for the [`OutAll`] system parameter.

use polaris_system::param::inspect::{Inspection, InspectionSink, ParamKind, ParamMeta};
use polaris_system::param::{Out, OutAll, SystemContext, SystemParam};
use polaris_system::resource::ProducerKey;
use polaris_system::system;
use polaris_system::system::System;
use std::sync::Mutex;

#[derive(Debug)]
struct Value(u8);

#[derive(Debug, PartialEq)]
struct Provenance(&'static str);

fn producer(node: &'static str, name: &'static str) -> ProducerKey {
    ProducerKey::new(node).with_provenance(Provenance(name))
}

#[test]
fn returns_sequential_entries_oldest_first_with_producers() {
    let mut ctx = SystemContext::new();
    ctx.insert_output_from(producer("first", "first system"), Value(1));
    ctx.insert_output_from(producer("second", "second system"), Value(2));

    let values = OutAll::<Value>::fetch(&ctx).expect("all output values must resolve");

    assert_eq!(
        values.iter().map(|entry| entry.0).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(values[0].producer().id(), "first");
    assert_eq!(
        values[0].producer().provenance::<Provenance>(),
        Some(&Provenance("first system"))
    );
    assert_eq!(values[1].producer().id(), "second");
    assert_eq!(
        values[1].producer().provenance::<Provenance>(),
        Some(&Provenance("second system"))
    );

    assert_eq!(OutAll::<Value>::access(), Out::<Value>::access());
}

#[test]
fn follows_parallel_branch_merge_order() {
    let mut parent = SystemContext::new();
    let first_branch = {
        let mut branch = parent.child();
        branch.insert_output_from(producer("branch-0-a", "branch zero a"), Value(1));
        branch.insert_output_from(producer("branch-0-b", "branch zero b"), Value(2));
        branch.take_outputs()
    };
    let second_branch = {
        let mut branch = parent.child();
        branch.insert_output_from(producer("branch-1", "branch one"), Value(3));
        branch.take_outputs()
    };

    // The graph executor performs these joins in branch declaration order.
    parent.outputs_mut().merge_from(first_branch);
    parent.outputs_mut().merge_from(second_branch);

    let values = OutAll::<Value>::fetch(&parent).expect("branch outputs must resolve");
    assert_eq!(
        values.iter().map(|entry| entry.0).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        values
            .iter()
            .map(|entry| entry.producer().id())
            .collect::<Vec<_>>(),
        ["branch-0-a", "branch-0-b", "branch-1"]
    );
}

#[test]
fn empty_context_returns_an_empty_list() {
    let ctx = SystemContext::new();
    let values = OutAll::<Value>::fetch(&ctx).expect("an empty output list is valid");

    assert!(values.is_empty());
}

#[derive(Default)]
struct CollectingSink(Mutex<Vec<(ParamMeta, Inspection)>>);

impl InspectionSink for CollectingSink {
    fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
        self.0
            .lock()
            .expect("sink mutex poisoned")
            .push((meta, render()));
    }
}

#[system(inspect(values))]
async fn inspect_all(values: OutAll<Value>) -> usize {
    values.len()
}

#[tokio::test]
async fn inspection_records_ordered_values_as_out_all() {
    let sink = std::sync::Arc::new(CollectingSink::default());
    let mut ctx = SystemContext::new();
    ctx.insert_output_from(producer("first", "first system"), Value(1));
    ctx.insert_output_from(producer("second", "second system"), Value(2));
    let _ = ctx.replace_inspection(sink.clone());

    assert_eq!(inspect_all().run(&ctx).await.expect("system must run"), 2);

    let records = sink.0.lock().expect("sink mutex poisoned");
    assert_eq!(records.len(), 1);
    let (meta, inspection) = &records[0];
    assert_eq!(meta.kind, ParamKind::OutAll);
    assert_eq!(meta.type_name, "Value");
    assert_eq!(
        inspection,
        &Inspection::Text {
            value: "[Value(1), Value(2)]".into(),
            truncated: false,
        }
    );
}
