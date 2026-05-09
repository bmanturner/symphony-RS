# Symphony-RS Architecture v4 Addendum

This document describes the target architecture for SPEC v4: per-role
instruction packs, structured assignment metadata, and a generated
decomposition role catalog that gives the platform lead enough doctrine
to assign children correctly.

`SPEC_v4.md` is the product contract and `CHECKLIST_v4.md` is the
iteration plan. This file explains how the v4 requirements land on the
existing v2/v3 crate surfaces. It is an **addendum to
`ARCHITECTURE_v2.md` and `ARCHITECTURE_v3.md`** and does not replace any
prior architectural decision.

## 1. Architectural Goal

v2 has the seams: `RoleConfig`, `AgentBackendProfile.system_prompt`, a
strict `PromptContext` renderer, and `RoutingConfig`/`RoutingRule`. v3
adds dependency-aware sequencing. What is missing is the operational
bridge between role doctrine (file-backed `role_prompt` and `soul`) and
the platform lead's decomposition prompt: today the lead gets one global
`prompt_template` plus terse YAML descriptions of its peers, and that is
not enough doctrine to assign children correctly.

The v4 kernel must, at every dispatch:

- resolve each role's instruction pack at workflow-load time;
- assemble prompts in a deterministic, role-aware order with explicit
  section provenance;
- assemble a platform-lead role catalog automatically from `WORKFLOW.md`
  metadata, never from an out-of-band `ASSIGNMENT.md`;
- keep transient run context (issue text, blockers) cleanly separate
  from durable role doctrine;
- fail loud on missing/escaping instruction file paths and on unknown
  prompt placeholders.

## 2. Layered View

```text
┌───────────────────────────────────────────────────────────────────────┐
│ Policy / Doctrine Layer                                               │
│   WORKFLOW.md (roles + agents + routing + assignment metadata)        │
│   .symphony/roles/<role>/AGENTS.md     (role_prompt, file-backed)     │
│   .symphony/roles/<role>/SOUL.md       (soul, file-backed)            │
├───────────────────────────────────────────────────────────────────────┤
│ Loader Layer (symphony-config)                                        │
│   WorkflowLoader → LoadedWorkflow + InstructionPackBundle             │
│   path-escape / existence / unknown-key validation                    │
├───────────────────────────────────────────────────────────────────────┤
│ Catalog + Prompt Assembly Layer (symphony-core)                       │
│   RoleCatalogBuilder · PromptAssembler · prompt section types         │
│   role-aware variants: specialist, decomposition, qa                  │
├───────────────────────────────────────────────────────────────────────┤
│ Dispatch Layer (symphony-cli, symphony-core)                          │
│   replaces legacy run.rs::render_prompt with strict assembly          │
│   instruction provenance recorded on each Run                         │
├───────────────────────────────────────────────────────────────────────┤
│ Observability                                                         │
│   `symphony prompt preview` · catalog inspect · run metadata          │
└───────────────────────────────────────────────────────────────────────┘
```

## 3. Key Design Shifts From v2/v3

### 3.1 From global `prompt_template` to role-aware assembly

Today `LoadedWorkflow.prompt_template` is the trimmed Markdown body of
`WORKFLOW.md`, and `run.rs::render_prompt` does an issue-field-only
substitution. v4 keeps `prompt_template` as the **global workflow
prompt** (section 1 of every assembly) and adds explicit per-role
sections layered on top.

### 3.2 From terse YAML descriptions to structured assignment metadata

`RoleConfig` carries `description` (free-form) and `agent`. v4 adds
`owns`, `does_not_own`, `requires`, `handoff_expectations`, plus
role-local routing hints (`paths_any`, `labels_any`, domain hints). The
catalog builder reads these directly so the platform lead's prompt does
not depend on a parallel `ASSIGNMENT.md`.

### 3.3 From inline `system_prompt` to file-backed instruction packs

`AgentBackendProfile.system_prompt` remains, but it is now scoped to
**backend** doctrine (model behavior, tool instructions). Role-level
doctrine moves to file-backed `role_prompt` and `soul` paths under a new
`RoleInstructionConfig`. The two surfaces compose; neither replaces the
other.

### 3.4 From "anything in `{{...}}`" to typed prompt sections

The legacy substitution is permissive — unknown placeholders silently
remain as literal `{{name}}`. v4 makes the production path go through
the existing strict `PromptContext` renderer (or its v4-extended
sibling) and treats unknown placeholders as a fail-loud error.

