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
/// Recognises the following tag forms on a single line:
/// - Attribute form: `#[req(R1, R2)]`
/// - Outer doc-comment form: `/// req(R1, R2)`
/// - Inner doc-comment form: `//! req(R1, R2)` (crate/module level)
/// - Plain comment form: `// req(R1, R2)`
/// - `req_test!("R1", "R2"; ...)` macro invocation
///
/// Duplicate IDs are deduplicated; the returned order is insertion order.
pub fn extract_req_ids(source: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim_start();

        let payload = if let Some(attr) = trimmed.strip_prefix("#[req(") {
            attr.strip_suffix(")]")
        } else if let Some(outer_doc) = trimmed.strip_prefix("/// req(") {
            outer_doc.strip_suffix(")")
        } else if let Some(inner_doc) = trimmed.strip_prefix("//! req(") {
            inner_doc.strip_suffix(")")
        } else if let Some(comment) = trimmed.strip_prefix("// req(") {
            comment.strip_suffix(")")
        } else {
            None
        };

        if let Some(payload) = payload {
            for id in payload.split(',') {
                let id = id.trim();
                if id.starts_with('R')
                    && id[1..].chars().all(|c| c.is_ascii_digit())
                    && seen.insert(id.to_string())
                {
                    ids.push(id.to_string());
                }
            }
        }

        // The `req_test!` macro emits a `#[doc = concat!("req(...)")]`
        // attribute, which is not itself a doc-comment tag. Parse the quoted
        // requirement IDs directly from the macro invocation.
        if let Some(rest) = trimmed
            .strip_prefix("req_test!(")
            .and_then(|r| r.split(';').next())
        {
            for part in rest.split(',') {
                let part = part.trim();
                if let Some(id) = part.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                    && id.starts_with('R')
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

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
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

    let mut files = Vec::new();
    collect_rs_files(&tests_dir, &mut files)?;

    for path in files {
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
/// Supports both synchronous `#[test]` and asynchronous `#[tokio::test]`
/// functions.
///
/// ```rust,ignore
/// req_test!("R1", "R2"; my_test, {
///     let _ = std::hint::black_box(42);
/// });
///
/// req_test!("R3"; async my_async_test, {
///     let _ = std::hint::black_box(42);
/// });
/// ```
#[macro_export]
macro_rules! req_test {
    ($($req:literal),+; async $name:ident, $body:tt) => {
        #[doc = concat!("req(", $($req),+, ")")]
        #[tokio::test]
        async fn $name() $body
    };
    ($($req:literal),+; $name:ident, $body:tt) => {
        #[doc = concat!("req(", $($req),+, ")")]
        #[test]
        fn $name() $body
    };
}

#[cfg(test)]
mod tests {
    use super::{collect_test_tags, extract_req_ids};
    use std::path::PathBuf;

    req_test!("R99"; sync_macro_example, {
        let _ = std::hint::black_box(42);
    });

    req_test!("R98", "R97"; async async_macro_example, {
        let _ = std::hint::black_box(42);
    });

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
    fn extracts_ids_from_inner_doc_comments() {
        let source = r#"
            //! req(R32)
            //! req(R33, R34)
        "#;
        let ids = extract_req_ids(source);
        assert_eq!(ids, vec!["R32", "R33", "R34"]);
    }

    #[test]
    fn extracts_ids_from_req_test_macro_invocations() {
        let source = std::fs::read_to_string(file!()).unwrap();
        let ids = extract_req_ids(&source);
        assert!(ids.contains(&"R99".to_string()));
        assert!(ids.contains(&"R98".to_string()));
        assert!(ids.contains(&"R97".to_string()));
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

    #[test]
    fn collect_test_tags_walks_subdirectories() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let tags = collect_test_tags(&root).expect("collect_test_tags should succeed");

        // These files live directly under tests/.
        assert!(
            tags.get("R1")
                .expect("R1 should be tagged")
                .contains(&PathBuf::from("tests/transport_unix.rs"))
        );

        // Schema sync lives under tests/ and is tagged via an inner doc comment.
        assert!(
            tags.get("R32")
                .expect("R32 should be tagged")
                .contains(&PathBuf::from("tests/schema_sync.rs"))
        );

        // The support module itself is parsed too; req_attr.rs contains examples.
        assert!(
            tags.get("R3")
                .expect("R3 should be tagged")
                .contains(&PathBuf::from("tests/support/req_attr.rs"))
        );
    }
}
