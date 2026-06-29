//! Lightweight JSON Schema -> Rust type generator for pacto-bot-api.
//!
//! This is intentionally simple: it reads the canonical schemas in `schemas/`
//! and emits generated Rust types to `src/config_generated.rs` and
//! `src/transport/protocol_generated.rs`. The generated files are checked into
//! git; `tests/schema_sync.rs` fails when they drift from the schemas.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Entry point invoked by `cargo xtask codegen`.
pub fn run() -> Result<()> {
    let root = find_workspace_root()?;
    let schemas_dir = root.join("schemas");

    generate_config(&schemas_dir, &root)?;
    generate_protocol(&schemas_dir, &root)?;
    generate_metrics(&schemas_dir, &root)?;
    generate_service_compatibility(&schemas_dir, &root)?;
    generate_python(&root)?;

    println!("codegen: generated Rust types from schemas/");
    Ok(())
}

fn find_workspace_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("Cargo.toml").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("could not find workspace root");
        }
    }
}

fn generate_config(schemas_dir: &Path, root: &Path) -> Result<()> {
    let schema: Value = read_schema(schemas_dir, "config.json")?;
    let mut out = String::new();
    out.push_str("//! Generated from schemas/config.json — do not edit manually.\n");
    out.push_str("//! Run `cargo xtask codegen` to regenerate.\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");

    if let Some(props) = schema["properties"].as_object() {
        if let Some(daemon) = props.get("daemon") {
            out.push_str("/// Daemon-wide settings.\n");
            emit_struct(&mut out, "DaemonConfigGenerated", daemon)?;
        }
        if let Some(bots) = props.get("bots")
            && let Some(items) = bots["items"].as_object()
        {
            out.push_str("/// Per-bot identity configuration.\n");
            emit_struct(
                &mut out,
                "BotConfigGenerated",
                &Value::Object(items.clone()),
            )?;
        }
    }

    let out = out.trim_end().to_string() + "\n";
    fs::write(root.join("src/config_generated.rs"), out)?;
    Ok(())
}

fn generate_protocol(schemas_dir: &Path, root: &Path) -> Result<()> {
    let _schema: Value = read_schema(schemas_dir, "jsonrpc.json")?;
    let mut out = String::new();
    out.push_str("//! Generated from schemas/jsonrpc.json — do not edit manually.\n");
    out.push_str("//! Run `cargo xtask codegen` to regenerate.\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n");
    out.push_str("use serde_json::Value;\n\n");

    out.push_str("/// JSON-RPC method catalog entry.\n");
    out.push_str("#[derive(Debug, Clone, Serialize, Deserialize)]\n");
    out.push_str("pub struct JsonRpcMethodGenerated {\n");
    out.push_str("    /// Method name (e.g. `handler.register`).\n");
    out.push_str("    pub name: String,\n");
    out.push_str("    /// Human-readable summary.\n");
    out.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
    out.push_str("    pub summary: Option<String>,\n");
    out.push_str("    /// Parameter schemas (by-name JSON-RPC style).\n");
    out.push_str("    #[serde(default)]\n");
    out.push_str("    pub params: Vec<JsonRpcParamGenerated>,\n");
    out.push_str("    /// Result schema, when the method returns a value.\n");
    out.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
    out.push_str("    pub result: Option<JsonRpcResultGenerated>,\n");
    out.push_str("}\n\n");

    out.push_str("/// Named parameter schema for a JSON-RPC method.\n");
    out.push_str("#[derive(Debug, Clone, Serialize, Deserialize)]\n");
    out.push_str("pub struct JsonRpcParamGenerated {\n");
    out.push_str("    /// Outer key used on the wire (`params` for by-name).\n");
    out.push_str("    pub name: String,\n");
    out.push_str("    /// JSON Schema fragment for the parameter object.\n");
    out.push_str("    pub schema: Value,\n");
    out.push_str("}\n\n");

    out.push_str("/// Result schema for a JSON-RPC method.\n");
    out.push_str("#[derive(Debug, Clone, Serialize, Deserialize)]\n");
    out.push_str("pub struct JsonRpcResultGenerated {\n");
    out.push_str("    /// Descriptive name for the result payload.\n");
    out.push_str("    pub name: String,\n");
    out.push_str("    /// JSON Schema fragment for the result value.\n");
    out.push_str("    pub schema: Value,\n");
    out.push_str("}\n\n");

    out.push_str("/// JSON-RPC catalog container.\n");
    out.push_str("#[derive(Debug, Clone, Serialize, Deserialize)]\n");
    out.push_str("pub struct JsonRpcCatalogGenerated {\n");
    out.push_str("    /// OpenRPC version.\n");
    out.push_str("    pub openrpc: String,\n");
    out.push_str("    /// Catalog metadata.\n");
    out.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
    out.push_str("    pub info: Option<Value>,\n");
    out.push_str("    /// Registered JSON-RPC methods.\n");
    out.push_str("    pub methods: Vec<JsonRpcMethodGenerated>,\n");
    out.push_str("}\n\n");

    let out = out.trim_end().to_string() + "\n";
    fs::write(root.join("src/transport/protocol_generated.rs"), out)?;
    Ok(())
}

