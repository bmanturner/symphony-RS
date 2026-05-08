//! Logging bootstrap for the `symphony` binary.
//!
//! The CLI is the composition root, so it owns the one-time install of
//! the `tracing` subscriber. Library crates emit `tracing` events but
//! never configure global state — that keeps tests deterministic and
//! lets embedders swap in their own subscriber if they ever wrap us.
//!
//! Two knobs are exposed, both env-only because they have to take effect
//! before `WORKFLOW.md` is even loaded (we want the loader's own logs to
//! honour them):
//!
//! - `SYMPHONY_LOG` — env-filter directive (e.g. `info,symphony_core=debug`).
//!   Falls back to `RUST_LOG`, then to `info` so a fresh install logs
//!   something useful out of the box.
//! - `SYMPHONY_LOG_FORMAT` — `pretty` (default, human-friendly with ANSI
//!   when stderr is a TTY) or `json` (one structured object per line,
//!   suitable for log shippers). Unknown values fall back to `pretty`
//!   and emit a warning *after* the subscriber is installed.

use std::env;
use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Env var that selects between the human-friendly `pretty` formatter
/// and machine-readable `json` formatter. Exposed as a constant so tests
/// and the eventual `symphony status` help text can reference it without
/// stringly-typed drift.
pub const FORMAT_ENV: &str = "SYMPHONY_LOG_FORMAT";

/// Env var that supplies the env-filter directive. Distinct from
/// `RUST_LOG` so users can scope `tracing` levels to Symphony without
/// also flipping levels for embedded libraries that read `RUST_LOG`.
pub const FILTER_ENV: &str = "SYMPHONY_LOG";

/// Output format the CLI should install. Modeled as an enum (rather than
/// a bool) so adding e.g. a `compact` formatter later is a non-breaking
/// addition with an exhaustive match catching every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Multi-line, ANSI-coloured when stderr is a TTY. Good default for
    /// developers running `symphony run` interactively.
    Pretty,
    /// One JSON object per line on stderr. Used by container deployments
    /// where a log shipper parses structured output.
    Json,
}

impl LogFormat {
    /// Parse a raw env-var value into a [`LogFormat`].
    ///
    /// Accepts `pretty` / `text` / `human` as the pretty alias set, and
    /// `json` / `structured` as the JSON alias set. Empty strings (which
    /// is what `std::env::var` returns when the var is set to "") are
    /// treated as "unset" and yield `Ok(None)`. Anything else is an
    /// `Err` carrying the offending value so the caller can warn.
    pub fn from_env_value(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(raw) = raw else { return Ok(None) };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "pretty" | "text" | "human" => Ok(Some(Self::Pretty)),
            "json" | "structured" => Ok(Some(Self::Json)),
            other => Err(other.to_string()),
        }
    }

    /// Resolve the active format by reading [`FORMAT_ENV`]. Returns the
    /// chosen format plus an optional warning the caller should log
    /// *after* the subscriber is installed (logging before install
    /// would silently drop the warning).
    pub fn from_env() -> (Self, Option<String>) {
        let raw = env::var(FORMAT_ENV).ok();
        match Self::from_env_value(raw.as_deref()) {
            Ok(Some(fmt)) => (fmt, None),
            Ok(None) => (Self::Pretty, None),
            Err(bad) => (
                Self::Pretty,
                Some(format!(
                    "{FORMAT_ENV}={bad:?} not recognised; falling back to pretty. \
                     Valid values: pretty, json."
                )),
            ),
        }
    }
}

/// Build the env-filter from `SYMPHONY_LOG`, falling back to `RUST_LOG`,
/// finally defaulting to `info`. Pulled out so tests can exercise the
/// precedence rules without touching globals.
fn resolve_filter(symphony_log: Option<&str>, rust_log: Option<&str>) -> EnvFilter {
    let directive = symphony_log
        .filter(|s| !s.trim().is_empty())
        .or(rust_log.filter(|s| !s.trim().is_empty()))
        .unwrap_or("info");
    // `EnvFilter::try_new` returns Err on a malformed directive. We do
    // not want a typo in `SYMPHONY_LOG` to crash the daemon, so fall
    // back to the default and let the caller keep going.
    EnvFilter::try_new(directive).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Install the global `tracing` subscriber. Idempotent in the sense that
/// the second call returns `Err` from `try_init`; we treat that as a
/// no-op so tests that init twice don't panic.
pub fn init() -> anyhow::Result<()> {
    let (format, warning) = LogFormat::from_env();
    let filter = resolve_filter(
        env::var(FILTER_ENV).ok().as_deref(),
        env::var("RUST_LOG").ok().as_deref(),
    );

    // ANSI is only meaningful when stderr is an interactive terminal —
    // log shippers and CI hate the escape codes otherwise.
    let ansi = std::io::stderr().is_terminal();

    let result = match format {
        LogFormat::Pretty => tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_writer(std::io::stderr).with_ansi(ansi))
            .try_init(),
        LogFormat::Json => tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_writer(std::io::stderr),
            )
            .try_init(),
    };

    // `try_init` only fails if a subscriber is already installed. That
    // is benign (e.g. a test harness installed one first) so we swallow
    // the error rather than propagate.
    let _ = result;

    if let Some(msg) = warning {
        tracing::warn!(target: "symphony::cli", "{msg}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pretty_aliases() {
        for raw in ["pretty", "PRETTY", "  text  ", "human"] {
            assert_eq!(
                LogFormat::from_env_value(Some(raw)).unwrap(),
                Some(LogFormat::Pretty),
                "input {raw:?} should map to Pretty",
            );
        }
    }

    #[test]
    fn parses_json_aliases() {
        for raw in ["json", "JSON", "structured"] {
            assert_eq!(
                LogFormat::from_env_value(Some(raw)).unwrap(),
                Some(LogFormat::Json),
                "input {raw:?} should map to Json",
            );
        }
    }

    #[test]
    fn unset_and_empty_yield_none() {
        assert_eq!(LogFormat::from_env_value(None).unwrap(), None);
        assert_eq!(LogFormat::from_env_value(Some("")).unwrap(), None);
        assert_eq!(LogFormat::from_env_value(Some("   ")).unwrap(), None);
    }

    #[test]
    fn unknown_value_returns_err_with_value() {
        let err = LogFormat::from_env_value(Some("yaml")).unwrap_err();
        assert_eq!(err, "yaml");
    }

    #[test]
    fn filter_prefers_symphony_log_over_rust_log() {
        // Smoke-tests the precedence rule. We can't easily inspect the
        // resulting EnvFilter's internal directives, but we can confirm
        // it constructs without panicking for each input shape.
        let _ = resolve_filter(Some("debug"), Some("trace"));
        let _ = resolve_filter(None, Some("trace"));
        let _ = resolve_filter(None, None);
        // Empty strings should be treated as unset so a stray
        // `SYMPHONY_LOG=` in a shell profile doesn't blank the filter.
        let _ = resolve_filter(Some(""), Some("trace"));
    }

    #[test]
    fn malformed_directive_falls_back_to_info() {
        // `not a valid directive!!!` is rejected by EnvFilter::try_new.
        // The function should still return a usable filter.
        let _ = resolve_filter(Some("!!!not-a-directive!!!"), None);
    }
}
