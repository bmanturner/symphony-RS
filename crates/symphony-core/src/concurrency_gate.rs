//! Multi-scope concurrency gate (CHECKLIST_v2 Phase 11).
//!
//! [`ConcurrencyGate`] is a pure in-process primitive that enforces
//! per-scope dispatch caps before a runner invokes an agent. Scopes
//! correspond to `symphony_config::ConcurrencyConfig`
//! — `Global`, `Role`, `AgentProfile`, `Repository` — so a single
//! dispatch can hold permits across several scopes simultaneously and
//! release them as a unit when the run terminates.
//!
//! The gate is deliberately synchronous and has no awareness of
//! durable state: callers compose acquisition with lease bookkeeping
//! and event emission upstream. Acquisition is *all-or-nothing*; if
//! any requested scope is at cap, every previously acquired permit in
//! the same call is released before [`ConcurrencyGate::try_acquire`]
//! returns `None`. Successful acquisition returns a
//! [`ScopePermitSet`] whose `Drop` decrements in-flight counts for
//! every held scope.
//!
//! Missing scope entries default to **unbounded** so an operator who
//! does not configure `concurrency.per_repository.foo` does not
//! accidentally cap that scope at zero. A scope explicitly configured
//! with a zero cap rejects every acquisition — that is how operators
//! park a scope without removing it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The kind of scope a permit is held against.
///
/// Mirrors the four cap surfaces in
/// `symphony_config::ConcurrencyConfig`. Serialised as a snake_case
/// string so downstream wire formats (e.g.
/// [`crate::events::OrchestratorEvent::ScopeCapReached`]) can include it
/// without bespoke encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// Single global bucket. Always keyed by [`Scope::GLOBAL_KEY`].
    Global,
    /// Per-role bucket, keyed by the operator-chosen role name.
    Role,
    /// Per-agent-profile bucket, keyed by the profile name in
    /// `WorkflowConfig::agents`.
    AgentProfile,
    /// Per-repository bucket, keyed by repo slug.
    Repository,
}

/// A concrete scope a permit is held against.
///
/// Constructed by callers from a dispatch's `(role, agent_profile,
/// repository)` triple; the `Global` variant is a singleton.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// The single global bucket.
    Global,
    /// A role-scoped bucket.
    Role(String),
    /// An agent-profile-scoped bucket.
    AgentProfile(String),
    /// A repository-scoped bucket.
    Repository(String),
}

impl Scope {
    /// Sentinel key used internally for [`Scope::Global`].
    pub const GLOBAL_KEY: &'static str = "";

    /// The [`ScopeKind`] of this scope.
    pub fn kind(&self) -> ScopeKind {
        match self {
            Scope::Global => ScopeKind::Global,
            Scope::Role(_) => ScopeKind::Role,
            Scope::AgentProfile(_) => ScopeKind::AgentProfile,
            Scope::Repository(_) => ScopeKind::Repository,
        }
    }

    /// The key portion of this scope.
    pub fn key(&self) -> &str {
        match self {
            Scope::Global => Self::GLOBAL_KEY,
            Scope::Role(k) | Scope::AgentProfile(k) | Scope::Repository(k) => k.as_str(),
        }
    }

