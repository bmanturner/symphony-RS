//! Routing engine (SPEC v2 §5.6).
//!
//! Maps a normalized [`WorkItem`] (plus per-call [`RoutingContext`]) to a
//! destination [`RoleName`] using a deterministic ordered rule list.
//!
//! This module mirrors `symphony_config::RoutingConfig` (and friends)
//! without depending on the config crate, following the same crate-seam
//! discipline as [`crate::role`] and [`crate::work_item::WorkItemId`].
//! Conversion from parsed YAML config into a [`RoutingTable`] happens at
//! the config-load seam in `symphony-cli`/orchestrator wiring.
//!
//! Determinism is the contract: the same `(work item, context, table)`
//! input MUST pick the same role on every run. Two match modes are
//! supported:
//!
//! * [`RoutingMatchMode::FirstMatch`] returns the first rule (in
//!   declaration order) whose `when` clause matches; `priority` fields
//!   are ignored.
//! * [`RoutingMatchMode::Priority`] returns the matching rule with the
//!   highest numeric `priority`; ties are runtime errors so the operator
//!   sees the conflict instead of getting silently inconsistent routing.
//!
//! When no rule matches, [`RoutingTable::default_role`] is used; if that
//! is also unset, the engine returns [`RoutingDecision::NoMatch`] so the
//! caller can surface a routing failure on operator surfaces rather than
//! silently dropping work (per SPEC v2 §5.6 + the `RoutingConfig` doc
//! comment in `symphony-config`).

use std::collections::HashSet;

use crate::role::RoleName;
use crate::work_item::WorkItem;

/// How [`RoutingTable::rules`] are combined when more than one matches.
///
/// Mirrors `symphony_config::RoutingMatchMode`. Closed enum because SPEC
/// v2 §5.6 defines exactly these two modes; adding a third changes the
/// routing contract and should be an explicit schema decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RoutingMatchMode {
    /// Evaluate rules in declaration order; return the first match.
    #[default]
    FirstMatch,
    /// Pick the matching rule with the highest numeric `priority`.
    Priority,
}

/// Predicate clause for a [`RoutingRule`].
///
/// Fields combine as AND-of-fields. Within a single list-typed field the
/// semantics are OR-of-elements (`labels_any` matches when *any* listed
/// label is present on the work item). An empty clause matches every
/// work item — this is how operators express a catch-all without relying
/// on `default_role`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingMatch {
    /// Match when the work item carries any of these labels. Compared
    /// case-insensitively against the lowercase-normalized labels on
    /// [`WorkItem::labels`] (SPEC §11.3 normalization).
    pub labels_any: Vec<String>,
    /// Match when any path glob in this list matches any path in
    /// [`RoutingContext::changed_paths`]. Globs support `**` (any number
    /// of path segments), `*` (within one segment), and `?` (one char
    /// within a segment).
    pub paths_any: Vec<String>,
    /// Match on coarse work-item size from [`RoutingContext::issue_size`]
    /// (e.g. `broad`). Compared case-insensitively.
    pub issue_size: Option<String>,
}

impl RoutingMatch {
    /// True when no predicate fields are configured. An empty match
    /// trivially matches every work item.
    pub fn is_empty(&self) -> bool {
        self.labels_any.is_empty() && self.paths_any.is_empty() && self.issue_size.is_none()
    }
}

/// One routing rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingRule {
    /// Numeric priority used by [`RoutingMatchMode::Priority`]. `None`
    /// is treated as `0`, matching SPEC v2 §5.6 / `symphony-config`.
    pub priority: Option<u32>,
    /// Predicate clause; an empty clause matches every work item.
    pub when: RoutingMatch,
    /// Destination role name on a successful match.
    pub assign_role: RoleName,
}

/// Compiled routing table fed into [`RoutingEngine`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingTable {
    /// Fallback role used when no rule matches. `None` means "no
    /// fallback configured".
    pub default_role: Option<RoleName>,
    /// How to combine matching rules.
    pub match_mode: RoutingMatchMode,
    /// Ordered rule list. Order is significant for
    /// [`RoutingMatchMode::FirstMatch`].
    pub rules: Vec<RoutingRule>,
}

