//! `cargo xtask dev-env-probe` implementation.
//!
//! Queries the local dev-env services, compares their reported versions to the
//! windows declared in `schemas/service-compatibility.json`, and exits non-zero
//! when a required service is unreachable or any service reports an
//! out-of-window version.

use anyhow::{Context, Result, bail};
use pacto_bot_api::dev_env_probe::{is_failure, log_warnings, run_probe};

/// Entry point invoked by `cargo xtask dev-env-probe`.
pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    let results = runtime.block_on(run_probe());
    log_warnings(&results);

    let failures: Vec<_> = results.iter().filter(|r| is_failure(r)).collect();
    if failures.is_empty() {
        println!("dev-env-probe: all checked services are within compatibility windows");
        Ok(())
    } else {
        for failure in &failures {
            println!(
                "dev-env-probe: failure on {} at {}: {:?}",
                failure.service, failure.endpoint, failure.status
            );
        }
        bail!(
            "{} dev-env service(s) failed the compatibility probe",
            failures.len()
        );
    }
}
