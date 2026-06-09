//! `tracing` subscriber initialization.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialize global logging.
///
/// Respects the `RUST_LOG` env var; falls back to `default_directive`
/// (e.g. `"info"`) when it is unset. Safe to call once at process start.
pub fn init(default_directive: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_directive));

    tracing_subscriber::registry()
        .with(fmt::layer().with_target(true))
        .with(filter)
        .init();
}
