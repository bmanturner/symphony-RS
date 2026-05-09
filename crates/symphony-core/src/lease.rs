//! Durable lease identity (SPEC v2 §7.2 / ARCHITECTURE_v2 §7.2).
//!
//! A running work item is gated by a durable lease in `runs.lease_owner`.
//! Today that column holds a free-form `String`; this module gives the
//! kernel a typed identity so the scheduler, runners, and recovery loops
//! all agree on what a lease holder *is* (host/process + scheduler
//! instance + worker slot) rather than each constructing ad-hoc strings.
//!
//! The canonical wire format is `"<host>/<scheduler_instance>/worker-<slot>"`,
//! e.g. `"hostname:1234/scheduler-1/worker-0"`. [`LeaseOwner`]'s
//! [`std::fmt::Display`] impl and [`LeaseOwner::from_str`] round-trip
//! through this format so the
//! string written to SQLite and the typed identity used by runners are
//! interchangeable. None of the three components may be empty or contain
//! the `/` separator; the parser rejects malformed inputs with
//! [`LeaseOwnerParseError`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Typed identity for a durable lease holder.
///
/// Composed of three components per ARCHITECTURE_v2 §7.2:
///
/// * `host` — process/host identifier (e.g. `"hostname:1234"`). The
///   kernel does not interpret this string; it only requires that two
///   distinct processes produce distinct values.
/// * `scheduler_instance` — operator-chosen scheduler tag (e.g.
///   `"scheduler-1"`). Lets a single host run multiple scheduler
///   instances without lease collisions.
/// * `worker_slot` — bounded worker index inside the scheduler. Pairs
///   with `agent.max_concurrent_agents` so each in-flight runner
///   advertises a stable slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct LeaseOwner {
    host: String,
    scheduler_instance: String,
    worker_slot: u32,
}

impl LeaseOwner {
    /// Construct a [`LeaseOwner`] from typed components.
    ///
    /// Returns [`LeaseOwnerParseError::EmptyComponent`] if `host` or
    /// `scheduler_instance` is empty, and
    /// [`LeaseOwnerParseError::ReservedSeparator`] if either contains
    /// the `/` separator used by the canonical wire format.
    pub fn new(
        host: impl Into<String>,
        scheduler_instance: impl Into<String>,
        worker_slot: u32,
    ) -> Result<Self, LeaseOwnerParseError> {
        let host = host.into();
        let scheduler_instance = scheduler_instance.into();
        validate_component("host", &host)?;
        validate_component("scheduler_instance", &scheduler_instance)?;
        Ok(Self {
            host,
            scheduler_instance,
            worker_slot,
        })
    }

    /// Resolve the current process's host string as
    /// `"<hostname>:<pid>"`, falling back to `"localhost:<pid>"` when
    /// the `HOSTNAME` environment variable is unavailable.
    ///
    /// Kept as a thin convenience so the runners do not each reinvent
    /// host detection. Tests should call [`LeaseOwner::new`] with a
    /// fixed host string for determinism.
    pub fn current_process(
        scheduler_instance: impl Into<String>,
        worker_slot: u32,
    ) -> Result<Self, LeaseOwnerParseError> {
        let host_name = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
        let pid = std::process::id();
        Self::new(
            format!("{host_name}:{pid}"),
            scheduler_instance,
            worker_slot,
        )
    }

    /// Borrow the host component.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Borrow the scheduler-instance component.
    pub fn scheduler_instance(&self) -> &str {
        &self.scheduler_instance
    }

    /// The worker-slot index.
    pub fn worker_slot(&self) -> u32 {
        self.worker_slot
    }

    /// Canonical wire form, identical to [`fmt::Display`].
    pub fn as_canonical(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for LeaseOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/worker-{}",
            self.host, self.scheduler_instance, self.worker_slot
        )
    }
}

impl From<LeaseOwner> for String {
    fn from(owner: LeaseOwner) -> Self {
        owner.to_string()
    }
}

impl TryFrom<String> for LeaseOwner {
    type Error = LeaseOwnerParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl FromStr for LeaseOwner {
    type Err = LeaseOwnerParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('/');
        let host = parts.next().ok_or(LeaseOwnerParseError::MissingComponent)?;
        let scheduler_instance = parts.next().ok_or(LeaseOwnerParseError::MissingComponent)?;
        let slot_part = parts.next().ok_or(LeaseOwnerParseError::MissingComponent)?;
        if parts.next().is_some() {
            return Err(LeaseOwnerParseError::TrailingComponents);
        }
        validate_component("host", host)?;
        validate_component("scheduler_instance", scheduler_instance)?;
        let slot_digits = slot_part
            .strip_prefix("worker-")
            .ok_or(LeaseOwnerParseError::MissingWorkerPrefix)?;
        let worker_slot = slot_digits
            .parse::<u32>()
            .map_err(|_| LeaseOwnerParseError::InvalidWorkerSlot)?;
        Ok(Self {
            host: host.to_string(),
            scheduler_instance: scheduler_instance.to_string(),
            worker_slot,
        })
    }
}

