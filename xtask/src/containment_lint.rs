//! Source lint: nostr 0.45-removed symbols must resolve only inside their
//! declared seam module.
//!
//! `nostr` 0.45 removes `NostrSigner`, `TagKind`, `TagStandard`, `TagKind`'s
//! sibling `JsonUtil` trait (and with it `as_json`/`from_json`),
//! `sign_with_keys`, `EventBuilder::pow`, and `EventBuilder::reaction_extended`
//! from its public API. `src/nostr_tags.rs`, `src/nostr_json.rs`, and
//! `src/signer.rs` each centralize one of these symbol families behind a seam
//! so the eventual removal touches a single file. This lint walks `src/` and
//! `tests/`, skips `_generated.rs`, parses with `syn`, and asserts that a
//! banned symbol resolves only inside its declared owner file — everywhere
//! else is a violation, seam erosion the eventual 0.45 upgrade would have to
//! pay down all over again.
//!
//! `TagStandard` and `EventBuilder::pow` have no owner: neither is used
//! anywhere in the tree today, and this lint keeps it that way rather than
//! naming a seam nothing calls into yet.
//!
//! `Timestamp::as_u64` is deliberately **not** on this list. `syn` parses
//! without type resolution, so `value.as_u64()` on a `serde_json::Value` and
//! `timestamp.as_u64()` on a nostr `Timestamp` are indistinguishable by name
//! alone — unlike the symbols here, which are specific enough that the
//! resulting false-positive rate is acceptable for a CI gate.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{Ident, ItemEnum, ItemStruct, ItemType, Path as SynPath, UseName, UseRename};

/// A banned symbol family and where it is allowed to resolve.
struct BannedSymbol {
    /// The identifier this lint looks for.
    name: &'static str,
    /// The seam file's basename that is exempt from this check, or `None`
    /// when the symbol must not appear anywhere in the tree.
    owner_basename: Option<&'static str>,
    /// How the symbol is matched against the AST.
    kind: SymbolKind,
}

enum SymbolKind {
    /// Matches when any segment of a `Path` (type position, expression
    /// position, or a `use` leaf) has this identifier.
    AnySegment,
    /// Matches when the *last* segment of a `Path` has this identifier, or
    /// when a method call's name has this identifier — covers both
    /// `Type::symbol(..)` associated-function and `value.symbol(..)` method
    /// call forms.
    LastSegmentOrMethod,
    /// Matches only the exact qualified path `left::right`, to avoid a
    /// blanket match on an unrelated identically-named method (`pow` is a
    /// common name on integer types).
    QualifiedPair { left: &'static str },
}

const BANNED: &[BannedSymbol] = &[
    BannedSymbol {
        name: "NostrSigner",
        owner_basename: Some("signer.rs"),
        kind: SymbolKind::AnySegment,
    },
    BannedSymbol {
        name: "TagKind",
        owner_basename: Some("nostr_tags.rs"),
        kind: SymbolKind::AnySegment,
    },
    BannedSymbol {
        name: "TagStandard",
        owner_basename: None,
        kind: SymbolKind::AnySegment,
    },
    BannedSymbol {
        name: "JsonUtil",
        owner_basename: Some("nostr_json.rs"),
        kind: SymbolKind::AnySegment,
    },
    BannedSymbol {
        name: "as_json",
        owner_basename: Some("nostr_json.rs"),
        kind: SymbolKind::LastSegmentOrMethod,
    },
    BannedSymbol {
        name: "from_json",
        owner_basename: Some("nostr_json.rs"),
        kind: SymbolKind::LastSegmentOrMethod,
    },
    BannedSymbol {
        name: "sign_with_keys",
        owner_basename: Some("nostr_json.rs"),
        kind: SymbolKind::LastSegmentOrMethod,
    },
    BannedSymbol {
        name: "reaction_extended",
        owner_basename: Some("nostr_tags.rs"),
        kind: SymbolKind::LastSegmentOrMethod,
    },
    BannedSymbol {
        name: "pow",
        owner_basename: None,
        kind: SymbolKind::QualifiedPair {
            left: "EventBuilder",
        },
    },
];

/// Entry point invoked by `cargo xtask containment-lint`.
pub fn run() -> Result<()> {
    let root = find_workspace_root()?;
    let mut violations = Vec::new();
    for sub in ["src", "tests"] {
        violations.extend(lint_dir(&root.join(sub))?);
    }

    if violations.is_empty() {
        println!("containment-lint: no seam violations found");
        Ok(())
    } else {
        for v in &violations {
            eprintln!("containment-lint: {v}");
        }
        bail!("containment-lint: {} violation(s)", violations.len())
    }
}

/// Lint every `.rs` file under `dir`, skipping generated files.
pub fn lint_dir(dir: &Path) -> Result<Vec<String>> {
    let mut violations = Vec::new();
    visit_rust_files(dir, &mut |path: &Path| {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let file = syn::parse_file(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        let basename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        // A file that declares its own type named the same as a banned
        // symbol (e.g. `src/scaffold/template.rs`'s local `TagKind` enum) is
        // exempt for that symbol only — it is not the nostr type.
        let mut locally_declared = LocalDeclVisitor::default();
        locally_declared.visit_file(&file);

        let mut visitor = ContainmentVisitor {
            path: path.to_path_buf(),
            basename,
            locally_declared: locally_declared.names,
            violations: Vec::new(),
        };
        visitor.visit_file(&file);
        violations.extend(visitor.violations);
        Ok(())
    })?;
    Ok(violations)
}

fn visit_rust_files(dir: &Path, cb: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;

        if meta.is_dir() {
            visit_rust_files(&path, cb)?;
        } else if meta.is_file() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".rs") && !name.ends_with("_generated.rs") {
                cb(&path)?;
            }
        }
    }
    Ok(())
}