/// Per-call ambient routing context that does not live on the work item.
///
/// `WorkItem` carries labels but not changed paths or coarse size; those
/// derive from the tracker payload + workflow heuristics at dispatch
/// time. Keeping them in a separate struct avoids polluting the durable
/// `WorkItem` shape with transient routing inputs.
#[derive(Debug, Clone, Default)]
pub struct RoutingContext {
    /// Repository-relative paths that this work item is expected to
    /// touch (e.g. from a tracker-supplied "files" list, attached PR,
    /// or specialist preview).
    pub changed_paths: Vec<String>,
    /// Coarse size label (e.g. `broad`, `narrow`). Compared
    /// case-insensitively against [`RoutingMatch::issue_size`].
    pub issue_size: Option<String>,
}

/// Routing outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingDecision {
    /// A rule matched; `rule_index` is the position of that rule in the
    /// configured list, useful for diagnostics and tests.
    Matched {
        /// Destination role for this work item.
        role: RoleName,
        /// Index into [`RoutingTable::rules`] of the winning rule.
        rule_index: usize,
    },
    /// No rule matched; falling back to [`RoutingTable::default_role`].
    Default {
        /// The configured fallback role.
        role: RoleName,
    },
    /// No rule matched and no `default_role` is configured. The kernel
    /// surfaces this on operator surfaces rather than silently dropping
    /// the work item.
    NoMatch,
}

/// Errors produced when evaluating a [`RoutingTable`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    /// Two or more rules matched with the same priority in
    /// [`RoutingMatchMode::Priority`]. Per SPEC v2 §5.6 ties are errors.
    #[error(
        "priority tie between rules at indices {first} and {second} (both at priority {priority}); resolve by adjusting priorities or switching match_mode"
    )]
    PriorityTie {
        /// Index of the first rule involved in the tie.
        first: usize,
        /// Index of the second rule involved in the tie.
        second: usize,
        /// The shared priority value.
        priority: u32,
    },
}

/// Stateless evaluator over a [`RoutingTable`].
///
/// Construct once at config-load time and reuse for every dispatch
/// decision. The engine borrows nothing transient — calls to
/// [`Self::route`] are pure functions of `(table, work_item, context)`.
#[derive(Debug, Clone)]
pub struct RoutingEngine {
    table: RoutingTable,
}

impl RoutingEngine {
    /// Build an engine from a compiled table. No validation is performed
    /// here: reference-resolution (rules naming roles that don't exist
    /// in `WorkflowConfig::roles`) belongs at the config-load seam, and
    /// priority-tie *configuration* validation is also a config-load
    /// concern (the engine only catches ties that surface against a
    /// concrete work item).
    pub fn new(table: RoutingTable) -> Self {
        Self { table }
    }

    /// Borrow the underlying table for inspection.
    pub fn table(&self) -> &RoutingTable {
        &self.table
    }

    /// Evaluate the table against a work item.
    pub fn route(
        &self,
        item: &WorkItem,
        ctx: &RoutingContext,
    ) -> Result<RoutingDecision, RoutingError> {
        let matched: Vec<usize> = self
            .table
            .rules
            .iter()
            .enumerate()
            .filter(|(_, r)| matches_clause(&r.when, item, ctx))
            .map(|(i, _)| i)
            .collect();

        if matched.is_empty() {
            return Ok(match &self.table.default_role {
                Some(role) => RoutingDecision::Default { role: role.clone() },
                None => RoutingDecision::NoMatch,
            });
        }

        match self.table.match_mode {
            RoutingMatchMode::FirstMatch => {
                let i = matched[0];
                Ok(RoutingDecision::Matched {
                    role: self.table.rules[i].assign_role.clone(),
                    rule_index: i,
                })
            }
            RoutingMatchMode::Priority => {
                // Pick the highest priority. Ties are errors, surfaced
                // with both offending indices so the operator can edit
                // the workflow without binary-searching.
                let mut best: Option<(usize, u32)> = None;
                let mut tie: Option<usize> = None;
                for &i in &matched {
                    let p = self.table.rules[i].priority.unwrap_or(0);
                    match best {
                        None => {
                            best = Some((i, p));
                        }
                        Some((_, bp)) if p > bp => {
                            best = Some((i, p));
                            tie = None;
                        }
                        Some((bi, bp)) if p == bp && bi != i => {
                            tie = Some(i);
                        }
                        _ => {}
                    }
                }
                let (i, p) = best.expect("non-empty matched set has a best");
                if let Some(tie_i) = tie {
                    let (first, second) = if i < tie_i { (i, tie_i) } else { (tie_i, i) };
                    return Err(RoutingError::PriorityTie {
                        first,
                        second,
                        priority: p,
                    });
                }
                Ok(RoutingDecision::Matched {
                    role: self.table.rules[i].assign_role.clone(),
                    rule_index: i,
                })
            }
        }
    }
}

