//! Verifies that every leaf-crate Cargo feature is reachable from the
//! `polaris-ai` umbrella crate via the `polaris_internal` propagation layer.
//!
//! Also flags references inside the umbrella or intermediate that point at
//! features (or crates) that do not exist — those are typos that would
//! otherwise produce a confusing "feature not found" error far from the
//! Cargo.toml that introduced it.
//!
//! Run as:
//!     cargo run -p feature-parity
//!
//! Exits non-zero with a list of orphans / dangling references on failure.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const UMBRELLA: &str = "polaris-ai";
const INTERMEDIATE: &str = "polaris_internal";

/// Leaf features intentionally not propagated to the umbrella. Each entry
/// here is a feature you've decided must be opted into by depending on the
/// leaf crate directly.
const ALLOWLIST: &[(&str, &str)] = &[
    // ("polaris_core_plugins", "private-experimental-thing"),
];

type Features = BTreeMap<String, Vec<String>>;

struct Manifest {
    name: String,
    features: Features,
}

fn main() -> ExitCode {
    let workspace_root = locate_workspace_root();

    let umbrella = parse_manifest(&workspace_root.join("Cargo.toml"));
    let intermediate =
        parse_manifest(&workspace_root.join(format!("crates/{INTERMEDIATE}/Cargo.toml")));

    let mut leaves: BTreeMap<String, Features> = BTreeMap::new();
    let crates_dir = workspace_root.join("crates");
    for entry in std::fs::read_dir(&crates_dir).expect("read crates/") {
        let path = entry.expect("entry").path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("Cargo.toml");
        if !manifest_path.exists() {
            continue;
        }
        let m = parse_manifest(&manifest_path);
        if m.name == INTERMEDIATE {
            continue;
        }
        leaves.insert(m.name, m.features);
    }

    let mut errors: Vec<String> = Vec::new();

    // Check 1: every `polaris_internal/X` referenced in the umbrella resolves
    // to a real intermediate feature.
    for (umbrella_feat, values) in &umbrella.features {
        for value in values {
            let (dep, feat) = parse_ref(value);
            if dep == Some(INTERMEDIATE)
                && let Some(feat) = feat
                && !intermediate.features.contains_key(feat)
            {
                errors.push(format!(
                    "dangling: `{UMBRELLA}/{umbrella_feat}` references \
                     `{INTERMEDIATE}/{feat}`, which is not defined in \
                     `crates/{INTERMEDIATE}/Cargo.toml`",
                ));
            }
        }
    }

    // Check 2: every leaf reference in the intermediate resolves to a real
    // crate + feature.
    for (intermediate_feat, values) in &intermediate.features {
        for value in values {
            let (dep, feat) = parse_ref(value);
            let Some(dep) = dep else { continue };
            if dep == INTERMEDIATE {
                continue;
            }
            let Some(feat) = feat else { continue };
            match leaves.get(dep) {
                None => errors.push(format!(
                    "dangling: `{INTERMEDIATE}/{intermediate_feat}` \
                     references crate `{dep}` which is not a workspace leaf",
                )),
                Some(crate_features) if !crate_features.contains_key(feat) => errors.push(format!(
                    "dangling: `{INTERMEDIATE}/{intermediate_feat}` \
                     references `{dep}/{feat}`, which is not defined in \
                     `crates/{dep}/Cargo.toml`",
                )),
                _ => {}
            }
        }
    }

    // Check 3: every non-allowlisted leaf feature is reachable from the
    // umbrella via the intermediate.
    let reachable = compute_reachable(&umbrella.features, &intermediate.features);
    let allowlist: HashSet<(&str, &str)> = ALLOWLIST.iter().copied().collect();

    let mut orphans: BTreeSet<(String, String)> = BTreeSet::new();
    for (crate_name, feats) in &leaves {
        for feat_name in feats.keys() {
            if feat_name == "default" {
                continue;
            }
            if allowlist.contains(&(crate_name.as_str(), feat_name.as_str())) {
                continue;
            }
            if !reachable.contains(&(crate_name.clone(), feat_name.clone())) {
                orphans.insert((crate_name.clone(), feat_name.clone()));
            }
        }
    }

    if !orphans.is_empty() {
        errors.push(format!(
            "orphan leaf features — defined in leaf crate but unreachable \
             from `{UMBRELLA}`:",
        ));
        for (crate_name, feat) in &orphans {
            errors.push(format!("  - {crate_name}/{feat}"));
        }
        errors.push(format!(
            "fix: expose each feature through `crates/{INTERMEDIATE}/Cargo.toml` \
             and the root `Cargo.toml`, or, if intentional, add it to \
             ALLOWLIST in tools/feature-parity/src/main.rs",
        ));
    }

    if errors.is_empty() {
        println!("ok: all leaf features reachable from `{UMBRELLA}`");
        return ExitCode::SUCCESS;
    }

    for line in &errors {
        eprintln!("{line}");
    }
    ExitCode::FAILURE
}