    fn as_index(&self) -> ScopeIndex {
        ScopeIndex(self.kind(), self.key().to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ScopeIndex(ScopeKind, String);

/// Snapshot of a single scope's available headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeAvailability {
    /// No cap configured — acquisition always succeeds.
    Unbounded { in_flight: u32 },
    /// A cap is configured. `available = cap - in_flight`.
    Bounded {
        cap: u32,
        in_flight: u32,
        available: u32,
    },
}

impl ScopeAvailability {
    /// Number of permits currently held for this scope.
    pub fn in_flight(&self) -> u32 {
        match self {
            ScopeAvailability::Unbounded { in_flight } => *in_flight,
            ScopeAvailability::Bounded { in_flight, .. } => *in_flight,
        }
    }

    /// `true` if a fresh acquisition would succeed (cap permitting).
    pub fn has_headroom(&self) -> bool {
        match self {
            ScopeAvailability::Unbounded { .. } => true,
            ScopeAvailability::Bounded { available, .. } => *available > 0,
        }
    }
}

/// A multi-scope concurrency gate.
///
/// Cheap to clone — internally an `Arc` over a single [`Mutex`].
#[derive(Debug, Clone, Default)]
pub struct ConcurrencyGate {
    inner: Arc<Mutex<GateInner>>,
}

#[derive(Debug, Default)]
struct GateInner {
    caps: HashMap<ScopeIndex, u32>,
    in_flight: HashMap<ScopeIndex, u32>,
}

impl ConcurrencyGate {
    /// Build an empty gate (every scope unbounded).
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the cap for a single scope.
    ///
    /// A `cap` of `0` means "always reject"; callers who want
    /// "unbounded" should call [`ConcurrencyGate::clear_cap`] instead.
    pub fn set_cap(&self, scope: Scope, cap: u32) {
        let mut g = self.inner.lock().expect("ConcurrencyGate mutex poisoned");
        g.caps.insert(scope.as_index(), cap);
    }

    /// Remove any configured cap for a scope, restoring its unbounded
    /// default.
    pub fn clear_cap(&self, scope: Scope) {
        let mut g = self.inner.lock().expect("ConcurrencyGate mutex poisoned");
        g.caps.remove(&scope.as_index());
    }

    /// Inspect a scope's current availability.
    pub fn available(&self, scope: &Scope) -> ScopeAvailability {
        let g = self.inner.lock().expect("ConcurrencyGate mutex poisoned");
        let idx = scope.as_index();
        let in_flight = g.in_flight.get(&idx).copied().unwrap_or(0);
        match g.caps.get(&idx).copied() {
            None => ScopeAvailability::Unbounded { in_flight },
            Some(cap) => ScopeAvailability::Bounded {
                cap,
                in_flight,
                available: cap.saturating_sub(in_flight),
            },
        }
    }

    /// Try to acquire one permit per `scope` in `scopes`, atomically.
    ///
    /// Returns a [`ScopePermitSet`] on success. If any scope is at cap
    /// (or has a zero cap configured), every permit acquired earlier in
    /// the same call is released and the offending scope is reported in
    /// [`ScopeContended`].
    ///
    /// Duplicate entries are honored: requesting the same scope twice
    /// counts as two permits.
    pub fn try_acquire(&self, scopes: &[Scope]) -> Result<ScopePermitSet, ScopeContended> {
        let mut g = self.inner.lock().expect("ConcurrencyGate mutex poisoned");
        let mut held: Vec<ScopeIndex> = Vec::with_capacity(scopes.len());
        for scope in scopes {
            let idx = scope.as_index();
            let cap = g.caps.get(&idx).copied();
            let in_flight = g.in_flight.get(&idx).copied().unwrap_or(0);
            match cap {
                Some(cap) if in_flight >= cap => {
                    // Roll back partial acquisitions before bailing.
                    for prior in &held {
                        let entry = g.in_flight.entry(prior.clone()).or_insert(0);
                        *entry = entry.saturating_sub(1);
                    }
                    return Err(ScopeContended {
                        scope: scope.clone(),
                        cap,
                        in_flight,
                    });
                }
                _ => {
                    *g.in_flight.entry(idx.clone()).or_insert(0) += 1;
                    held.push(idx);
                }
            }
        }
        Ok(ScopePermitSet {
            inner: Arc::clone(&self.inner),
            held,
        })
    }
}

/// A bundle of permits held against a [`ConcurrencyGate`].
///
/// Drop releases every held permit. Dropping is idempotent across
/// successive [`ScopePermitSet::release`] calls because `release`
/// consumes the set.
#[derive(Debug)]
#[must_use = "permits release on drop; binding to `_` releases immediately"]
pub struct ScopePermitSet {
    inner: Arc<Mutex<GateInner>>,
    held: Vec<ScopeIndex>,
}

impl ScopePermitSet {
    /// Number of permits in this set.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// `true` if no permits are held (empty acquire).
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Release every permit eagerly (rather than waiting for drop).
    pub fn release(mut self) {
        self.release_in_place();
    }

    fn release_in_place(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for idx in self.held.drain(..) {
            let entry = g.in_flight.entry(idx).or_insert(0);
            *entry = entry.saturating_sub(1);
        }
    }
}

impl Drop for ScopePermitSet {
    fn drop(&mut self) {
        self.release_in_place();
    }
}

/// Reason a [`ConcurrencyGate::try_acquire`] call rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeContended {
    /// The scope that was at (or above) cap.
    pub scope: Scope,
    /// The configured cap on that scope.
    pub cap: u32,
    /// In-flight permits on that scope at the moment of rejection.
    pub in_flight: u32,
}

/// The `(role, agent_profile, repository)` triple a dispatch derives
/// its scope permits from.
///
/// `role` is always present — every dispatch has a role. `agent_profile`
/// and `repository` are optional: a workflow that has not yet bound an
/// agent profile to a role, or a tracker without a repository slug,
/// simply skips that scope rather than acquiring against an empty key.
///
/// Construct via [`DispatchTriple::new`] and read back the canonical
/// scope-request list with [`DispatchTriple::scopes`]. The shape is
/// fixed and ordered so downstream tests can assert it deterministically:
///
/// 1. [`Scope::Global`]
/// 2. [`Scope::Role`] (always)
/// 3. [`Scope::AgentProfile`] (iff `agent_profile.is_some()`)
/// 4. [`Scope::Repository`] (iff `repository.is_some()`)
///
/// This subtask intentionally does not change runner behavior — it only
/// fixes the request shape so subsequent per-runner subtasks can wire
/// acquisition uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchTriple {
    role: String,
    agent_profile: Option<String>,
    repository: Option<String>,
}