fn matches_clause(when: &RoutingMatch, item: &WorkItem, ctx: &RoutingContext) -> bool {
    if !when.labels_any.is_empty() {
        let item_labels: HashSet<String> =
            item.labels.iter().map(|s| s.to_ascii_lowercase()).collect();
        let any = when
            .labels_any
            .iter()
            .any(|l| item_labels.contains(&l.to_ascii_lowercase()));
        if !any {
            return false;
        }
    }
    if !when.paths_any.is_empty() {
        let any = when
            .paths_any
            .iter()
            .any(|pat| ctx.changed_paths.iter().any(|p| glob_match(pat, p)));
        if !any {
            return false;
        }
    }
    if let Some(want) = &when.issue_size {
        match &ctx.issue_size {
            Some(have) if have.eq_ignore_ascii_case(want) => {}
            _ => return false,
        }
    }
    true
}

/// Match a path against a glob pattern.
///
/// Supports gitignore-style segment globs:
/// * `**` matches zero or more `/`-separated path segments.
/// * `*` matches any run of characters within one segment.
/// * `?` matches exactly one character within one segment.
///
/// Path comparison is byte-exact and case-sensitive — workflow authors
/// write paths in their repo's actual casing, and the engine should not
/// silently fold them.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let path_segs: Vec<&str> = path.split('/').collect();
    seg_match(&pat_segs, &path_segs)
}

fn seg_match(pat: &[&str], path: &[&str]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(&"**") => {
            // `**` consumes zero or more whole segments. Try each split.
            for i in 0..=path.len() {
                if seg_match(&pat[1..], &path[i..]) {
                    return true;
                }
            }
            false
        }
        Some(p) => match path.first() {
            Some(s) if single_segment_match(p.as_bytes(), s.as_bytes()) => {
                seg_match(&pat[1..], &path[1..])
            }
            _ => false,
        },
    }
}

