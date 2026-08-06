//! Integration tests for the [`SessionsAPI`] capability-contract registry.
//!
//! Verifies that named contracts are matched against cached agent signatures
//! by subsumption ([`GraphSignature::satisfies`]), that satisfaction is
//! computed at read time (registration order never matters), that duplicate
//! contract names are refused unless identical, and that
//! [`SessionsAPI::contract_diff`] surfaces human-readable drift.

use polaris_agent::Agent;
use polaris_graph::{Graph, GraphSignature};
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::{ContractName, SessionError, SessionsAPI};
use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// The typed input the capability slot names.
#[derive(Debug, Clone)]
struct TraceLine(#[expect(dead_code, reason = "type-level fixture")] String);
impl LocalResource for TraceLine {}

/// A resource the implementing agent's environment provides — conservative
/// signature derivation still reports it as required.
#[derive(Debug, Clone)]
struct Env(#[expect(dead_code, reason = "type-level fixture")] u8);
impl LocalResource for Env {}

/// The output the capability slot names.
#[derive(Debug, Clone)]
struct Triples(#[expect(dead_code, reason = "type-level fixture")] u32);

#[system]
async fn extract(_line: Res<TraceLine>, _env: Res<Env>) -> Triples {
    Triples(0)
}

/// Reads the slot input (plus an environment-provided extra) and produces
/// the slot output — satisfies the contract by subsumption.
struct ExtractionAgent;
impl Agent for ExtractionAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(extract);
    }
    fn name(&self) -> &'static str {
        "ExtractionAgent"
    }
}

#[system]
async fn echo() -> u32 {
    0
}

/// Neither reads the slot input nor produces the slot output.
struct EchoAgent;
impl Agent for EchoAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(echo);
    }
    fn name(&self) -> &'static str {
        "EchoAgent"
    }
}

/// Re-registration fixture: same agent-type name as [`ExtractionAgent`],
/// but a graph that no longer touches the contract's types.
struct ReworkedExtractionAgent;
impl Agent for ReworkedExtractionAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(echo);
    }
    fn name(&self) -> &'static str {
        "ExtractionAgent"
    }
}

fn sessions() -> SessionsAPI {
    SessionsAPI::new(Arc::new(InMemoryStore::new()))
}