impl DispatchTriple {
    /// Construct a dispatch triple from a role and optional agent
    /// profile / repository slug. An empty `role` string is allowed at
    /// the gate layer — workflow validation rejects empties upstream.
    pub fn new(
        role: impl Into<String>,
        agent_profile: Option<String>,
        repository: Option<String>,
    ) -> Self {
        Self {
            role: role.into(),
            agent_profile,
            repository,
        }
    }

    /// Borrow the role label.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// Borrow the agent profile name, if bound.
    pub fn agent_profile(&self) -> Option<&str> {
        self.agent_profile.as_deref()
    }

    /// Borrow the repository slug, if bound.
    pub fn repository(&self) -> Option<&str> {
        self.repository.as_deref()
    }

    /// Canonical scope-request derivation for this triple.
    ///
    /// See [`DispatchTriple`] for the fixed ordering. The returned
    /// `Vec<Scope>` is the exact argument runners pass to
    /// [`ConcurrencyGate::try_acquire`].
    pub fn scopes(&self) -> Vec<Scope> {
        let mut out = Vec::with_capacity(4);
        out.push(Scope::Global);
        out.push(Scope::Role(self.role.clone()));
        if let Some(profile) = &self.agent_profile {
            out.push(Scope::AgentProfile(profile.clone()));
        }
        if let Some(repo) = &self.repository {
            out.push(Scope::Repository(repo.clone()));
        }
        out
    }
}

/// Bundle a [`ConcurrencyGate`] with a dispatch's [`DispatchTriple`] so
/// runners share one acquire surface.
///
/// Each dispatch builds a [`RunnerScopes`] from its triple, calls
/// [`RunnerScopes::try_acquire`] before invoking the agent, and holds
/// the returned [`ScopePermitSet`] for the lifetime of the dispatch —
/// `Drop` releases every held permit on terminal completion.
///
/// This struct is intentionally thin: no runner consumes it yet. The
/// next checklist subtasks plumb it through individual runners.
#[derive(Debug, Clone)]
pub struct RunnerScopes {
    gate: ConcurrencyGate,
    triple: DispatchTriple,
}

