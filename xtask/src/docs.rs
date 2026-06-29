//! Generate the LLM-readable operator's guide committed at
//! `docs/pacto-bot-admin-llms.txt`.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Entry point invoked by `cargo xtask docs`.
pub fn run() -> Result<()> {
    let guide = pacto_bot_api::guide::render_llm_guide();
    let root = find_workspace_root()?;
    let out_path = root.join("docs").join("pacto-bot-admin-llms.txt");

    fs::write(&out_path, guide)
        .with_context(|| format!("failed to write {}", out_path.display()))?;

    println!("docs: generated {}", out_path.display());
    Ok(())
}

fn find_workspace_root() -> Result<std::path::PathBuf> {
    let manifest_dir = std::env!("CARGO_MANIFEST_DIR");
    let root = Path::new(manifest_dir)
        .parent()
        .context("xtask manifest has no parent directory")?;
    Ok(root.to_path_buf())
}