fn single_segment_match(pat: &[u8], seg: &[u8]) -> bool {
    match pat.first() {
        None => seg.is_empty(),
        Some(&b'*') => {
            for i in 0..=seg.len() {
                if single_segment_match(&pat[1..], &seg[i..]) {
                    return true;
                }
            }
            false
        }
        Some(&b'?') => match seg.first() {
            None => false,
            Some(_) => single_segment_match(&pat[1..], &seg[1..]),
        },
        Some(c) => match seg.first() {
            Some(s) if s == c => single_segment_match(&pat[1..], &seg[1..]),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::{TrackerStatus, WorkItemId, WorkItemStatusClass};

    fn item_with(labels: &[&str]) -> WorkItem {
        WorkItem {
            id: WorkItemId::new(1),
            tracker_id: "github".into(),
            identifier: "OWNER/REPO#1".into(),
            title: "t".into(),
            description: None,
            tracker_status: TrackerStatus::new("Todo"),
            status_class: WorkItemStatusClass::Intake,
            priority: None,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            url: None,
            parent_id: None,
        }
    }

    fn rule(
        priority: Option<u32>,
        labels: &[&str],
        paths: &[&str],
        size: Option<&str>,
        role: &str,
    ) -> RoutingRule {
        RoutingRule {
            priority,
            when: RoutingMatch {
                labels_any: labels.iter().map(|s| s.to_string()).collect(),
                paths_any: paths.iter().map(|s| s.to_string()).collect(),
                issue_size: size.map(|s| s.to_string()),
            },
            assign_role: RoleName::from(role),
        }
    }

    #[test]
    fn empty_clause_matches_any_work_item() {
        assert!(matches_clause(
            &RoutingMatch::default(),
            &item_with(&[]),
            &RoutingContext::default()
        ));
    }

    #[test]
    fn labels_any_matches_case_insensitively() {
        let when = RoutingMatch {
            labels_any: vec!["QA".into()],
            ..Default::default()
        };
        assert!(matches_clause(
            &when,
            &item_with(&["qa"]),
            &RoutingContext::default()
        ));
        assert!(!matches_clause(
            &when,
            &item_with(&["bug"]),
            &RoutingContext::default()
        ));
    }

    #[test]
    fn paths_any_matches_with_doubleglob_and_single_glob() {
        let ctx = RoutingContext {
            changed_paths: vec!["lib/foo/bar.rs".into()],
            issue_size: None,
        };
        let when = RoutingMatch {
            paths_any: vec!["lib/**".into()],
            ..Default::default()
        };
        assert!(matches_clause(&when, &item_with(&[]), &ctx));

        let when_star = RoutingMatch {
            paths_any: vec!["lib/*/bar.rs".into()],
            ..Default::default()
        };
        assert!(matches_clause(&when_star, &item_with(&[]), &ctx));

        let when_miss = RoutingMatch {
            paths_any: vec!["test/**".into()],
            ..Default::default()
        };
        assert!(!matches_clause(&when_miss, &item_with(&[]), &ctx));
    }

    #[test]
    fn issue_size_matches_case_insensitively() {
        let ctx = RoutingContext {
            changed_paths: vec![],
            issue_size: Some("Broad".into()),
        };
        let when = RoutingMatch {
            issue_size: Some("broad".into()),
            ..Default::default()
        };
        assert!(matches_clause(&when, &item_with(&[]), &ctx));

        let ctx_missing = RoutingContext::default();
        assert!(!matches_clause(&when, &item_with(&[]), &ctx_missing));
    }

    #[test]
    fn first_match_returns_first_matching_rule_in_order() {
        let table = RoutingTable {
            default_role: Some(RoleName::from("platform_lead")),
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![
                rule(Some(10), &["other"], &[], None, "specialist_a"),
                rule(Some(99), &["qa"], &[], None, "qa"),
                rule(Some(50), &["qa"], &[], None, "specialist_b"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(
            decision,
            RoutingDecision::Matched {
                role: RoleName::from("qa"),
                rule_index: 1,
            }
        );
    }

    #[test]
    fn priority_mode_picks_highest_priority_match() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(10), &["qa"], &[], None, "low"),
                rule(Some(100), &["qa"], &[], None, "high"),
                rule(Some(50), &["qa"], &[], None, "mid"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(
            decision,
            RoutingDecision::Matched {
                role: RoleName::from("high"),
                rule_index: 1,
            }
        );
    }

    #[test]
    fn priority_mode_reports_tie_with_both_indices() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(50), &["qa"], &[], None, "first"),
                rule(Some(50), &["qa"], &[], None, "second"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let err = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap_err();
        assert_eq!(
            err,
            RoutingError::PriorityTie {
                first: 0,
                second: 1,
                priority: 50,
            }
        );
    }

    #[test]
    fn missing_priority_is_treated_as_zero_in_priority_mode() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(None, &["qa"], &[], None, "implicit_zero"),
                rule(Some(1), &["qa"], &[], None, "wins"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap();
        assert!(matches!(
            decision,
            RoutingDecision::Matched { rule_index: 1, .. }
        ));
    }

    #[test]
    fn no_match_with_default_role_returns_default() {
        let table = RoutingTable {
            default_role: Some(RoleName::from("platform_lead")),
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["nope"], &[], None, "unused")],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["bug"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(
            decision,
            RoutingDecision::Default {
                role: RoleName::from("platform_lead"),
            }
        );
    }

    #[test]
    fn no_match_without_default_role_returns_no_match() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![rule(None, &["nope"], &[], None, "unused")],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["bug"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(decision, RoutingDecision::NoMatch);
    }

    #[test]
    fn empty_table_with_no_default_returns_no_match() {
        let engine = RoutingEngine::new(RoutingTable::default());
        let decision = engine
            .route(&item_with(&["anything"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(decision, RoutingDecision::NoMatch);
    }

    #[test]
    fn and_of_fields_requires_all_configured_predicates_to_match() {
        let when = RoutingMatch {
            labels_any: vec!["qa".into()],
            paths_any: vec!["lib/**".into()],
            issue_size: None,
        };
        let ctx_only_label = RoutingContext {
            changed_paths: vec!["src/main.rs".into()],
            issue_size: None,
        };
        let ctx_both = RoutingContext {
            changed_paths: vec!["lib/x.rs".into()],
            issue_size: None,
        };
        assert!(!matches_clause(&when, &item_with(&["qa"]), &ctx_only_label));
        assert!(matches_clause(&when, &item_with(&["qa"]), &ctx_both));
        assert!(!matches_clause(&when, &item_with(&["bug"]), &ctx_both));
    }

    #[test]
    fn glob_handles_double_star_zero_segments() {
        // `lib/**` should match `lib/foo.rs` AND `lib` itself? Per
        // gitignore-ish semantics, `**` matches zero or more segments,
        // so `lib/**` matches both. The engine documents this.
        assert!(glob_match("lib/**", "lib"));
        assert!(glob_match("lib/**", "lib/x.rs"));
        assert!(glob_match("lib/**", "lib/a/b.rs"));
        assert!(!glob_match("lib/**", "test/x.rs"));
    }

    #[test]
    fn glob_question_mark_matches_single_char() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(!glob_match("a?c", "ac"));
    }

    // --- first-match dispatch --------------------------------------------

    #[test]
    fn first_match_ignores_priority_field_entirely() {
        // A later rule with a much higher priority must not "win" in
        // first-match mode — order is the only signal.
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![
                rule(Some(1), &["qa"], &[], None, "first_winner"),
                rule(Some(9999), &["qa"], &[], None, "would_win_in_priority"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(
            decision,
            RoutingDecision::Matched {
                role: RoleName::from("first_winner"),
                rule_index: 0,
            }
        );
    }

    #[test]
    fn first_match_catch_all_at_top_shadows_specific_rules_below() {
        // An empty `when` is a catch-all. Operators using first-match must
        // place it last; placing it first intentionally short-circuits.
        let table = RoutingTable {
            default_role: Some(RoleName::from("fallback")),
            match_mode: RoutingMatchMode::FirstMatch,
            rules: vec![
                rule(None, &[], &[], None, "catch_all"),
                rule(None, &["qa"], &[], None, "specific"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(
            decision,
            RoutingDecision::Matched {
                role: RoleName::from("catch_all"),
                rule_index: 0,
            }
        );
    }

    // --- priority dispatch -----------------------------------------------

    #[test]
    fn priority_mode_falls_through_to_default_role_when_nothing_matches() {
        let table = RoutingTable {
            default_role: Some(RoleName::from("platform_lead")),
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(50), &["docs"], &[], None, "docs_role"),
                rule(Some(99), &["infra"], &[], None, "infra_role"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["bug"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(
            decision,
            RoutingDecision::Default {
                role: RoleName::from("platform_lead"),
            }
        );
    }

    #[test]
    fn priority_mode_returns_no_match_when_no_rule_and_no_default() {
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![rule(Some(99), &["docs"], &[], None, "docs_role")],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["bug"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(decision, RoutingDecision::NoMatch);
    }

    #[test]
    fn priority_tie_is_detected_between_non_adjacent_rules() {
        // Rules at indices 0 and 3 share the winning priority, with a
        // lower-priority rule between them that must not mask the tie.
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(50), &["qa"], &[], None, "first"),
                rule(Some(10), &["qa"], &[], None, "noise_low"),
                rule(Some(20), &["qa"], &[], None, "noise_mid"),
                rule(Some(50), &["qa"], &[], None, "fourth"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let err = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap_err();
        assert_eq!(
            err,
            RoutingError::PriorityTie {
                first: 0,
                second: 3,
                priority: 50,
            }
        );
    }

    #[test]
    fn priority_none_ties_with_explicit_zero() {
        // `None` is documented as equivalent to `0`. When the only two
        // matching rules are `None` and `Some(0)`, they MUST tie.
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(None, &["qa"], &[], None, "implicit_zero"),
                rule(Some(0), &["qa"], &[], None, "explicit_zero"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let err = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap_err();
        assert_eq!(
            err,
            RoutingError::PriorityTie {
                first: 0,
                second: 1,
                priority: 0,
            }
        );
    }

    #[test]
    fn priority_mode_records_winning_rule_index() {
        // The decision must report the index of the rule that actually
        // won, not e.g. the first matching rule scanned.
        let table = RoutingTable {
            default_role: None,
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(10), &["qa"], &[], None, "low"),
                rule(Some(40), &["qa"], &[], None, "mid"),
                rule(Some(99), &["qa"], &[], None, "high"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let decision = engine
            .route(&item_with(&["qa"]), &RoutingContext::default())
            .unwrap();
        assert_eq!(
            decision,
            RoutingDecision::Matched {
                role: RoleName::from("high"),
                rule_index: 2,
            }
        );
    }

    // --- predicate semantics ---------------------------------------------

    #[test]
    fn labels_any_is_or_across_listed_labels() {
        // Within `labels_any`, listed labels combine as OR. Either of the
        // two configured labels present on the work item should match.
        let when = RoutingMatch {
            labels_any: vec!["bug".into(), "qa".into()],
            ..Default::default()
        };
        assert!(matches_clause(
            &when,
            &item_with(&["qa"]),
            &RoutingContext::default()
        ));
        assert!(matches_clause(
            &when,
            &item_with(&["bug"]),
            &RoutingContext::default()
        ));
        assert!(!matches_clause(
            &when,
            &item_with(&["docs"]),
            &RoutingContext::default()
        ));
    }

    #[test]
    fn paths_any_is_or_across_listed_globs_and_changed_paths() {
        let when = RoutingMatch {
            paths_any: vec!["docs/**".into(), "infra/**".into()],
            ..Default::default()
        };
        let ctx_match_first = RoutingContext {
            changed_paths: vec!["docs/intro.md".into()],
            issue_size: None,
        };
        let ctx_match_second = RoutingContext {
            changed_paths: vec!["src/main.rs".into(), "infra/terraform/main.tf".into()],
            issue_size: None,
        };
        let ctx_no_match = RoutingContext {
            changed_paths: vec!["src/main.rs".into()],
            issue_size: None,
        };
        assert!(matches_clause(&when, &item_with(&[]), &ctx_match_first));
        assert!(matches_clause(&when, &item_with(&[]), &ctx_match_second));
        assert!(!matches_clause(&when, &item_with(&[]), &ctx_no_match));
    }

    // --- determinism ------------------------------------------------------

    #[test]
    fn routing_is_deterministic_across_repeated_calls() {
        // Same inputs → same decision, every time. This is the headline
        // contract of the engine.
        let table = RoutingTable {
            default_role: Some(RoleName::from("platform_lead")),
            match_mode: RoutingMatchMode::Priority,
            rules: vec![
                rule(Some(10), &["qa"], &[], None, "low"),
                rule(Some(99), &["qa"], &[], None, "high"),
                rule(Some(50), &["qa"], &[], None, "mid"),
            ],
        };
        let engine = RoutingEngine::new(table);
        let item = item_with(&["qa"]);
        let ctx = RoutingContext::default();
        let first = engine.route(&item, &ctx).unwrap();
        for _ in 0..16 {
            assert_eq!(engine.route(&item, &ctx).unwrap(), first);
        }
    }
}