impl RunnerScopes {
    /// Build a [`RunnerScopes`] from a gate clone and a dispatch triple.
    pub fn new(gate: ConcurrencyGate, triple: DispatchTriple) -> Self {
        Self { gate, triple }
    }

    /// Borrow the underlying triple.
    pub fn triple(&self) -> &DispatchTriple {
        &self.triple
    }

    /// Borrow the underlying gate.
    pub fn gate(&self) -> &ConcurrencyGate {
        &self.gate
    }

    /// The exact scope-request list this dispatch will acquire.
    ///
    /// Equivalent to `self.triple().scopes()`, surfaced as a method so
    /// callers can read the request without reaching into the triple.
    pub fn request(&self) -> Vec<Scope> {
        self.triple.scopes()
    }

    /// Try to acquire one permit per scope in [`RunnerScopes::request`],
    /// atomically. Forwards to [`ConcurrencyGate::try_acquire`].
    pub fn try_acquire(&self) -> Result<ScopePermitSet, ScopeContended> {
        self.gate.try_acquire(&self.request())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(s: &str) -> Scope {
        Scope::Role(s.to_owned())
    }
    fn profile(s: &str) -> Scope {
        Scope::AgentProfile(s.to_owned())
    }
    fn repo(s: &str) -> Scope {
        Scope::Repository(s.to_owned())
    }

    #[test]
    fn cap_respected_per_scope() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(role("specialist"), 2);

        let p1 = gate.try_acquire(&[role("specialist")]).unwrap();
        let p2 = gate.try_acquire(&[role("specialist")]).unwrap();
        let err = gate.try_acquire(&[role("specialist")]).unwrap_err();
        assert_eq!(err.scope, role("specialist"));
        assert_eq!(err.cap, 2);
        assert_eq!(err.in_flight, 2);

        drop(p1);
        let _p3 = gate.try_acquire(&[role("specialist")]).unwrap();
        drop(p2);
    }

    #[test]
    fn missing_scope_keys_default_to_unbounded() {
        let gate = ConcurrencyGate::new();
        // No caps configured at all.
        let mut held = Vec::new();
        for _ in 0..32 {
            held.push(gate.try_acquire(&[role("anything")]).unwrap());
        }
        assert_eq!(held.len(), 32);
        match gate.available(&role("anything")) {
            ScopeAvailability::Unbounded { in_flight } => assert_eq!(in_flight, 32),
            other => panic!("expected unbounded, got {other:?}"),
        }
    }

    #[test]
    fn zero_cap_always_rejects() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(repo("frozen"), 0);

        let err = gate.try_acquire(&[repo("frozen")]).unwrap_err();
        assert_eq!(err.scope, repo("frozen"));
        assert_eq!(err.cap, 0);
        assert_eq!(err.in_flight, 0);

