//! Ensures generated Rust types stay in sync with canonical schemas.
//!
//! req(R32)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use pacto_bot_api::transport::protocol::Method;
use pacto_bot_api::transport::protocol_generated::JsonRpcCatalogGenerated;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use syn::{Fields, Item};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Files produced by `cargo xtask codegen` from `schemas/`.
const TRACKED_GENERATED_FILES: &[&str] = &[
    "src/config_generated.rs",
    "src/metrics_generated.rs",
    "src/service_compatibility_generated.rs",
    "src/transport/protocol_generated.rs",
];

/// Schema files that are intentionally not represented by a generated Rust
/// module, with a short justification. Every other file in `schemas/` must
/// either be generated or appear here.
const EXEMPT_SCHEMAS: &[(&str, &str)] = &[(
    "version.json",
    "response shape tracked by handwritten AgentVersionResponse in src/transport/protocol.rs",
)];

/// Hand-written Rust response types that must match a canonical schema.
/// Each entry maps `(source_file, schema_file, schema_selector, rust_type)`
/// where `schema_selector` is the dotted path to the object whose properties
/// define the type (empty for the schema root) and `rust_type` is the struct
/// name in `source_file`.
const TRACKED_HANDWRITTEN_TYPES: &[(&str, &str, &[&str], &str)] = &[
    ("src/transport/protocol.rs", "metrics.json", &[], "MetricsResponse"),
    (
        "src/transport/protocol.rs",
        "version.json",
        &[],
        "AgentVersionResponse",
    ),
];

#[test]
fn generated_types_match_schemas() {
    let root = workspace_root();

    // Snapshot the committed contents before codegen overwrites them in-place.
    let committed: Vec<String> = TRACKED_GENERATED_FILES
        .iter()
        .map(|path| {
            let committed_path = root.join(path);
            fs::read_to_string(&committed_path)
                .unwrap_or_else(|e| panic!("failed to read {}: {}", committed_path.display(), e))
        })
        .collect();

    // Run the codegen xtask against the current working tree.
    let status = Command::new("cargo")
        .args(["xtask", "codegen"])
        .current_dir(&root)
        .status()
        .expect("failed to run cargo xtask codegen");
    assert!(status.success(), "cargo xtask codegen failed");

    // Compare freshly generated files to the committed snapshot.
    for (path, committed) in TRACKED_GENERATED_FILES.iter().zip(committed) {
        let generated_path = root.join(path);
        let generated = fs::read_to_string(&generated_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", generated_path.display(), e));

        assert_eq!(
            generated,
            committed,
            "generated file {} does not match committed version; run `cargo xtask codegen`",
            generated_path.display()
        );
    }
}

#[test]
fn every_schema_has_generated_type_or_exemption() {
    let root = workspace_root();
    let schemas_dir = root.join("schemas");

    let schema_files: BTreeSet<String> = fs::read_dir(&schemas_dir)
        .unwrap_or_else(|e| panic!("failed to read schemas dir: {e}"))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            if name.ends_with(".json") {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    let generated: BTreeSet<String> = SCHEMA_GENERATION_MAP
        .iter()
        .map(|mapping| mapping.schema_file.to_string())
        .collect();
    let exempt: BTreeSet<String> = EXEMPT_SCHEMAS
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();

    let uncovered: Vec<String> = schema_files
        .difference(&generated.union(&exempt).cloned().collect())
        .cloned()
        .collect();

    assert!(
        uncovered.is_empty(),
        "schema files without generated types or explicit exemptions: {:?}",
        uncovered
    );
}

#[test]
fn required_schema_fields_are_non_option() {
    let root = workspace_root();
    let generated_structs = parse_generated_structs(&root);

    for mapping in SCHEMA_GENERATION_MAP {
        let schema_path = root.join("schemas").join(mapping.schema_file);
        let schema: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&schema_path)
                .unwrap_or_else(|e| panic!("failed to read {}: {}", schema_path.display(), e)),
        )
        .unwrap_or_else(|e| panic!("failed to parse {}: {}", schema_path.display(), e));

        for struct_mapping in mapping.structs {
            let required = required_fields(&schema, struct_mapping.schema_selector);
            let rust_fields = generated_structs
                .get(struct_mapping.rust_type)
                .unwrap_or_else(|| {
                    panic!(
                        "generated type {} not found for schema {}",
                        struct_mapping.rust_type, mapping.schema_file
                    )
                });

            for field in &required {
                let ty = rust_fields.get(field).unwrap_or_else(|| {
                    panic!(
                        "required schema field {} not found in generated type {}",
                        field, struct_mapping.rust_type
                    )
                });
                assert!(
                    !is_option_type(ty),
                    "required schema field {}::{} must not be Option<T> (found {})",
                    struct_mapping.rust_type,
                    field,
                    type_to_string(ty)
                );
            }
        }
    }
}