fn validate_component(field: &'static str, value: &str) -> Result<(), LeaseOwnerParseError> {
    if value.is_empty() {
        return Err(LeaseOwnerParseError::EmptyComponent { field });
    }
    if value.contains('/') {
        return Err(LeaseOwnerParseError::ReservedSeparator { field });
    }
    Ok(())
}

/// Errors produced when constructing or parsing a [`LeaseOwner`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LeaseOwnerParseError {
    /// The canonical form requires exactly three `/`-separated parts.
    #[error("lease owner must have host/scheduler_instance/worker-N components")]
    MissingComponent,
    /// More than three `/`-separated parts were supplied.
    #[error("lease owner contains trailing components after worker slot")]
    TrailingComponents,
    /// Either `host` or `scheduler_instance` was empty.
    #[error("lease owner component `{field}` must not be empty")]
    EmptyComponent {
        /// Which component triggered the error.
        field: &'static str,
    },
    /// A component contained the `/` separator used by the wire format.
    #[error("lease owner component `{field}` must not contain `/`")]
    ReservedSeparator {
        /// Which component triggered the error.
        field: &'static str,
    },
    /// The third component did not begin with `worker-`.
    #[error("lease owner worker slot must be of the form `worker-<u32>`")]
    MissingWorkerPrefix,
    /// The digits after `worker-` did not parse as `u32`.
    #[error("lease owner worker slot must be a `u32` integer")]
    InvalidWorkerSlot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_uses_canonical_format() {
        let owner = LeaseOwner::new("hostname:1234", "scheduler-1", 0).unwrap();
        assert_eq!(owner.to_string(), "hostname:1234/scheduler-1/worker-0");
        assert_eq!(owner.as_canonical(), "hostname:1234/scheduler-1/worker-0");
    }

    #[test]
    fn round_trip_through_string() {
        let original = LeaseOwner::new("host-a", "scheduler-2", 7).unwrap();
        let wire: String = original.clone().into();
        let parsed: LeaseOwner = wire.parse().unwrap();
        assert_eq!(original, parsed);
        assert_eq!(parsed.host(), "host-a");
        assert_eq!(parsed.scheduler_instance(), "scheduler-2");
        assert_eq!(parsed.worker_slot(), 7);
    }

    #[test]
    fn round_trip_through_serde_json() {
        let owner = LeaseOwner::new("host-a", "scheduler-1", 3).unwrap();
        let json = serde_json::to_string(&owner).unwrap();
        assert_eq!(json, "\"host-a/scheduler-1/worker-3\"");
        let parsed: LeaseOwner = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, owner);
    }

    #[test]
    fn parse_rejects_missing_components() {
        assert_eq!(
            "host-only".parse::<LeaseOwner>().unwrap_err(),
            LeaseOwnerParseError::MissingComponent,
        );
        assert_eq!(
            "host/scheduler".parse::<LeaseOwner>().unwrap_err(),
            LeaseOwnerParseError::MissingComponent,
        );
    }

    #[test]
    fn parse_rejects_trailing_components() {
        assert_eq!(
            "host/sched/worker-0/extra"
                .parse::<LeaseOwner>()
                .unwrap_err(),
            LeaseOwnerParseError::TrailingComponents,
        );
    }

    #[test]
    fn parse_rejects_missing_worker_prefix() {
        assert_eq!(
            "host/sched/0".parse::<LeaseOwner>().unwrap_err(),
            LeaseOwnerParseError::MissingWorkerPrefix,
        );
    }

    #[test]
    fn parse_rejects_invalid_worker_slot() {
        assert_eq!(
            "host/sched/worker-abc".parse::<LeaseOwner>().unwrap_err(),
            LeaseOwnerParseError::InvalidWorkerSlot,
        );
    }

    #[test]
    fn new_rejects_empty_components() {
        assert_eq!(
            LeaseOwner::new("", "sched", 0).unwrap_err(),
            LeaseOwnerParseError::EmptyComponent { field: "host" },
        );
        assert_eq!(
            LeaseOwner::new("host", "", 0).unwrap_err(),
            LeaseOwnerParseError::EmptyComponent {
                field: "scheduler_instance"
            },
        );
    }

    #[test]
    fn new_rejects_reserved_separator() {
        assert_eq!(
            LeaseOwner::new("ho/st", "sched", 0).unwrap_err(),
            LeaseOwnerParseError::ReservedSeparator { field: "host" },
        );
        assert_eq!(
            LeaseOwner::new("host", "sch/ed", 0).unwrap_err(),
            LeaseOwnerParseError::ReservedSeparator {
                field: "scheduler_instance"
            },
        );
    }

    #[test]
    fn current_process_produces_round_trippable_owner() {
        let owner = LeaseOwner::current_process("scheduler-test", 0).unwrap();
        let parsed: LeaseOwner = owner.to_string().parse().unwrap();
        assert_eq!(owner, parsed);
        assert_eq!(parsed.scheduler_instance(), "scheduler-test");
        assert_eq!(parsed.worker_slot(), 0);
        assert!(!parsed.host().is_empty());
    }
}