### 3.5 From "all roles flat" to a role catalog projection

The platform-lead-facing catalog is a **projection** over
`WorkflowConfig.roles`, `WorkflowConfig.agents`, and the routing tables.
It is computed deterministically and is the only thing the lead reads
about its peers. Specialists never see other specialists' SOUL files.

## 4. Crate Evolution

### 4.1 `symphony-config`

#### 4.1.1 `RoleInstructionConfig`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RoleInstructionConfig {
    /// Long-form operating instructions for the role. Path is workflow-
    /// root-relative; absolute paths and `..` traversal are rejected.
    #[serde(default)]
    pub role_prompt: Option<PathBuf>,

    /// Stable behavioral doctrine / quality bar / SOUL.
    #[serde(default)]
    pub soul: Option<PathBuf>,
}
```

Mounted on `RoleConfig`:

```rust
pub struct RoleConfig {
    // existing v2 fields unchanged ...
    pub kind: RoleKind,
    pub description: Option<String>,
    pub agent: Option<String>,
    // ... authority flags ...

    // new in v4
    pub instructions: RoleInstructionConfig,
    pub assignment: RoleAssignmentMetadata,
}
```

#### 4.1.2 `RoleAssignmentMetadata`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RoleAssignmentMetadata {
    /// Bullets describing work this role should receive.
    #[serde(default)]
    pub owns: Vec<String>,

    /// Bullets describing work this role should NOT receive.
    #[serde(default)]
    pub does_not_own: Vec<String>,

    /// Prerequisite context or dependencies this role usually needs.
    #[serde(default)]
    pub requires: Vec<String>,

    /// Evidence / handoff fields this role must return.
    #[serde(default)]
    pub handoff_expectations: Vec<String>,

    /// Role-local routing hints. Distinct from the global
    /// `routing.rules` table: these are advisory inputs the catalog
    /// builder renders verbatim, not deterministic dispatch rules.
    #[serde(default)]
    pub routing_hints: RoleRoutingHints,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RoleRoutingHints {
    #[serde(default)] pub paths_any: Vec<String>,
    #[serde(default)] pub labels_any: Vec<String>,
    #[serde(default)] pub issue_types_any: Vec<String>,
    #[serde(default)] pub domains_any: Vec<String>,
}
```

The split between `RoleRoutingHints` and global `RoutingConfig::rules`
is deliberate (see §9 ADR): role-local hints are **advisory catalog
text**; global rules are **deterministic dispatch authority**. The
catalog builder reads both; the dispatcher reads only global.

#### 4.1.3 Validation

Added to `WorkflowConfig::validate` (or its loader-time successor):

| check | severity | message |
|-------|----------|---------|
| instruction path escapes workflow root | fail | `instruction_path_escapes_root` |
| configured instruction file is missing/unreadable | fail | `instruction_file_missing` |
| unknown key in `instructions:` block | fail | `unknown_instruction_key` |
| role references missing agent profile | fail (already v2) | unchanged |
| decomposition enabled but no eligible child-owning role | fail | `no_eligible_child_owning_roles` |
| specialist role has neither `assignment.owns` nor routing hints, only terse `description` | warn | `terse_specialist_role` |
| platform-lead catalog would be built mostly from fallback descriptions | warn | `weak_catalog_metadata` |
| platform-lead prompt would exceed token budget after assembly | warn | `oversized_decomposition_prompt` |

The existing `LoadedWorkflow` already retains `source_path`; the
workflow root is its parent directory. Path-escape validation
canonicalizes via `Path::canonicalize` and asserts the result starts
with the canonicalized root.

### 4.2 `symphony-core`

#### 4.2.1 New module: `instruction_pack.rs`

Owns the typed file-backed doctrine bundle and the loader/cache:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionPack {
    pub role: RoleName,
    pub role_prompt: Option<InstructionFile>,
    pub soul: Option<InstructionFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
    pub hash: String,        // sha256 of content; provenance + cache key
    pub mtime: SystemTime,
    pub loaded_at: SystemTime,
}