#[test]
fn jsonrpc_method_catalog_matches_handwritten_types() {
    let root = workspace_root();
    let schema_path = root.join("schemas/jsonrpc.json");
    let raw = fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", schema_path.display(), e));
    let catalog: JsonRpcCatalogGenerated = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("failed to parse {}: {}", schema_path.display(), e));

    let schema_names: BTreeSet<String> = catalog.methods.iter().map(|m| m.name.clone()).collect();

    let code_names: BTreeSet<String> = Method::all()
        .iter()
        .map(|m| match serde_json::to_value(m) {
            Ok(serde_json::Value::String(s)) => s,
            other => panic!("unexpected serialized Method value: {:?}", other),
        })
        .collect();

    assert_eq!(
        schema_names, code_names,
        "JSON-RPC method catalog in schemas/jsonrpc.json does not match Method enum in src/transport/protocol.rs"
    );

    for method in &catalog.methods {
        assert!(
            !method.params.is_empty()
                || matches!(method.name.as_str(), "agent.metrics" | "agent.version"),
            "{} must declare params (or explicitly declare no params)",
            method.name
        );

        let expects_result = matches!(
            method.name.as_str(),
            "handler.register"
                | "handler.unregister"
                | "agent.send_dm"
                | "agent.set_profile"
                | "agent.metrics"
                | "agent.version"
        );
        if expects_result {
            assert!(
                method.result.is_some(),
                "{} must declare a result schema",
                method.name
            );
        } else {
            assert!(
                method.result.is_none(),
                "{} is a notification and must not declare a result schema",
                method.name
            );
        }
    }
}

#[test]
fn agent_status_params_match_schema() {
    use jsonschema::Validator;
    use pacto_bot_api::transport::protocol::AgentStatusParams;

    let root = workspace_root();
    let schema_path = root.join("schemas/jsonrpc.json");
    let raw = fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", schema_path.display(), e));
    let catalog: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("failed to parse {}: {}", schema_path.display(), e));

    let status_method = catalog["methods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"].as_str() == Some("agent.status"))
        .expect("agent.status method missing from catalog");
    let params_schema = status_method["params"]
        .as_array()
        .and_then(|arr| arr.iter().find(|p| p["name"] == "params"))
        .map(|p| p["schema"].clone())
        .expect("agent.status params schema missing");

    let validator = Validator::new(&params_schema)
        .unwrap_or_else(|e| panic!("failed to compile agent.status params schema: {e}"));

    let full = AgentStatusParams {
        state: "ready".into(),
        identity: Some("npub1example".into()),
        capabilities: vec!["ReadMessages".into(), "SendMessages".into()],
    };
    validator
        .validate(&serde_json::to_value(&full).unwrap())
        .unwrap_or_else(|e| panic!("full agent.status params invalid: {e}"));

    let minimal = AgentStatusParams {
        state: "initializing".into(),
        identity: None,
        capabilities: vec![],
    };
    validator
        .validate(&serde_json::to_value(&minimal).unwrap())
        .unwrap_or_else(|e| panic!("minimal agent.status params invalid: {e}"));

    let invalid_state = AgentStatusParams {
        state: "not_a_state".into(),
        identity: None,
        capabilities: vec![],
    };
    assert!(
        validator
            .validate(&serde_json::to_value(&invalid_state).unwrap())
            .is_err(),
        "invalid state should fail schema validation"
    );
}