fn generate_metrics(schemas_dir: &Path, root: &Path) -> Result<()> {
    let schema: Value = read_schema(schemas_dir, "metrics.json")?;
    let mut out = String::new();
    out.push_str("//! Generated from schemas/metrics.json — do not edit manually.\n");
    out.push_str("//! Run `cargo xtask codegen` to regenerate.\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");

    // Emit any named definitions referenced by $ref first.
    if let Some(defs) = schema["definitions"].as_object() {
        for (def_name, def_schema) in defs {
            let rust_name = format!("{}Generated", to_pascal_case(def_name));
            out.push_str(&format!(
                "/// Generated from `#/definitions/{}`.\n",
                def_name
            ));
            emit_struct(&mut out, &rust_name, def_schema)?;
        }
    }

    out.push_str("/// Metrics payload generated from schemas/metrics.json.\n");
    emit_struct(&mut out, "MetricsPayloadGenerated", &schema)?;

    let out = out.trim_end().to_string() + "\n";
    fs::write(root.join("src/metrics_generated.rs"), out)?;
    Ok(())
}

fn generate_service_compatibility(schemas_dir: &Path, root: &Path) -> Result<()> {
    let schema: Value = read_schema(schemas_dir, "service-compatibility.json")?;
    let mut out = String::new();
    out.push_str("//! Generated from schemas/service-compatibility.json — do not edit manually.\n");
    out.push_str("//! Run `cargo xtask codegen` to regenerate.\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");

    // Emit any named definitions referenced by $ref first.
    if let Some(defs) = schema["definitions"].as_object() {
        for (def_name, def_schema) in defs {
            let rust_name = format!("{}Generated", to_pascal_case(def_name));
            out.push_str(&format!(
                "/// Generated from `#/definitions/{}`.\n",
                def_name
            ));
            emit_struct_with_derives(&mut out, &rust_name, def_schema, &["PartialEq", "Eq"])?;
        }
    }

    out.push_str(
        "/// Service compatibility windows generated from schemas/service-compatibility.json.\n",
    );
    emit_struct_with_derives(
        &mut out,
        "ServiceCompatibilityGenerated",
        &schema,
        &["PartialEq", "Eq"],
    )?;

    let out = out.trim_end().to_string() + "\n";
    fs::write(root.join("src/service_compatibility_generated.rs"), out)?;
    Ok(())
}

fn generate_python(root: &Path) -> Result<()> {
    let script = root.join("python").join("scripts").join("generate.py");
    if !script.exists() {
        bail!("python generator script not found: {}", script.display());
    }

    let python = std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into());
    let status = std::process::Command::new(&python)
        .arg(&script)
        .current_dir(root)
        .status()
        .with_context(|| format!("failed to run python generator {}", script.display()))?;

    if !status.success() {
        bail!(
            "python generator exited with status: {}",
            status.code().unwrap_or(-1)
        );
    }

    Ok(())
}

fn read_schema(schemas_dir: &Path, name: &str) -> Result<Value> {
    let path = schemas_dir.join(name);
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn emit_struct(out: &mut String, name: &str, schema: &Value) -> Result<()> {
    emit_struct_with_derives(out, name, schema, &[])
}

fn emit_struct_with_derives(
    out: &mut String,
    name: &str,
    schema: &Value,
    extra_derives: &[&str],
) -> Result<()> {
    out.push_str("#[derive(Debug, Clone, Default, Serialize, Deserialize");
    for d in extra_derives {
        out.push_str(", ");
        out.push_str(d);
    }
    out.push_str(")]\n");
    out.push_str(&format!("pub struct {} {{\n", name));

    let props = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let required: Vec<String> = schema["required"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    for (key, value) in props.iter() {
        let rust_name = to_snake_case(key);
        let rust_type = schema_type_to_rust(value)?;
        let required = required.contains(key);

        out.push_str(&format!(
            "    /// {}\n",
            value["description"].as_str().unwrap_or(key)
        ));
        if !required {
            out.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
            out.push_str(&format!("    pub {}: Option<{}>,\n", rust_name, rust_type));
        } else {
            out.push_str(&format!("    pub {}: {},\n", rust_name, rust_type));
        }
    }

    out.push_str("}\n\n");
    Ok(())
}

fn schema_type_to_rust(schema: &Value) -> Result<String> {
    // Resolve a local `$ref` to the referenced definition.
    if let Some(reference) = schema["$ref"].as_str() {
        let def_name = reference
            .strip_prefix("#/definitions/")
            .or_else(|| reference.strip_prefix("#/components/schemas/"))
            .with_context(|| format!("unsupported $ref: {reference}"))?;
        return Ok(format!("{}Generated", to_pascal_case(def_name)));
    }

    let typ = schema["type"].as_str().unwrap_or("object");
    match typ {
        "string" => Ok("String".into()),
        "integer" => Ok("u64".into()),
        "boolean" => Ok("bool".into()),
        "array" => {
            let items = &schema["items"];
            let inner = schema_type_to_rust(items)?;
            Ok(format!("Vec<{}>", inner))
        }
        "object" => {
            // Anonymous object: fall back to a JSON Value.
            Ok("serde_json::Value".into())
        }
        _ => Ok("serde_json::Value".into()),
    }
}

fn to_pascal_case(s: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for ch in s.chars() {
        if ch == '_' || ch == '-' {
            upper = true;
        } else if upper {
            out.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
