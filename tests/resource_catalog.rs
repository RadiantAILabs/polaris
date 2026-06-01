//! Drift guard for the resource catalog in `src/docs/resources.md`.
//!
//! Scans every crate in the workspace for `impl GlobalResource for X` and
//! `impl LocalResource for X` declarations and asserts each consumer-facing
//! resource name appears in the catalog. Adding a new resource without
//! listing it in the catalog will fail this test.
//!
//! Internal resources (plugin private state that no downstream system reads)
//! are exempt; add them to [`INTERNAL_RESOURCES`] with a brief justification
//! comment.

use std::fs;
use std::path::{Path, PathBuf};

const CATALOG_PATH: &str = "src/docs/resources.md";
const CRATES_DIR: &str = "crates";

/// Resources that exist in the workspace but are intentionally not listed in
/// the catalog because they are internal to a plugin's implementation and not
/// part of the downstream consumer surface. Keep this list short — most
/// internal resources should be `#[doc(hidden)]` or non-`pub` instead.
const INTERNAL_RESOURCES: &[&str] = &[
    // Per-node identity threaded into the context by the executor. Read only by
    // graph internals (middleware, hooks) — not declared as a `Res<NodeId>`
    // parameter in downstream systems.
    "NodeId",
];

#[test]
fn every_exported_resource_is_in_the_catalog() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let catalog_path = manifest_dir.join(CATALOG_PATH);
    let crates_dir = manifest_dir.join(CRATES_DIR);

    let catalog = fs::read_to_string(&catalog_path).unwrap_or_else(|err| {
        panic!(
            "failed to read resource catalog at {}: {err}",
            catalog_path.display()
        )
    });

    let mut found = collect_resource_impls(&crates_dir);
    found.sort();
    found.dedup();

    let missing: Vec<&String> = found
        .iter()
        .filter(|name| !INTERNAL_RESOURCES.contains(&name.as_str()))
        .filter(|name| !catalog_mentions(&catalog, name))
        .collect();

    assert!(
        missing.is_empty(),
        "the following resources are missing from {CATALOG_PATH}: {missing:?}\n\
         add an entry under the appropriate Layer/area section, or add the \
         resource to INTERNAL_RESOURCES in this test if it is intentionally \
         not part of the downstream consumer surface."
    );
}

/// Walks the workspace and collects every resource name that appears in a
/// top-level (non-indented) `impl GlobalResource for X` or `impl LocalResource
/// for X` declaration. Indented matches are skipped so that test fixtures
/// inside `#[cfg(test)] mod tests` blocks don't pollute the inventory.
fn collect_resource_impls(crates_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    visit_rs_files(crates_dir, &mut |path, contents| {
        if is_test_path(path) {
            return;
        }
        for line in contents.lines() {
            if let Some(name) = extract_resource_name(line) {
                names.push(name);
            }
        }
    });
    names
}

fn visit_rs_files(dir: &Path, visit: &mut dyn FnMut(&Path, &str)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            visit_rs_files(&path, visit);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            && let Ok(contents) = fs::read_to_string(&path)
        {
            visit(&path, &contents);
        }
    }
}

fn is_test_path(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("tests") | Some("benches") | Some("examples")
        )
    })
}

/// Returns the resource name iff `line` is exactly `impl GlobalResource for
/// <Name>` or `impl LocalResource for <Name>` at column zero (no leading
/// whitespace, no doc-comment prefix).
fn extract_resource_name(line: &str) -> Option<String> {
    const PREFIXES: &[&str] = &["impl GlobalResource for ", "impl LocalResource for "];
    for prefix in PREFIXES {
        if let Some(rest) = line.strip_prefix(prefix) {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            return Some(rest[..end].to_string());
        }
    }
    None
}

fn catalog_mentions(catalog: &str, name: &str) -> bool {
    catalog.match_indices(name).any(|(i, _)| {
        let before = catalog[..i].chars().next_back();
        let after = catalog[i + name.len()..].chars().next();
        let boundary = |c: Option<char>| match c {
            None => true,
            Some(ch) => !(ch.is_alphanumeric() || ch == '_'),
        };
        boundary(before) && boundary(after)
    })
}

#[test]
fn extract_resource_name_handles_common_shapes() {
    assert_eq!(
        extract_resource_name("impl GlobalResource for Clock {}"),
        Some("Clock".into())
    );
    assert_eq!(
        extract_resource_name("impl LocalResource for Stopwatch{"),
        Some("Stopwatch".into())
    );
    assert_eq!(
        extract_resource_name("    impl LocalResource for Counter {}"),
        None
    );
    assert_eq!(
        extract_resource_name("/// impl GlobalResource for Counter {}"),
        None
    );
    assert_eq!(
        extract_resource_name("impl<T> GlobalResource for Wrap<T> {}"),
        None
    );
}

#[test]
fn catalog_mentions_respects_word_boundaries() {
    let catalog = "[`Clock`](crate::plugins::Clock)";
    assert!(catalog_mentions(catalog, "Clock"));
    assert!(!catalog_mentions(catalog, "lock"));
    assert!(!catalog_mentions(catalog, "Cloc"));
}
