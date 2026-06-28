//! Helpers for tagging tests with requirement IDs and parsing those tags.
//!
//! Tests can be tagged with requirement IDs using either an attribute-style
//! comment or a doc-comment tag:
//!
//! ```rust,ignore
//! /// req(R1, R2)
//! #[test]
//! fn example() {}
//!
//! #[req(R3)]
//! #[tokio::test]
//! async fn tagged() {}
//! ```

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Extract requirement IDs (e.g. `R1`, `R12`) from Rust source text.
///
/// Recognises two tag forms:
/// - Attribute form: `#[req(R1, R2)]`
/// - Doc-comment form: `/// req(R1, R2)` or `// req(R1, R2)`
///
/// Duplicate IDs are deduplicated; the returned order is insertion order.
pub fn extract_req_ids(source: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim_start();

        let payload = if let Some(attr) = trimmed.strip_prefix("#[req(") {
            attr.strip_suffix(")]")
        } else if let Some(doc) = trimmed.strip_prefix("/// req(") {
            doc.strip_suffix(")")
        } else if let Some(comment) = trimmed.strip_prefix("// req(") {
            comment.strip_suffix(")")
        } else {
            None
        };

        if let Some(payload) = payload {
            for id in payload.split(',') {
                let id = id.trim();
                if id.is_empty() {
                    continue;
                }
                if id.starts_with('R')
                    && id[1..].chars().all(|c| c.is_ascii_digit())
                    && seen.insert(id.to_string())
                {
                    ids.push(id.to_string());
                }
            }
        }
    }

    ids
}

/// Parse requirement tags from a single Rust source file.
#[allow(dead_code)]
pub fn extract_req_ids_from_file(path: &Path) -> std::io::Result<Vec<String>> {
    let source = std::fs::read_to_string(path)?;
    Ok(extract_req_ids(&source))
}

/// Collect requirement tags from all `.rs` files under `tests/`.
///
/// Returns a map from requirement ID to the list of test source files (relative
/// to `root`) that tag it. Paths are sorted for deterministic output.
#[allow(dead_code)]
pub fn collect_test_tags(root: &Path) -> std::io::Result<HashMap<String, Vec<PathBuf>>> {
    let tests_dir = root.join("tests");
    let mut tags: HashMap<String, Vec<PathBuf>> = HashMap::new();

    if !tests_dir.exists() {
        return Ok(tags);
    }

    for entry in std::fs::read_dir(tests_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }

        let ids = extract_req_ids_from_file(&path)?;
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        for id in ids {
            tags.entry(id).or_default().push(rel.clone());
        }
    }

    for paths in tags.values_mut() {
        paths.sort();
        paths.dedup();
    }

    Ok(tags)
}

/// Convenience macro that emits a doc-comment `req(...)` tag for a test.
///
/// ```rust,ignore
/// req_test!("R1", "R2"; my_test, {
///     assert!(true);
/// });
/// ```
#[macro_export]
macro_rules! req_test {
    ($first:literal $(, $rest:literal)+; $name:ident, $body:tt) => {
        #[doc = concat!("req(", $first, $(",", $rest),+, ")")]
        #[test]
        fn $name() $body
    };
    ($req:literal; $name:ident, $body:tt) => {
        #[doc = concat!("req(", $req, ")")]
        #[test]
        fn $name() $body
    };
}

#[cfg(test)]
mod tests {
    use super::extract_req_ids;

    #[test]
    fn extracts_ids_from_doc_comment_tags() {
        let source = r#"
            /// req(R1, R2)
            #[test]
            fn example() {}

            // req(R37)
            fn helper() {}
        "#;
        let ids = extract_req_ids(source);
        assert!(ids.contains(&"R1".to_string()));
        assert!(ids.contains(&"R2".to_string()));
        assert!(ids.contains(&"R37".to_string()));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn extracts_ids_from_attribute_tags() {
        let source = r#"
            #[req(R5)]
            #[tokio::test]
            async fn tagged() {}

            #[req(R10, R11)]
            #[test]
            fn another() {}
        "#;
        let ids = extract_req_ids(source);
        assert!(ids.contains(&"R5".to_string()));
        assert!(ids.contains(&"R10".to_string()));
        assert!(ids.contains(&"R11".to_string()));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn deduplicates_and_skips_invalid_ids() {
        let source = r#"
            /// req(R1, R1, R2, not-a-req, R3)
            #[test]
            fn example() {}
        "#;
        let ids = extract_req_ids(source);
        assert_eq!(ids, vec!["R1", "R2", "R3"]);
    }
}