fn locate_workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at `<root>/tools/feature-parity`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tools/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn parse_manifest(path: &Path) -> Manifest {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let parsed: toml::Table =
        toml::from_str(&raw).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));
    let name = parsed
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_else(|| panic!("no [package].name in {}", path.display()))
        .to_string();
    let features = parsed
        .get("features")
        .and_then(|f| f.as_table())
        .map(|tbl| {
            tbl.iter()
                .map(|(k, v)| {
                    let arr = v.as_array().unwrap_or_else(|| {
                        panic!("[features].{k} in {} is not an array", path.display())
                    });
                    let vals = arr
                        .iter()
                        .filter_map(|x| x.as_str())
                        .map(str::to_string)
                        .collect();
                    (k.clone(), vals)
                })
                .collect()
        })
        .unwrap_or_default();
    Manifest { name, features }
}

/// Parse one entry from a feature's value list.
///
/// Returns `(dep_name, feat_name)` for `dep/feat` and `dep?/feat` forms,
/// `(None, Some(feat))` for same-crate references, and `(None, None)` for
/// `dep:foo` activation entries.
fn parse_ref(value: &str) -> (Option<&str>, Option<&str>) {
    if value.starts_with("dep:") {
        return (None, None);
    }
    match value.split_once('/') {
        Some((dep, feat)) => {
            let dep = dep.strip_suffix('?').unwrap_or(dep);
            (Some(dep), Some(feat))
        }
        None => (None, Some(value)),
    }
}

/// Set of `(leaf_crate, feature)` reachable from any non-default umbrella
/// feature through `polaris_internal`.
fn compute_reachable(umbrella: &Features, intermediate: &Features) -> HashSet<(String, String)> {
    let mut intermediate_seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    for values in umbrella.values() {
        for value in values {
            let (dep, feat) = parse_ref(value);
            if dep == Some(INTERMEDIATE)
                && let Some(feat) = feat
                && intermediate_seen.insert(feat.to_string())
            {
                queue.push_back(feat.to_string());
            }
        }
    }

    while let Some(feat) = queue.pop_front() {
        let Some(values) = intermediate.get(&feat) else {
            continue;
        };
        for value in values {
            let (dep, sub_feat) = parse_ref(value);
            if dep.is_none()
                && let Some(sub) = sub_feat
                && intermediate_seen.insert(sub.to_string())
            {
                queue.push_back(sub.to_string());
            }
        }
    }

    let mut leaves: HashSet<(String, String)> = HashSet::new();
    for feat in &intermediate_seen {
        let Some(values) = intermediate.get(feat) else {
            continue;
        };
        for value in values {
            let (dep, leaf_feat) = parse_ref(value);
            if let (Some(dep), Some(leaf_feat)) = (dep, leaf_feat)
                && dep != INTERMEDIATE
            {
                leaves.insert((dep.to_string(), leaf_feat.to_string()));
            }
        }
    }
    leaves
}