        // available() reflects bounded-zero shape.
        match gate.available(&repo("frozen")) {
            ScopeAvailability::Bounded {
                cap,
                in_flight,
                available,
            } => {
                assert_eq!(cap, 0);
                assert_eq!(in_flight, 0);
                assert_eq!(available, 0);
            }
            other => panic!("expected bounded, got {other:?}"),
        }
    }

    #[test]
    fn contention_on_one_scope_releases_prior_partials() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Global, 4);
        gate.set_cap(role("integration"), 1);

        let _hold = gate
            .try_acquire(&[Scope::Global, role("integration")])
            .unwrap();

        // Second acquire wants Global (still has room) then integration (full).
        let err = gate
            .try_acquire(&[Scope::Global, role("integration")])
            .unwrap_err();
        assert_eq!(err.scope, role("integration"));

        // Global must NOT be incremented by the failed call: confirm by
        // exhausting global from 1 → 4 and observing exactly 3 fits.
        let p1 = gate.try_acquire(&[Scope::Global]).unwrap();
        let p2 = gate.try_acquire(&[Scope::Global]).unwrap();
        let p3 = gate.try_acquire(&[Scope::Global]).unwrap();
        let again = gate.try_acquire(&[Scope::Global]).unwrap_err();
        assert_eq!(again.scope, Scope::Global);
        drop((p1, p2, p3));
    }

    #[test]
    fn repeated_acquire_release_cycles_preserve_counts() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(profile("claude"), 3);

        for _ in 0..50 {
            let a = gate.try_acquire(&[profile("claude")]).unwrap();
            let b = gate.try_acquire(&[profile("claude")]).unwrap();
            let c = gate.try_acquire(&[profile("claude")]).unwrap();
            assert!(gate.try_acquire(&[profile("claude")]).is_err());
            drop((a, b, c));
            assert_eq!(gate.available(&profile("claude")).in_flight(), 0);
        }
    }

    #[test]
    fn permit_set_release_is_eager() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(role("qa"), 1);
        let p = gate.try_acquire(&[role("qa")]).unwrap();
        assert!(gate.try_acquire(&[role("qa")]).is_err());
        p.release();
        // Permit released eagerly — fresh acquire succeeds without drop scope.
        let _p2 = gate.try_acquire(&[role("qa")]).unwrap();
    }

    #[test]
    fn duplicate_scope_in_request_consumes_two_permits() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Global, 2);
        let _hold = gate.try_acquire(&[Scope::Global, Scope::Global]).unwrap();
        let err = gate.try_acquire(&[Scope::Global]).unwrap_err();
        assert_eq!(err.in_flight, 2);
    }

    #[test]
    fn duplicate_scope_rolls_back_when_second_copy_blocks() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Global, 1);
        let err = gate
            .try_acquire(&[Scope::Global, Scope::Global])
            .unwrap_err();
        assert_eq!(err.scope, Scope::Global);
        // The first copy must have rolled back: a fresh single acquire
        // should succeed.
        let _p = gate.try_acquire(&[Scope::Global]).unwrap();
    }

    #[test]
    fn available_reports_bounded_after_acquisition() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(repo("alpha"), 5);
        let _p = gate.try_acquire(&[repo("alpha")]).unwrap();
        match gate.available(&repo("alpha")) {
            ScopeAvailability::Bounded {
                cap,
                in_flight,
                available,
            } => {
                assert_eq!(cap, 5);
                assert_eq!(in_flight, 1);
                assert_eq!(available, 4);
            }
            other => panic!("expected bounded, got {other:?}"),
        }
    }

    #[test]
    fn clear_cap_restores_unbounded() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(role("specialist"), 1);
        let p = gate.try_acquire(&[role("specialist")]).unwrap();
        assert!(gate.try_acquire(&[role("specialist")]).is_err());
        gate.clear_cap(role("specialist"));
        let _p2 = gate.try_acquire(&[role("specialist")]).unwrap();
        let _p3 = gate.try_acquire(&[role("specialist")]).unwrap();
        drop(p);
    }

    #[test]
    fn empty_acquire_succeeds_with_empty_set() {
        let gate = ConcurrencyGate::new();
        let permit = gate.try_acquire(&[]).unwrap();
        assert!(permit.is_empty());
        assert_eq!(permit.len(), 0);
    }

    #[test]
    fn dispatch_triple_full_request_includes_four_scopes_in_order() {
        let triple = DispatchTriple::new(
            "specialist",
            Some("claude".into()),
            Some("acme/widgets".into()),
        );
        let scopes = triple.scopes();
        assert_eq!(
            scopes,
            vec![
                Scope::Global,
                Scope::Role("specialist".into()),
                Scope::AgentProfile("claude".into()),
                Scope::Repository("acme/widgets".into()),
            ]
        );
    }

    #[test]
    fn dispatch_triple_drops_agent_profile_when_unbound() {
        let triple = DispatchTriple::new("integration_owner", None, Some("acme/widgets".into()));
        assert_eq!(
            triple.scopes(),
            vec![
                Scope::Global,
                Scope::Role("integration_owner".into()),
                Scope::Repository("acme/widgets".into()),
            ]
        );
    }

    #[test]
    fn dispatch_triple_drops_repository_when_unbound() {
        let triple = DispatchTriple::new("qa", Some("claude".into()), None);
        assert_eq!(
            triple.scopes(),
            vec![
                Scope::Global,
                Scope::Role("qa".into()),
                Scope::AgentProfile("claude".into()),
            ]
        );
    }

    #[test]
    fn dispatch_triple_minimal_request_is_global_then_role() {
        let triple = DispatchTriple::new("specialist", None, None);
        assert_eq!(
            triple.scopes(),
            vec![Scope::Global, Scope::Role("specialist".into())]
        );
    }

    #[test]
    fn dispatch_triple_accessors_round_trip() {
        let triple = DispatchTriple::new("qa", Some("claude".into()), Some("acme/widgets".into()));
        assert_eq!(triple.role(), "qa");
        assert_eq!(triple.agent_profile(), Some("claude"));
        assert_eq!(triple.repository(), Some("acme/widgets"));
    }

    #[test]
    fn runner_scopes_request_matches_triple_scopes() {
        let gate = ConcurrencyGate::new();
        let triple = DispatchTriple::new(
            "specialist",
            Some("claude".into()),
            Some("acme/widgets".into()),
        );
        let scopes = RunnerScopes::new(gate, triple.clone());
        assert_eq!(scopes.request(), triple.scopes());
        assert_eq!(scopes.triple(), &triple);
    }

    #[test]
    fn runner_scopes_try_acquire_uses_canonical_shape() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(role("specialist"), 1);
        let triple = DispatchTriple::new("specialist", Some("claude".into()), None);
        let scopes = RunnerScopes::new(gate.clone(), triple);

        let permit = scopes.try_acquire().unwrap();
        // Role is at cap=1; another acquire on the same triple must
        // contend on Role (proving Role is in the request).
        let err = scopes.try_acquire().unwrap_err();
        assert_eq!(err.scope, role("specialist"));
        drop(permit);

        // After release, a fresh acquire succeeds again.
        let _again = scopes.try_acquire().unwrap();
    }

    #[test]
    fn runner_scopes_try_acquire_contends_on_repository_when_bound() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(repo("acme/widgets"), 1);
        let triple = DispatchTriple::new("specialist", None, Some("acme/widgets".into()));
        let scopes = RunnerScopes::new(gate, triple);

        let _hold = scopes.try_acquire().unwrap();
        let err = scopes.try_acquire().unwrap_err();
        assert_eq!(err.scope, repo("acme/widgets"));
    }

    #[test]
    fn runner_scopes_with_no_caps_acquires_unbounded() {
        let gate = ConcurrencyGate::new();
        let triple = DispatchTriple::new(
            "specialist",
            Some("claude".into()),
            Some("acme/widgets".into()),
        );
        let scopes = RunnerScopes::new(gate, triple);
        let mut held = Vec::new();
        for _ in 0..16 {
            held.push(scopes.try_acquire().unwrap());
        }
        assert_eq!(held.len(), 16);
    }

    #[test]
    fn scopes_with_different_kinds_share_keyspace_safely() {
        let gate = ConcurrencyGate::new();
        gate.set_cap(role("shared"), 1);
        gate.set_cap(profile("shared"), 1);
        // Same string key, different kinds — separate buckets.
        let _a = gate.try_acquire(&[role("shared")]).unwrap();
        let _b = gate.try_acquire(&[profile("shared")]).unwrap();
        assert!(gate.try_acquire(&[role("shared")]).is_err());
        assert!(gate.try_acquire(&[profile("shared")]).is_err());
    }
}