/// The capability slot: reads a `TraceLine`, produces `Triples`.
fn self_learn_contract() -> GraphSignature {
    GraphSignature::new()
        .require_read::<TraceLine>()
        .produce::<Triples>()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn satisfied_contracts_are_advertised_regardless_of_registration_order() {
    // Agents first, contract second.
    let api = sessions();
    api.register_agent(ExtractionAgent).unwrap();
    api.register_agent(EchoAgent).unwrap();
    api.register_contract("self-learn", self_learn_contract())
        .unwrap();

    let infos = api.agent_type_infos();
    assert_eq!(infos.len(), 2, "one entry per registered agent");
    // Sorted by name.
    assert_eq!(infos[0].name, "EchoAgent");
    assert!(
        infos[0].contracts.is_empty(),
        "EchoAgent implements nothing: {:?}",
        infos[0].contracts
    );
    assert_eq!(infos[1].name, "ExtractionAgent");
    assert_eq!(
        infos[1].contracts,
        vec![ContractName::new("self-learn").unwrap()]
    );

    // Contract first, agent second — read-time satisfaction means the
    // result is identical.
    let api = sessions();
    api.register_contract("self-learn", self_learn_contract())
        .unwrap();
    api.register_agent(ExtractionAgent).unwrap();
    let infos = api.agent_type_infos();
    assert_eq!(
        infos[0].contracts,
        vec![ContractName::new("self-learn").unwrap()]
    );
}

#[test]
fn subsumption_admits_environment_provided_extra_requires() {
    let api = sessions();
    api.register_agent(ExtractionAgent).unwrap();

    // The agent also requires `Env` (its registrant's setup provides it),
    // so exact matching would refuse the narrower slot — subsumption admits.
    let signature = api.agent_signature("ExtractionAgent").unwrap();
    assert!(signature.satisfies(&self_learn_contract()));
    assert!(!signature.compatible_with(&self_learn_contract()));
}

#[test]
fn duplicate_registration_is_idempotent_but_a_conflict_errors() {
    let api = sessions();
    api.register_contract("self-learn", self_learn_contract())
        .unwrap();
    // Identical signature: no-op.
    api.register_contract("self-learn", self_learn_contract())
        .unwrap();
    // Different signature under the same name: refused, not last-write-wins.
    let err = api
        .register_contract("self-learn", GraphSignature::new().produce::<u32>())
        .unwrap_err();
    assert!(
        matches!(&err, SessionError::ContractConflict { name } if name.as_str() == "self-learn"),
        "unexpected error: {err}"
    );
}

#[test]
fn contract_names_reject_empty_and_non_normalized_values() {
    assert!(ContractName::new("").is_err());
    assert!(ContractName::new(" self-learn").is_err());
    assert!(ContractName::new("self-learn ").is_err());
    assert!(ContractName::new("self\nlearn").is_err());

    let api = sessions();
    let err = api
        .register_contract("", GraphSignature::new())
        .unwrap_err();
    assert!(matches!(err, SessionError::InvalidContractName(_)));
}

#[test]
fn contract_diff_reports_drift_and_lookup_failures() {
    let api = sessions();
    api.register_agent(ExtractionAgent).unwrap();
    api.register_agent(EchoAgent).unwrap();
    api.register_contract("self-learn", self_learn_contract())
        .unwrap();

    // The registrant's startup assertion: its own agent satisfies its own
    // contract, so the diff is empty.
    let diff = api.contract_diff("self-learn", "ExtractionAgent").unwrap();
    assert!(diff.is_empty(), "unexpected drift: {diff}");

    // A non-implementer's diff names the violated axes human-readably.
    let diff = api.contract_diff("self-learn", "EchoAgent").unwrap();
    assert!(!diff.is_empty());
    let rendered = diff.to_string();
    assert!(rendered.contains("TraceLine"), "{rendered}");
    assert!(rendered.contains("Triples"), "{rendered}");

    let err = api.contract_diff("unknown", "EchoAgent").unwrap_err();
    assert!(
        matches!(&err, SessionError::ContractNotFound(name) if name.as_str() == "unknown"),
        "unexpected error: {err}"
    );
    let err = api.contract_diff("self-learn", "NoSuchAgent").unwrap_err();
    assert!(
        matches!(&err, SessionError::AgentNotFound(name) if name == "NoSuchAgent"),
        "unexpected error: {err}"
    );
}

#[test]
fn agent_signature_is_none_for_unregistered_agents() {
    let api = sessions();
    assert!(api.agent_signature("NoSuchAgent").is_none());
}

#[test]
fn contract_version_still_matches_original_caret_consumers() {
    // The capability-contract registry was an additive addition to
    // `SessionsAPI`, so an out-of-tree consumer that pinned
    // `caret(0.1.0)` before it existed must keep resolving. This pins the
    // versioning decision: additive surface ⇒ patch/minor-compatible bump,
    // never a caret-breaking one.
    use polaris_system::plugin::{Contract, Version, VersionReq};

    assert!(
        VersionReq::caret(Version::new(0, 1, 0)).matches(SessionsAPI::CONTRACT_VERSION),
        "an additive change must not evict consumers pinned to ^0.1.0 \
         (current: {})",
        SessionsAPI::CONTRACT_VERSION
    );
}

#[test]
fn satisfied_contracts_are_advertised_sorted_by_name() {
    // Three contracts the same agent satisfies, registered out of sorted
    // order: `agent_type_infos` promises a name-sorted contract list, which
    // the single-contract tests above cannot pin.
    let api = sessions();
    api.register_agent(ExtractionAgent).unwrap();
    api.register_contract(
        "z-produces-triples",
        GraphSignature::new().produce::<Triples>(),
    )
    .unwrap();
    api.register_contract("a-self-learn", self_learn_contract())
        .unwrap();
    api.register_contract(
        "m-reads-trace",
        GraphSignature::new().require_read::<TraceLine>(),
    )
    .unwrap();

    let infos = api.agent_type_infos();
    assert_eq!(
        infos[0].contracts,
        vec![
            ContractName::new("a-self-learn").unwrap(),
            ContractName::new("m-reads-trace").unwrap(),
            ContractName::new("z-produces-triples").unwrap(),
        ],
        "satisfied contract names must be sorted"
    );
}

#[test]
fn re_registering_an_agent_replaces_its_cached_signature() {
    let api = sessions();
    api.register_agent(ExtractionAgent).unwrap();
    api.register_contract("self-learn", self_learn_contract())
        .unwrap();
    assert_eq!(
        api.agent_type_infos()[0].contracts,
        vec![ContractName::new("self-learn").unwrap()]
    );

    // Same agent-type name, new graph that no longer touches the slot
    // types: the cached signature must be replaced, or the registry keeps
    // advertising a satisfaction the agent no longer honors.
    api.register_agent(ReworkedExtractionAgent).unwrap();
    let signature = api.agent_signature("ExtractionAgent").unwrap();
    assert!(!signature.satisfies(&self_learn_contract()));
    assert!(
        api.agent_type_infos()[0].contracts.is_empty(),
        "stale contract advertisement after re-registration"
    );
}