/// Collects top-level type names declared in a file (enum/struct/type
/// alias idents), so a locally-declared lookalike does not trip the lint.
#[derive(Default)]
struct LocalDeclVisitor {
    names: Vec<String>,
}

impl<'ast> Visit<'ast> for LocalDeclVisitor {
    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        self.names.push(item.ident.to_string());
        syn::visit::visit_item_enum(self, item);
    }
    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        self.names.push(item.ident.to_string());
        syn::visit::visit_item_struct(self, item);
    }
    fn visit_item_type(&mut self, item: &'ast ItemType) {
        self.names.push(item.ident.to_string());
        syn::visit::visit_item_type(self, item);
    }
}

struct ContainmentVisitor {
    path: PathBuf,
    basename: String,
    locally_declared: Vec<String>,
    violations: Vec<String>,
}

impl ContainmentVisitor {
    fn is_owner(&self, symbol: &BannedSymbol) -> bool {
        symbol.owner_basename == Some(self.basename.as_str())
    }

    fn is_locally_shadowed(&self, name: &str) -> bool {
        self.locally_declared.iter().any(|n| n == name)
    }

    fn record(&mut self, symbol: &BannedSymbol, line: usize) {
        if self.is_owner(symbol) || self.is_locally_shadowed(symbol.name) {
            return;
        }
        self.violations.push(format!(
            "{}:{}: `{}` resolves outside its seam module{}",
            self.path.display(),
            line,
            symbol.name,
            symbol
                .owner_basename
                .map(|o| format!(" (`{o}`)"))
                .unwrap_or_else(|| " (must not be used anywhere)".to_string()),
        ));
    }