#[test]
fn handwritten_response_types_match_schemas() {
    let root = workspace_root();
    let source_structs = parse_source_structs(&root);

    for (source_file, schema_file, selector, rust_type) in TRACKED_HANDWRITTEN_TYPES {
        let schema_path = root.join("schemas").join(schema_file);
        let schema: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&schema_path)
                .unwrap_or_else(|e| panic!("failed to read {}: {}", schema_path.display(), e)),
        )
        .unwrap_or_else(|e| panic!("failed to parse {}: {}", schema_path.display(), e));

        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{} must declare properties", schema_file));
        let required = required_fields(&schema, selector);

        let fields = source_structs.get(*rust_type).unwrap_or_else(|| {
            panic!(
                "hand-written type {} not found in {}",
                rust_type, source_file
            )
        });

        for (field_name, ty) in fields {
            let is_optional = is_option_type(ty);
            let type_string = type_to_string(ty);
            let is_valid_type = if *schema_file == "version.json" {
                type_string == "String"
            } else if field_name == "bots" {
                is_optional && type_string.contains("Vec")
            } else {
                type_string == "u64" || type_string == "Option < u64 >"
            };
            assert!(
                is_valid_type,
                "{}::{} must match the schema type, found {}",
                rust_type, field_name, type_string
            );

            let schema_requires = required.contains(field_name);
            assert_eq!(
                is_optional, !schema_requires,
                "{}::{} optionality must match schema (required={})",
                rust_type, field_name, schema_requires
            );
        }

        for property_name in properties.keys() {
            assert!(
                fields.contains_key(property_name),
                "{} must contain field {} declared in {}",
                rust_type,
                property_name,
                schema_file
            );
        }

        for field_name in fields.keys() {
            assert!(
                properties.contains_key(field_name),
                "{} has field {} which is not declared in {}",
                rust_type,
                field_name,
                schema_file
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Schema -> generated type mapping
// ---------------------------------------------------------------------------

struct SchemaGenerationMapping {
    schema_file: &'static str,
    structs: &'static [StructMapping],
}

struct StructMapping {
    /// Dotted path through the schema to the object whose properties define
    /// the generated struct. Empty means the schema root.
    schema_selector: &'static [&'static str],
    rust_type: &'static str,
}

const SCHEMA_GENERATION_MAP: &[SchemaGenerationMapping] = &[
    SchemaGenerationMapping {
        schema_file: "config.json",
        structs: &[
            StructMapping {
                schema_selector: &["properties", "daemon"],
                rust_type: "DaemonConfigGenerated",
            },
            StructMapping {
                schema_selector: &["properties", "bots", "items"],
                rust_type: "BotConfigGenerated",
            },
        ],
    },
    SchemaGenerationMapping {
        schema_file: "metrics.json",
        structs: &[StructMapping {
            schema_selector: &[],
            rust_type: "MetricsPayloadGenerated",
        }],
    },
    SchemaGenerationMapping {
        schema_file: "service-compatibility.json",
        structs: &[
            StructMapping {
                schema_selector: &["definitions", "versionWindow"],
                rust_type: "VersionWindowGenerated",
            },
            StructMapping {
                schema_selector: &[],
                rust_type: "ServiceCompatibilityGenerated",
            },
        ],
    },
    SchemaGenerationMapping {
        schema_file: "jsonrpc.json",
        structs: &[StructMapping {
            schema_selector: &[],
            rust_type: "JsonRpcCatalogGenerated",
        }],
    },
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn required_fields(schema: &serde_json::Value, selector: &[&str]) -> BTreeSet<String> {
    let mut current = schema;
    for key in selector {
        current = &current[*key];
    }
    current["required"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_generated_structs(
    root: &std::path::Path,
) -> BTreeMap<String, BTreeMap<String, syn::Type>> {
    parse_structs_from_files(root, TRACKED_GENERATED_FILES)
}

fn parse_source_structs(root: &std::path::Path) -> BTreeMap<String, BTreeMap<String, syn::Type>> {
    let files: Vec<&str> = TRACKED_HANDWRITTEN_TYPES
        .iter()
        .map(|(f, _, _, _)| *f)
        .collect();
    parse_structs_from_files(root, &files)
}

fn parse_structs_from_files(
    root: &std::path::Path,
    files: &[&str],
) -> BTreeMap<String, BTreeMap<String, syn::Type>> {
    let mut result = BTreeMap::new();
    for file in files {
        let path = root.join(file);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
        let ast = syn::parse_file(&source)
            .unwrap_or_else(|e| panic!("failed to parse {}: {}", path.display(), e));

        for item in ast.items {
            if let Item::Struct(s) = item {
                let name = s.ident.to_string();
                let mut fields = BTreeMap::new();
                if let Fields::Named(named) = s.fields {
                    for field in named.named {
                        let field_name = field.ident.as_ref().unwrap().to_string();
                        fields.insert(field_name, field.ty);
                    }
                }
                result.insert(name, fields);
            }
        }
    }
    result
}

fn is_option_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            return segment.ident == "Option";
        }
    }
    false
}

fn type_to_string(ty: &syn::Type) -> String {
    quote::quote!(#ty).to_string()
}