pub trait InstructionPackLoader: Send + Sync {
    fn load(&self, role: &RoleConfig, root: &Path) -> Result<InstructionPack, InstructionLoadError>;
    fn invalidate(&self, role: &RoleName);
}
```

Cache invalidation hooks into the existing workflow reload signal: when
`WorkflowLoader::from_path` is called again, every previously cached
pack is invalidated and re-loaded.

Secret redaction runs over instruction content before any logging or
event emission. The redactor is a small allow-list around the existing
`config::redact` helpers, applied at the boundary
(`emit_event(InstructionPackLoaded {...})` and the prompt-preview CLI).

#### 4.2.2 New module: `role_catalog.rs`

Owns the projection from `WorkflowConfig` into the platform-lead
catalog:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCatalog {
    pub entries: Vec<RoleCatalogEntry>,
    pub source_summary: RoleCatalogSourceSummary, // for warnings
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCatalogEntry {
    pub name: RoleName,
    pub kind: RoleKind,
    pub description: Option<String>,
    pub agent_profile: Option<String>,
    pub agent_backend: Option<AgentBackend>,
    pub max_concurrent: Option<u32>,
    pub tools: Vec<String>,
    pub owns: Vec<String>,
    pub does_not_own: Vec<String>,
    pub requires: Vec<String>,
    pub handoff_expectations: Vec<String>,
    pub routing_hints: RoleRoutingHints,
    pub matching_global_rules: Vec<RoutingRuleSummary>,
    pub instruction_pack_summary: InstructionPackSummary,
}

pub struct RoleCatalogBuilder<'a> {
    pub workflow: &'a WorkflowConfig,
    pub instruction_packs: &'a InstructionPackBundle,
    pub current_role: &'a RoleName,
}

impl RoleCatalogBuilder<'_> {
    pub fn build(self) -> RoleCatalog;
}
```

Inclusion rules implement SPEC v4 §6 exactly:

- `RoleKind::Specialist` → included by default;
- `RoleKind::Reviewer` → included only when workflow allows
  reviewer-as-child (a new `decomposition.allow_reviewer_children`
  flag, defaulting to `true` to match SPEC's "when review can be
  requested as child/follow-up work");
- `RoleKind::Operator` → included only when
  `decomposition.allow_operator_children` is true (defaulting to
  `true`);
- `RoleKind::QaGate` → excluded unless
  `decomposition.allow_manual_qa_children` is explicitly true;
- `RoleKind::IntegrationOwner` other than the current role → excluded
  unless `decomposition.allow_nested_integration_owners` is explicitly
  true;
- `RoleKind::Custom` → included by default; operators opt out via the
  same per-kind flag pattern if needed.

The builder is a pure function over `WorkflowConfig` and the
instruction-pack bundle — same input → same catalog. Tests assert
deterministic ordering (alphabetical by role name, with the current
role excluded).

`InstructionPackSummary` is a concise extract — typically the first
non-empty paragraph of `role_prompt`, plus the `owns`/`does_not_own`
bullets. Full SOUL files are never embedded in the catalog (SPEC §10
non-goal).

#### 4.2.3 Extended module: `prompt.rs` → typed sections

`PromptContext` already exists with strict `{{path}}` rendering. v4
extends the typed model with explicit section discriminants and a
deterministic assembler.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSection {
    GlobalWorkflow { content: String, source: PromptSource },
    RolePrompt     { content: String, source: PromptSource },
    RoleSoul       { content: String, source: PromptSource },
    AgentSystem    { content: String, source: PromptSource },
    IssueContext   { content: String, source: PromptSource },
    ParentChildGraph { content: String, source: PromptSource },
    Blockers       { content: String, source: PromptSource },
    Workspace      { content: String, source: PromptSource },
    AcceptanceCriteria { content: String, source: PromptSource },
    RoleCatalog    { content: String, source: PromptSource },
    DecompositionPolicy { content: String, source: PromptSource },
    OutputSchema   { content: String, source: PromptSource },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSource {
    pub kind: PromptSourceKind, // WorkflowBody | InstructionFile { path, hash } | AgentProfile | KernelDerived
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledPrompt {
    pub sections: Vec<PromptSection>, // already in canonical order
    pub rendered: String,
    pub provenance: Vec<PromptSource>,
}

pub fn assemble_specialist_prompt(...) -> Result<AssembledPrompt, AssemblyError>;
pub fn assemble_decomposition_prompt(...) -> Result<AssembledPrompt, AssemblyError>;
pub fn assemble_qa_prompt(...) -> Result<AssembledPrompt, AssemblyError>;
```

Canonical orderings (SPEC v4 §5) are enforced by the assembler, not by
caller convention. Each assembler is a pure function; the only side
effect is provenance accumulation.

The strict `{{path}}` renderer is reused for placeholder expansion
inside each section's content. The new fail-loud rule is enforced at
the assembler boundary: an unknown placeholder is `AssemblyError`, not
a silently retained literal.

#### 4.2.4 Run metadata

`Run` (or its successor record) gains an `instruction_provenance:
Vec<PromptSource>` field stored as JSON on the `runs` row (existing
`runs` table; see §4.3). This is what `symphony status` and
`symphony issue graph` render to show "this run used
`platform_lead/AGENTS.md@<hash>`".

### 4.3 `symphony-state`

Schema impact is minimal. The existing `runs` table has a
`result_summary TEXT` column and an open JSON convention in adjacent
tables. v4 adds one column:

```sql
-- v4: prompt provenance is durable evidence of which doctrine fired.
ALTER TABLE runs
    ADD COLUMN instruction_provenance TEXT;
```

`instruction_provenance` is a JSON array of `PromptSource` rows. It is
nullable because legacy / non-v4 paths still write `runs` rows without
provenance during the transitional window — once the legacy
`render_prompt` path is removed, a future migration may make it
`NOT NULL`.

No new tables are required. Instruction-pack content is **not**
persisted in the database; the canonical store stays the file-backed
doctrine plus the `hash` recorded in provenance. Git tracks the file
history.

### 4.4 `symphony-cli`

#### 4.4.1 Removing the legacy `render_prompt`

`crates/symphony-cli/src/run.rs::render_prompt` (line 734) is the
legacy issue-field-only renderer. The v4 production dispatch path is
rerouted through `symphony_core::prompt::assemble_*_prompt`. The
function is kept (with `#[deprecated]`) only for the existing tests
that document v1 substitution behavior; production callers no longer
reach it.

#### 4.4.2 New CLI surfaces

Two operator surfaces, both purely diagnostic:

- `symphony prompt preview --role <name> [--issue <id>]` — assemble and
  print the effective prompt for a role/run without launching the
  agent. Redacts secrets through the same path as event emission.
- `symphony catalog show` — print the platform-lead catalog as it
  would render for the current workflow's integration owner. `--json`
  emits the typed `RoleCatalog` for tooling.

Existing `symphony status` learns to render
`runs.instruction_provenance` so an operator can see which doctrine
each run used.

### 4.5 Fixtures

`tests/fixtures/sample-workflow/` grows a `.symphony/roles/` directory
with `AGENTS.md` and `SOUL.md` per role for at least the platform
lead, QA, a backend specialist, and a frontend/TUI specialist. The
existing `WORKFLOW.md` fixture is extended with structured
`assignment` blocks for those roles. These fixtures are the
ground-truth test inputs for the catalog builder snapshot tests.

## 5. Doctrine Layout on Disk

```text
<workflow_root>/
├── WORKFLOW.md                          # roles, agents, routing, assignment metadata
└── .symphony/
    └── roles/
        ├── platform_lead/
        │   ├── AGENTS.md                # role_prompt
        │   └── SOUL.md                  # soul
        ├── qa/
        │   ├── AGENTS.md
        │   └── SOUL.md
        ├── elixir_otp_engineer/
        │   ├── AGENTS.md
        │   └── SOUL.md
        └── tui_engineer/
            ├── AGENTS.md
            └── SOUL.md
```

The `.symphony/roles/` location is convention, not contract. The
contract is "paths in `RoleInstructionConfig` are workflow-root-relative
and stay inside the root". Operators may organize files however they
like as long as the validation gates pass.

## 6. Prompt Assembly

### 6.1 Specialist run (SPEC v4 §5)

```text
section 1  GlobalWorkflow        from WORKFLOW.md body
section 2  RolePrompt            from <role>/AGENTS.md
section 3  RoleSoul              from <role>/SOUL.md
section 4  AgentSystem           from agents.<profile>.system_prompt   (optional)
section 5  IssueContext          kernel-derived (current child issue)
section 6  ParentChildGraph      kernel-derived (parent + sibling status, dependency edges)
section 7  Blockers              kernel-derived (open blockers gating this child)
section 8  Workspace             kernel-derived (workspace claim)
section 9  AcceptanceCriteria    kernel-derived (per-child)
section 10 OutputSchema          handoff envelope schema
```

Each section is a `PromptSection` variant; the assembler concatenates
them with section delimiters and records a `PromptSource` per section.
The assembler refuses to emit if any *required* section is empty
(GlobalWorkflow, RolePrompt for v4 specialists, IssueContext,
OutputSchema).

### 6.2 Platform lead decomposition run (SPEC v4 §5)

```text
section 1  GlobalWorkflow
section 2  RolePrompt            (platform-lead)
section 3  RoleSoul              (platform-lead)
section 4  IssueContext          (parent issue)
section 5  DecompositionPolicy   from decomposition.* config
section 6  RoleCatalog           generated projection
section 7  ParentChildGraph      dependency / blocker edge format
section 8  OutputSchema          decomposition proposal schema
```

The platform lead's prompt explicitly **does not** include other
specialists' SOUL files. Each catalog entry carries a concise
instruction-pack summary plus the structured assignment metadata; that
is the entire peer-doctrine surface.

### 6.3 QA run (SPEC v4 §5)

```text
section 1  GlobalWorkflow
section 2  RolePrompt            (qa)
section 3  RoleSoul              (qa)
section 4  IssueContext          (integrated branch / draft PR context)
section 5  AcceptanceCriteria    (acceptance trace)
section 6  ParentChildGraph      (child handoffs + known blockers)
section 7  CiContext             (build/check status, when available)
section 8  OutputSchema          (qa verdict schema)
```

QA is treated as a gate over integrated output, not as a normal child
implementer. The QA prompt assembler refuses to run for an issue that
has not produced an integration record (matching the existing v2 QA
gate semantics in `crates/symphony-core/src/qa.rs`).

## 7. Loading Lifecycle

```text
WorkflowLoader::from_path
        │
        ├─ parse front matter → WorkflowConfig
        ├─ parse markdown body → prompt_template (global workflow prompt)
        │
        └─▶ InstructionPackLoader::load_all
                    │
                    ├─ for each role with instructions:
                    │     · canonicalize path → assert inside workflow root
                    │     · read file → hash (sha256) → mtime → loaded_at
                    │     · cache by (role, path)
                    │
                    └─▶ InstructionPackBundle { packs: BTreeMap<RoleName, InstructionPack> }
```

The loader returns a `LoadedWorkflowV4` (extension of the existing
`LoadedWorkflow`):

```rust
pub struct LoadedWorkflowV4 {
    pub source_path: PathBuf,
    pub config: WorkflowConfig,
    pub prompt_template: String,
    pub instruction_packs: InstructionPackBundle,  // new in v4
}
```

Adding a new struct rather than mutating `LoadedWorkflow` lets the v2
loader keep working while v4 callers consume the richer envelope.

## 8. Failure Modes

### 8.1 Missing required instruction file

- **Detected at:** workflow load (`WorkflowLoader::from_path`).
- **Behavior:** `WorkflowLoadError::InstructionFileMissing { role, path, source }`.
- **Operator outcome:** dispatch never starts; `symphony validate` and
  `symphony run` both fail with the typed error mapped to a stable
  code.

### 8.2 Path escape

- **Detected at:** workflow load. Canonicalization compares against the
  canonicalized workflow root prefix.
- **Behavior:** `WorkflowLoadError::InstructionPathEscapesRoot { role, path }`.
- **Why:** prevents a malicious or accidental `../../etc/passwd` from
  leaking into a prompt.

### 8.3 Unknown placeholder in a section

- **Detected at:** `assemble_*_prompt`.
- **Behavior:** `AssemblyError::UnknownPlaceholder { section, path }`.
- **Operator outcome:** dispatch fails before the agent process starts;
  the error names the offending section so the operator knows which
  file to fix.

### 8.4 Catalog has no eligible child-owning role

- **Detected at:** workflow validate (when `decomposition.enabled`).
- **Behavior:** `ConfigValidationError::NoEligibleChildOwningRoles`.
- **Operator outcome:** workflow refuses to load. v4 will not silently
  let the platform lead decompose into a vacuum.

### 8.5 Terse-only specialist role

- **Detected at:** workflow validate.
- **Behavior:** warning event emitted, workflow still loads. SPEC v4
  §7 lists this as SHOULD-warn rather than MUST-fail.

## 9. ADRs

### 2026-05-09 — File-backed instructions, with hash-based provenance

Context: SPEC v4 §4 wants reviewable, reusable role doctrine. Two
shapes were considered: (a) inline strings under
`RoleConfig.role_prompt`/`RoleConfig.soul`; (b) file paths the loader
reads.

Decision: file paths. Inline strings make `WORKFLOW.md` unmanageable
once doctrine grows past a few lines and force every doctrine edit
through the YAML escaper. File paths keep the YAML small and let
operators review doctrine as Markdown in PRs.

Consequence: the loader gains path-escape and existence validation
gates and a small cache. Provenance carries the file's SHA-256 hash so
two runs against the same role text produce the same provenance, and a
doctrine change is observable as a hash change in `runs.instruction_provenance`.

### 2026-05-09 — Catalog is a projection, not a separate file

Context: Paperclip uses per-role `ASSIGNMENT.md` files. SPEC v4 §4.3
explicitly forbids that pattern: the platform-lead catalog must come
from `WORKFLOW.md`.

Decision: build the catalog as a pure function over
`WorkflowConfig.roles`, `WorkflowConfig.agents`, `RoutingConfig.rules`,
and the instruction-pack summaries. No `ASSIGNMENT.md` is read at
runtime, ever.

Consequence: `WORKFLOW.md` carries every assignment knob the lead
needs. The new `RoleAssignmentMetadata` block is the load-bearing
surface; the existing `description` is preserved as a fallback and
emits a warning when it's the only available data.

### 2026-05-09 — Role-local routing hints are advisory, not authoritative

Context: SPEC v4 wants role-local routing metadata (`paths_any`,
`labels_any`, etc.) so the platform lead can see "this role usually
handles `lib/**`". v2 already has `RoutingConfig::rules` for
deterministic dispatch. The risk is two parallel routing systems that
drift or contradict.

Decision: `RoleRoutingHints` is **advisory**, rendered into the catalog
as text. Deterministic dispatch continues to read `RoutingConfig::rules`
exclusively. The catalog builder cross-references both: when a global
rule targets a role, that rule appears in the role's catalog entry as
`matching_global_rules` so the lead sees the authoritative routing
rather than guessing from hints.

Consequence: the kernel keeps a single source of dispatch truth. The
operator can write hints freely without changing dispatch behavior, and
the validator emits a warning if the hints contradict existing global
rules badly enough to mislead the lead.

### 2026-05-09 — Specialists never see other specialists' SOUL files

Context: tempting to dump every role's full doctrine into the platform
lead's prompt and into specialist runs as "shared context". SPEC v4 §10
explicitly excludes this.

Decision: a specialist prompt only embeds the current role's
`role_prompt` and `soul`. The platform lead prompt embeds its own
doctrine plus a *summary* per peer (assignment metadata + first
paragraph of role prompt), never the peer's SOUL.

Consequence: prompt token usage stays bounded and roles cannot
cross-contaminate one another's doctrine. The catalog builder is
responsible for the "summary" extract, which is deterministic so tests
can snapshot it.

### 2026-05-09 — Strict prompt assembly replaces `run.rs::render_prompt`

Context: the legacy `render_prompt` in `symphony-cli/src/run.rs` is
permissive (unknown `{{...}}` left as literal text) and issue-only
(no role, no SOUL, no catalog). SPEC v4 §9 requires the legacy path to
no longer be the production renderer.

Decision: route production dispatch through
`symphony_core::prompt::assemble_*_prompt`, which uses the existing
strict `{{path}}` renderer for placeholder expansion and treats unknown
placeholders as a fail-loud `AssemblyError`. Keep the legacy function
under `#[deprecated]` for the v1-substitution tests only.

Consequence: misspelled placeholders fail at build time, not at agent
runtime. A future cleanup deletes the deprecated function once the
legacy tests are migrated.

### 2026-05-09 — Provenance lives on `runs`, not in a new table

Context: instruction-pack provenance (which file, which hash, when
loaded) needs to survive restarts so `symphony status` can show "this
run used X@hash". Two shapes were considered: (a) a new
`run_prompt_sources` table joined on `run_id`; (b) a JSON column on
`runs`.

Decision: JSON column. The provenance is a small per-run snapshot
read whole or not at all; a join would force every run-read path to
touch two tables forever for a payload that is never queried by sub-
field. The pattern matches existing JSON columns on `runs`,
`workspace_claims`, and `qa_verdicts`.

Consequence: one additive migration column. Future analytics (e.g.
"which runs used SOUL@oldhash") can be derived by reading the column,
matching the precedent set by `qa_verdicts.evidence`.
