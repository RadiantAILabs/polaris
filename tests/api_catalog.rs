//! Drift guard for the API catalog in `src/docs/apis.md`.
//!
//! Scans every crate in the workspace for `impl API for X` declarations and
//! asserts each API name appears in the catalog. Adding a new API without
//! listing it in the catalog will fail this test.

use std::fs;
use std::path::{Path, PathBuf};

const CATALOG_PATH: &str = "src/docs/apis.md";
const CRATES_DIR: &str = "crates";

#[test]
fn every_exported_api_is_in_the_catalog() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let catalog_path = manifest_dir.join(CATALOG_PATH);
    let crates_dir = manifest_dir.join(CRATES_DIR);

    let catalog = fs::read_to_string(&catalog_path).unwrap_or_else(|err| {
        panic!(
            "failed to read API catalog at {}: {err}",
            catalog_path.display()
        )
    });

    let mut found = collect_api_impls(&crates_dir);
    found.sort();
    found.dedup();

    let missing: Vec<&String> = found
        .iter()
        .filter(|name| !catalog_mentions(&catalog, name))
        .collect();

    assert!(
        missing.is_empty(),
        "the following APIs are missing from {CATALOG_PATH}: {missing:?}\n\
         add an entry under the appropriate Layer/area section. \
         every catalog entry should link to the API's rustdoc page."
    );
}

/// Walks the workspace and collects every API name that appears in a
/// top-level (non-indented) `impl API for X` declaration. Indented matches
/// are skipped so that test fixtures inside `#[cfg(test)] mod tests` blocks
/// don't pollute the inventory.
fn collect_api_impls(crates_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    visit_rs_files(crates_dir, &mut |path, contents| {
        if is_test_path(path) {
            return;
        }
        for line in contents.lines() {
            if let Some(name) = extract_api_name(line) {
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

/// Returns the API name iff `line` is an `impl ...API for <Name>` declaration
/// at column zero (no leading whitespace, no doc-comment prefix). Both the
/// short `impl API for X` form and the qualified `impl crate::api::API for X`
/// form match — earlier the scanner only accepted the short form, so a
/// qualified-path impl could ship without a catalog entry. Generic-parameter
/// impls (`impl<T> API for ...`) are still skipped.
fn extract_api_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("impl ")?;
    // Reject `impl<...>` — the regex would have to walk the generic arg list
    // before reaching the trait, and no shipped API impl needs generics.
    if rest.starts_with('<') {
        return None;
    }
    // Walk the trait path up to ` for `. Accept identifier characters plus
    // `::` so `polaris_system::api::API` matches the same way `API` does.
    let trait_end = rest.find(" for ")?;
    let trait_path = &rest[..trait_end];
    if !trait_path
        .split("::")
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '_'))
    {
        return None;
    }
    if trait_path != "API" && !trait_path.ends_with("::API") {
        return None;
    }
    let after_for = &rest[trait_end + " for ".len()..];
    let end = after_for
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(after_for.len());
    if end == 0 {
        return None;
    }
    Some(after_for[..end].to_string())
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
fn extract_api_name_handles_common_shapes() {
    assert_eq!(
        extract_api_name("impl API for HttpRouter {}"),
        Some("HttpRouter".into())
    );
    assert_eq!(
        extract_api_name("impl API for SessionsAPI{"),
        Some("SessionsAPI".into())
    );
    assert_eq!(
        extract_api_name("impl polaris_system::api::API for SpanStoreHandle {}"),
        Some("SpanStoreHandle".into())
    );
    assert_eq!(
        extract_api_name("impl crate::api::API for Local {}"),
        Some("Local".into())
    );
    assert_eq!(extract_api_name("    impl API for Inner {}"), None);
    assert_eq!(extract_api_name("/// impl API for MyAPI {}"), None);
    assert_eq!(extract_api_name("impl<T> API for Wrap<T> {}"), None);
    // Trait names that merely *contain* "API" are not the target trait.
    assert_eq!(extract_api_name("impl APIWrapper for X {}"), None);
    assert_eq!(extract_api_name("impl PartialAPI for X {}"), None);
}

#[test]
fn catalog_mentions_respects_word_boundaries() {
    let catalog = "[`HttpRouter`](crate::app::HttpRouter)";
    assert!(catalog_mentions(catalog, "HttpRouter"));
    assert!(!catalog_mentions(catalog, "ttpRouter"));
    assert!(!catalog_mentions(catalog, "HttpRoute"));
}