    fn check_ident(&mut self, ident: &Ident, last_only: bool) {
        let name = ident.to_string();
        let line = ident.span().start().line;
        for symbol in BANNED {
            if symbol.name != name {
                continue;
            }
            match symbol.kind {
                SymbolKind::AnySegment => self.record(symbol, line),
                SymbolKind::LastSegmentOrMethod if last_only => self.record(symbol, line),
                SymbolKind::LastSegmentOrMethod => {}
                SymbolKind::QualifiedPair { .. } => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for ContainmentVisitor {
    fn visit_path(&mut self, path: &'ast SynPath) {
        let segments: Vec<&Ident> = path.segments.iter().map(|s| &s.ident).collect();
        for (i, ident) in segments.iter().enumerate() {
            let is_last = i + 1 == segments.len();
            self.check_ident(ident, is_last);
        }
        // Qualified-pair symbols (currently only `EventBuilder::pow`) match
        // an exact adjacent pair rather than a bare identifier, so a
        // coincidentally-named method elsewhere (`u64::pow`) is left alone.
        for symbol in BANNED {
            if let SymbolKind::QualifiedPair { left } = symbol.kind {
                for window in segments.windows(2) {
                    if *window[0] == *left && *window[1] == *symbol.name {
                        self.record(symbol, window[1].span().start().line);
                    }
                }
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.check_ident(&call.method, true);
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_use_name(&mut self, name: &'ast UseName) {
        self.check_ident(&name.ident, true);
        syn::visit::visit_use_name(self, name);
    }

    fn visit_use_rename(&mut self, rename: &'ast UseRename) {
        // `rename.ident` is the symbol as published; `rename.rename` is the
        // local alias. The published name is what matters here.
        self.check_ident(&rename.ident, true);
        syn::visit::visit_use_rename(self, rename);
    }
}

fn find_workspace_root() -> Result<PathBuf> {
    let start = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("current dir available"));
    let mut dir = start;
    // `xtask` lives one level below the workspace root.
    if dir.file_name() == Some(std::ffi::OsStr::new("xtask")) {
        dir.pop();
        return Ok(dir);
    }
    // Otherwise search upward for `Cargo.toml` containing `[workspace]`.
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            let content = fs::read_to_string(&manifest)
                .with_context(|| format!("failed to read {}", manifest.display()))?;
            if content.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            bail!("could not find workspace root");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pacto-containment-lint-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn tree_after_u1_and_u2_passes() {
        let root = find_workspace_root().unwrap();
        let mut violations = lint_dir(&root.join("src")).unwrap();
        violations.extend(lint_dir(&root.join("tests")).unwrap());
        assert!(
            violations.is_empty(),
            "violations in production/test code: {violations:?}"
        );
    }

    #[test]
    fn tag_kind_outside_seam_is_rejected() {
        let dir = temp_dir("tagkind");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "leaky.rs",
            "fn build() { let _ = nostr::TagKind::custom(\"h\"); }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected one violation: {violations:?}"
        );
        assert!(violations[0].contains("TagKind"));
    }

    #[test]
    fn tag_kind_inside_owner_file_is_allowed() {
        let dir = temp_dir("tagkind-owner");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "nostr_tags.rs",
            "fn build() { let _ = nostr::TagKind::custom(\"h\"); }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            violations.is_empty(),
            "expected no violations: {violations:?}"
        );
    }

    #[test]
    fn locally_declared_tag_kind_enum_does_not_trip_the_assertion() {
        let dir = temp_dir("tagkind-local");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "template.rs",
            "enum TagKind { Var, Block }\nfn f() { let _ = TagKind::Var; }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            violations.is_empty(),
            "expected no violations: {violations:?}"
        );
    }

    #[test]
    fn json_util_and_as_json_and_from_json_and_sign_with_keys_outside_seam_are_rejected() {
        let dir = temp_dir("jsonutil");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "leaky.rs",
            "use nostr::JsonUtil;\nfn f(e: &nostr::Event, k: &nostr::Keys, u: nostr::UnsignedEvent) {\n    let _ = e.as_json();\n    let _ = nostr::Event::from_json(\"{}\".to_string());\n    let _ = u.sign_with_keys(k);\n}\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            4,
            "expected JsonUtil + as_json + from_json + sign_with_keys violations: {violations:?}"
        );
    }

    #[test]
    fn nostr_signer_outside_seam_is_rejected() {
        let dir = temp_dir("nostrsigner");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "leaky.rs",
            "fn f(s: &dyn nostr::NostrSigner) { let _ = s; }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected one violation: {violations:?}"
        );
        assert!(violations[0].contains("NostrSigner"));
    }

    #[test]
    fn tag_standard_is_always_rejected_even_inside_a_seam_named_file() {
        let dir = temp_dir("tagstandard");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "nostr_tags.rs",
            "fn f() { let _: Option<nostr::TagStandard> = None; }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected one violation: {violations:?}"
        );
        assert!(violations[0].contains("TagStandard"));
    }

    #[test]
    fn reaction_extended_outside_seam_is_rejected() {
        let dir = temp_dir("reaction");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "leaky.rs",
            "fn f() { let _ = nostr::EventBuilder::reaction_extended(id, pk, None, \"+\"); }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected one violation: {violations:?}"
        );
        assert!(violations[0].contains("reaction_extended"));
    }

    #[test]
    fn event_builder_pow_is_rejected_but_integer_pow_is_not() {
        let dir = temp_dir("pow");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "leaky.rs",
            "fn f(b: nostr::EventBuilder) -> nostr::EventBuilder {\n    let _ = 2u64.pow(3);\n    nostr::EventBuilder::pow(b, 20)\n}\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected only the EventBuilder::pow violation, not integer .pow(): {violations:?}"
        );
        assert!(violations[0].contains("pow"));
    }

    #[test]
    fn violation_inside_cfg_test_mod_still_trips() {
        let dir = temp_dir("cfgtest");
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "leaky.rs",
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        let _ = nostr::TagKind::custom(\"h\");\n    }\n}\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected one violation: {violations:?}"
        );
    }

    #[test]
    fn violation_under_a_tests_style_directory_trips_the_same_way_as_src() {
        // `lint_dir` is directory-agnostic; the top-level `run()` walks both
        // `src/` and `tests/` with it, so a nested arbitrary directory name
        // exercises the same code path a `tests/` fixture would.
        let dir = temp_dir("tests-dir").join("tests");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        write_file(
            &dir,
            "some_test.rs",
            "fn f() { let _ = nostr::TagKind::custom(\"h\"); }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        assert_eq!(
            violations.len(),
            1,
            "expected one violation: {violations:?}"
        );
    }
}
