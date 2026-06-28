//! Source lint: sensitive values must not be stored in plain `String`/`&str` fields.
//!
//! Walks the production `src/` tree and rejects struct/enum fields, type
//! aliases, and const/static items named `nsec`, `uri`, `bunker_uri`, `token`,
//! or `raw_secret_string` whose type is (or contains) a plain `String` or
//! `&str`.  `secrecy::SecretString` and `zeroize::Zeroizing` are treated as
//! safe containers.

use anyhow::{Context, Result, bail};
use quote::ToTokens;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{Field, Type};

const SECRET_NAMES: &[&str] = &["nsec", "uri", "bunker_uri", "token", "raw_secret_string"];

/// Entry point invoked by `cargo xtask secret-lint`.
pub fn run() -> Result<()> {
    let root = find_workspace_root()?;
    let src = root.join("src");
    let violations = lint_dir(&src)?;

    if violations.is_empty() {
        println!("secret-lint: no plain string secret fields found");
        Ok(())
    } else {
        for v in &violations {
            eprintln!("secret-lint: {v}");
        }
        bail!("secret-lint: {} violation(s)", violations.len())
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

        let mut visitor = SecretVisitor {
            path: path.to_path_buf(),
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

struct SecretVisitor {
    path: PathBuf,
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for SecretVisitor {
    fn visit_field(&mut self, field: &Field) {
        // Command-line argument definitions are transient input plumbing, not
        // secret storage, and are allowed to use `String`/`Option<String>`.
        if field.attrs.iter().any(|attr| attr.path().is_ident("arg")) {
            return;
        }

        let Some(ident) = &field.ident else {
            syn::visit::visit_field(self, field);
            return;
        };

        if is_secret_name(ident.to_string().as_str()) && is_plain_string(&field.ty) {
            let line = ident.span().start().line;
            self.violations.push(format!(
                "{}:{}: field `{}` stores a secret as `{}`",
                self.path.display(),
                line,
                ident,
                type_string(&field.ty)
            ));
        }
        syn::visit::visit_field(self, field);
    }

    fn visit_item_type(&mut self, item: &syn::ItemType) {
        if is_secret_name(item.ident.to_string().as_str()) && is_plain_string(&item.ty) {
            let line = item.ident.span().start().line;
            self.violations.push(format!(
                "{}:{}: type alias `{}` stores a secret as `{}`",
                self.path.display(),
                line,
                item.ident,
                type_string(&item.ty)
            ));
        }
        syn::visit::visit_item_type(self, item);
    }

    fn visit_item_const(&mut self, item: &syn::ItemConst) {
        if is_secret_name(item.ident.to_string().as_str()) && is_plain_string(&item.ty) {
            let line = item.ident.span().start().line;
            self.violations.push(format!(
                "{}:{}: const `{}` stores a secret as `{}`",
                self.path.display(),
                line,
                item.ident,
                type_string(&item.ty)
            ));
        }
        syn::visit::visit_item_const(self, item);
    }

    fn visit_item_static(&mut self, item: &syn::ItemStatic) {
        if is_secret_name(item.ident.to_string().as_str()) && is_plain_string(&item.ty) {
            let line = item.ident.span().start().line;
            self.violations.push(format!(
                "{}:{}: static `{}` stores a secret as `{}`",
                self.path.display(),
                line,
                item.ident,
                type_string(&item.ty)
            ));
        }
        syn::visit::visit_item_static(self, item);
    }
}

fn is_secret_name(name: &str) -> bool {
    SECRET_NAMES.contains(&name)
}

/// Returns `true` if `ty` is (or wraps) a plain `String` or `&str`.
///
/// `SecretString` and `Zeroizing<...>` are treated as safe, so they short-
/// circuit recursion even when they wrap a `String`.
fn is_plain_string(ty: &Type) -> bool {
    match ty {
        Type::Path(type_path) => {
            let Some(seg) = type_path.path.segments.last() else {
                return false;
            };
            let name = seg.ident.to_string();

            // Safe wrappers.
            if name == "SecretString" || name == "Zeroizing" {
                return false;
            }

            if name == "String" || name == "str" {
                return true;
            }

            if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                return args.args.iter().any(|arg| match arg {
                    syn::GenericArgument::Type(inner) => is_plain_string(inner),
                    _ => false,
                });
            }

            false
        }
        Type::Reference(r) => is_plain_string(&r.elem),
        Type::Paren(p) => is_plain_string(&p.elem),
        Type::Group(g) => is_plain_string(&g.elem),
        Type::Tuple(t) => t.elems.iter().any(is_plain_string),
        Type::Array(a) => is_plain_string(&a.elem),
        Type::Slice(s) => is_plain_string(&s.elem),
        _ => false,
    }
}

fn type_string(ty: &Type) -> String {
    ty.to_token_stream().to_string()
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

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "pacto-secret-lint-{}",
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
    fn raw_secret_string_field_is_rejected() {
        let dir = temp_dir();
        let _ = fs::remove_dir_all(&dir);
        write_file(&dir, "bad.rs", "struct Bad { raw_secret_string: String }\n");
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected one violation, got: {violations:?}"
        );
        assert!(violations[0].contains("raw_secret_string"));
    }

    #[test]
    fn nsec_secret_string_is_allowed() {
        let dir = temp_dir();
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "good.rs",
            "struct Good { nsec: secrecy::SecretString }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            violations.is_empty(),
            "expected no violations, got: {violations:?}"
        );
    }

    #[test]
    fn wrapped_secret_string_is_allowed() {
        let dir = temp_dir();
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "good.rs",
            "type HttpToken = std::sync::Arc<tokio::sync::RwLock<secrecy::SecretString>>;\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            violations.is_empty(),
            "expected no violations, got: {violations:?}"
        );
    }

    #[test]
    fn option_string_secret_is_rejected() {
        let dir = temp_dir();
        let _ = fs::remove_dir_all(&dir);
        write_file(
            &dir,
            "bad.rs",
            "enum SigningConfig { Nsec { nsec: Option<String> } }\n",
        );
        let violations = lint_dir(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            violations.len(),
            1,
            "expected one violation, got: {violations:?}"
        );
        assert!(violations[0].contains("nsec"));
    }

    #[test]
    fn production_source_passes() {
        let root = find_workspace_root().unwrap();
        let violations = lint_dir(&root.join("src")).unwrap();
        assert!(
            violations.is_empty(),
            "violations in production code: {violations:?}"
        );
    }
}
