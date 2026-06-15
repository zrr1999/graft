mod candidate;
mod config;
mod constraint;
mod daemon_client;
mod daemon_wire;
mod explain_catalog;
mod patch_query;
mod presentation;
mod promotion;
mod registry;
mod repo;
mod requirements;
mod roto_constraints;
mod routing;
mod scratch;
mod state_runtime;
mod validation;
mod view;
mod workspace;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, anyhow, bail};
use candidate::{CandidateCommand, run_candidate_command};
use clap::{Parser, Subcommand, ValueEnum};
#[cfg(test)]
use config::load_constraint_defs;
#[cfg(test)]
use config::write_constraint_lock;
use config::{GraftConfig, load_graft_config};
use constraint::{ConstraintCommand, run_constraint_command};
#[cfg(test)]
use daemon_wire::result_to_envelope;
use daemon_wire::{
    run_via_daemon, run_via_daemon_with_argv, run_workspace_registry_write_via_daemon,
};
#[cfg(test)]
use explain_catalog::promote_requirement_explain_line;
use explain_catalog::{build_concept_catalog, plan_labels_or_core_only};
use graft_core::{
    AdmissionSummary, ApplicationRef, Change, EvidenceRecord, EvidenceResult, FileChangeKind,
    GraftCandidate, PatchRecord, PatchRelation, PatchRelationKind, PlanId, PromotionRecord,
    Provenance, StateId, TreeEntry, TreeSnapshot, application_from_change, candidate_id, patch_id,
    promotion_id, relation_id,
};
use graft_promote::GixBackend;
use graft_store::{
    DEFAULT_WORKSPACE_ID, GraftStore, RegistryStore, ResolvedApplication, StoreError,
    WorkspaceDiscovery, normalize_workspace_path,
};
#[cfg(test)]
use graft_store::{WorkspaceKind, local_workspace_id_for_root};
use graft_sync::{DivergencePolicy as SyncDivergencePolicy, GraftSyncTransport, SyncOptions};
use patch_query::{
    constraint_id_matches, incoming_command, list_candidate_summaries, run_patch_list_command,
    search_patches, warn_if_constraint_unknown,
};
use presentation::{
    change_view_for_application, evidence_view, next_actions_for_candidate, next_search_actions,
    promotion_view, state_label, summarize_candidate, summarize_candidate_with_evidence,
    summarize_patch_with_evidence,
};
#[cfg(test)]
use promotion::materialize_ref_name;
use promotion::{
    ensure_materialized_commit, git_ref_component_for_patch_id, promote_next_action,
    target_snapshot_for_patch, validate_promote_ref_args,
};
use registry::{RegistryCommand, run_registry_command};
use repo::{RepoCommand, base_snapshot_for_state, resolve_base_state, run_repo_command};
use requirements::{
    admission_required_constraint, constraint_from_plans, constraint_matches_request,
    constraint_primitives, needs_revalidation_or, plan_label,
    promotion_requirement_plan_with_target, validation_constraint_with_base,
};
use routing::{
    DaemonCliExecRouter, PatchCommandRoute, TopLevelRoute, command_is_gc,
    command_skips_workspace_init_check, command_uses_cwd_directly, route_patch_command,
    route_top_level_command,
};
use scratch::{ScratchCommand, run_scratch_command, run_scratch_status};
#[cfg(test)]
use state_runtime::materialize_worktree_path;
use state_runtime::{materialize_state, object_diff_summary, run_in_state};
use time::OffsetDateTime;
use validation::{
    ensure_change_integrity, evidence_for_current_verifiers, validate_candidate, validate_patch,
};
use view::{CommandEnvelope, print_human};
use workspace::{
    gc_apply_daemon_argv, init_workspace_files, modernize_legacy_gc_apply_message,
    run_attach_command, run_detach_command, run_doctor_command, run_gc, run_init_command,
    run_ps_command, run_workspace_command, workspace_status,
};

#[derive(Parser, Debug)]
#[command(
    name = "graft",
    about = "Constraint-aware patch runtime for agent changes",
    long_about = "Draft, validate and admit patch candidates with explicit constraint obligations and evidence, isolated from .git/ until you explicitly materialize or promote."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(
        long,
        global = true,
        help = "Emit machine-readable JSON instead of the human Hole Report"
    )]
    json: bool,

    #[arg(
        long,
        global = true,
        default_value = ".",
        help = "Working directory whose .graft/ store and worktree are addressed"
    )]
    cwd: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Clone a Graft object store into a new empty workspace
    Get {
        /// Remote storage directory or bundle source to copy from
        remote: PathBuf,
        /// Destination directory for the new empty workspace
        dir: PathBuf,
    },
    /// Synchronize public graft objects with a remote object store
    Sync {
        /// Remote storage directory for public graft objects
        remote: Option<PathBuf>,
        #[arg(long, help = "Only fetch remote public objects")]
        fetch_only: bool,
        #[arg(long, help = "Only push local public objects")]
        push_only: bool,
        #[arg(
            long,
            value_enum,
            default_value_t = OnDivergence::Abort,
            help = "Policy for incompatible remote manifest history"
        )]
        on_divergence: OnDivergence,
    },
    /// Manage workspace initialization, attachment, health and garbage collection
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Send scratch operations to graftd over Unix socket (daemon-held editing state)
    Scratch {
        #[command(subcommand)]
        command: ScratchCommand,
        #[arg(long, help = "graftd Unix socket path")]
        socket: Option<PathBuf>,
    },
    /// Manage admitted patches and patch candidates
    Patch {
        #[command(subcommand)]
        command: PatchCommand,
    },
    /// Manage source repo caches used by repo:<id>@<treeish> base refs
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Export or import admitted public objects as a portable bundle
    Bundle {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// Explain a concept id, diagnostic code, or builtin evaluator id
    Explain {
        /// Identifier to explain (e.g. `agent-workflow`, `admit`, `V003`, `changed_paths_any_match`)
        id: String,
    },

    /// Initialize .graft/ store and graft.toml in the current directory
    #[command(hide = true)]
    Init {
        #[arg(
            long,
            help = "Only register an existing initialized workspace in $GRAFT_HOME/registry.toml"
        )]
        register_only: bool,
    },
    /// Attach this cwd to a workspace route, or show attach status
    #[command(hide = true)]
    Attach {
        #[arg(long, help = "Workspace id to attach to; defaults to ws:default")]
        workspace: Option<String>,
        #[arg(long, hide = true, help = "Show current cwd route/discovery status")]
        status: bool,
    },
    /// Detach this cwd route from the local registry
    #[command(hide = true)]
    Detach,
    /// Show global daemon and registry workspace status
    #[command(hide = true)]
    Ps,
    /// Diagnose or repair the machine-local registry
    #[command(hide = true)]
    Doctor {
        #[arg(
            long,
            help = "Rebuild missing registry entries from $GRAFT_HOME/workspaces/*"
        )]
        rebuild_registry: bool,
    },
    /// Clone a Graft object store into a new empty workspace
    #[command(hide = true)]
    Clone {
        /// Remote storage directory or bundle source to copy from
        remote: PathBuf,
        /// Destination directory for the new empty workspace
        dir: PathBuf,
    },
    /// Manage candidate lifecycle operations
    #[command(hide = true)]
    Candidate {
        #[command(subcommand)]
        command: CandidateCommand,
        #[arg(long, help = "graftd Unix socket path")]
        socket: Option<PathBuf>,
    },
    /// List candidates that are not yet admitted
    #[command(hide = true)]
    Candidates {
        #[arg(
            long,
            help = "Show only candidates whose constraint contains this constraint primitive"
        )]
        constraint: Option<String>,
        #[arg(long, help = "Show only candidates with at least one failed evidence")]
        failed: bool,
        #[arg(long, help = "Filter by provenance producer label")]
        producer: Option<String>,
    },
    /// Show identity, change summary, constraints and evidence for a candidate or patch
    #[command(hide = true)]
    Show {
        /// Candidate or patch id to inspect
        id: String,
        #[arg(
            long,
            help = "Include the evidence list with verifier and reason details"
        )]
        evidence: bool,
        #[arg(long, help = "Include the per-file change summary")]
        change: bool,
    },
    /// Run verifiers and produce evidence for an explicit candidate, patch, or change
    #[command(hide = true)]
    Validate {
        /// Candidate, patch, or change id to validate
        id: String,
        #[arg(
            long = "expect",
            help = "Validate this whole-state constraint, for example tests_pass (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
    },
    /// Admit a candidate into the registry once required evidence is present
    #[command(hide = true)]
    Admit {
        /// Candidate id to admit
        id: String,
        #[arg(
            long = "require",
            help = "Add a one-shot admission requirement like tests_pass; append to [admission.required] plus candidate constraints as all_of"
        )]
        required: Vec<String>,
    },
    /// Show cwd route and resolved workspace status
    #[command(hide = true)]
    Status,
    /// Show object-to-object changes between materializable refs
    #[command(hide = true)]
    Diff {
        /// Source state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, or repo:<id>@<treeish>
        from: String,
        /// Target state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, or repo:<id>@<treeish>
        to: String,
    },
    /// Obsolete: cwd is not a managed view and cannot be restored by Graft
    #[command(hide = true)]
    Discard,
    /// Show incoming patch groups from the local public store
    #[command(hide = true)]
    Incoming,
    /// Search admitted patches in the registry by constraint, base or evidence
    #[command(hide = true)]
    Search {
        #[arg(
            long,
            help = "Match patches whose constraint set contains this constraint"
        )]
        constraint: Option<String>,
        #[arg(long, help = "Match patches whose declared base equals this state")]
        base: Option<String>,
        #[arg(
            long,
            help = "Match patches whose provenance producer equals this label"
        )]
        producer: Option<String>,
        #[arg(
            long = "has-evidence",
            help = "Match patches that carry passing evidence for this whole-state constraint"
        )]
        has_evidence: Option<String>,
    },
    /// Compose two sequential patches into a new candidate (target(first) == base(second))
    #[command(hide = true)]
    Compose {
        /// First patch id (its target becomes the composition's base)
        first: String,
        /// Second patch id (its base must equal first's target)
        second: String,
        #[arg(
            long = "expect",
            help = "Whole-state constraint the composed candidate should later satisfy (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
        #[arg(long, help = "Run validators on the composed candidate immediately")]
        validate: bool,
    },
    /// Re-base an admitted patch onto a new state, producing a fresh candidate
    #[command(hide = true)]
    Migrate {
        /// Patch id to migrate
        id: String,
        #[arg(
            long,
            default_value = "HEAD",
            help = "Target state to migrate the patch onto"
        )]
        onto: String,
        #[arg(
            long = "expect",
            help = "Whole-state constraint the migrated candidate should later satisfy (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
        #[arg(long, help = "Run validators on the migrated candidate immediately")]
        validate: bool,
    },
    /// Produce a candidate that reverts an admitted patch
    #[command(hide = true)]
    Revert {
        /// Patch id to revert
        id: String,
        #[arg(
            long = "expect",
            help = "Whole-state constraint the revert candidate should later satisfy (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
        #[arg(long, help = "Run validators on the revert candidate immediately")]
        validate: bool,
    },
    /// Run a command inside a temporary materialized state; writes are discarded
    Run {
        /// State ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, or repo:<id>@<treeish>
        state: String,
        #[arg(
            long,
            value_name = "PATH",
            help = "Relative directory inside the materialized state root to run from"
        )]
        cwd: Option<PathBuf>,
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            help = "Command and arguments to run after --"
        )]
        command: Vec<String>,
    },
    /// Materialize any state ref into an isolated inspection state
    #[command(hide = true)]
    Materialize {
        /// State ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, or repo:<id>@<treeish>
        id: String,
        #[arg(
            long,
            help = "Plan the materialization but do not write the inspection state or Git objects"
        )]
        dry_run: bool,
        #[arg(
            long,
            hide = true,
            help = "Accepted for compatibility; materialize no longer writes cwd"
        )]
        discard: bool,
        #[arg(
            long,
            hide = true,
            help = "Unsupported; materialize only writes inspection states"
        )]
        as_commit: bool,
        #[arg(
            long = "ref",
            hide = true,
            help = "Unsupported; promote writes Git refs"
        )]
        ref_name: Option<String>,
    },
    /// Promote an admitted patch target state to a Git branch, PR or release
    #[command(hide = true)]
    Promote {
        /// Patch id to promote
        id: String,
        #[arg(long, help = "Target branch or configured promote target to update")]
        to: String,
        #[arg(
            long,
            help = "Branch/ref to update when --to names a configured promote target"
        )]
        branch: Option<String>,
        #[arg(long, help = "Skip the dry-run gate and apply the promotion")]
        yes: bool,
        #[arg(
            long = "require",
            help = "Constraint that must have passing evidence before promotion (repeatable; repeats compose as all_of)"
        )]
        required: Vec<String>,
        #[arg(
            long,
            help = "Open a GitHub Pull Request via gh after the branch is updated"
        )]
        pr: bool,
        #[arg(
            long,
            help = "Also create a release tag ref pointing at the promoted commit"
        )]
        release: Option<String>,
        #[arg(long, help = "Pull request title (when --pr is set)")]
        title: Option<String>,
        #[arg(long, help = "Pull request body (when --pr is set)")]
        body: Option<String>,
        #[arg(long, help = "Pull request head branch (when --pr is set)")]
        head: Option<String>,
    },
    /// Manage explicit constraint definitions and their lockfile
    #[command(hide = true)]
    Constraint {
        #[command(subcommand)]
        command: ConstraintCommand,
    },
    /// Export or import admitted public objects as a portable bundle
    #[command(hide = true)]
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// Inspect or query private candidate state
    #[command(hide = true)]
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Rebuild missing local evidence bodies referenced by public evidence_refs
    #[command(hide = true)]
    VerifyPending {
        #[arg(long)]
        patch: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// List evidence records attached to a candidate or patch
    #[command(hide = true)]
    Evidence {
        /// Candidate or patch subject id
        subject: String,
    },
    /// Collect unreachable .graft objects and stale registry paths; asks before applying by default
    #[command(hide = true)]
    Gc {
        #[arg(
            long,
            help = "Delete unreachable objects and stale registry paths instead of only reporting them"
        )]
        apply: bool,
        #[arg(long, help = "Clear only store/derived evidence bodies")]
        derived_only: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OnDivergence {
    Abort,
    KeepRemote,
}

impl From<OnDivergence> for SyncDivergencePolicy {
    fn from(value: OnDivergence) -> Self {
        match value {
            OnDivergence::Abort => Self::Abort,
            OnDivergence::KeepRemote => Self::KeepRemote,
        }
    }
}

#[derive(Subcommand, Debug)]
pub(crate) enum WorkspaceCommand {
    /// Initialize .graft/ store and graft.toml in the current directory
    Init {
        #[arg(
            long,
            help = "Only register an existing initialized workspace in $GRAFT_HOME/registry.toml"
        )]
        register_only: bool,
    },
    /// Show cwd route and resolved workspace status
    Status,
    /// Attach this cwd to a workspace route, or show attach status
    Attach {
        #[arg(long, help = "Workspace id to attach to; defaults to ws:default")]
        workspace: Option<String>,
        #[arg(long, help = "Show current cwd route/discovery status")]
        status: bool,
    },
    /// Detach this cwd route from the local registry
    Detach,
    /// Show global daemon and registry workspace status
    Ps,
    /// Diagnose or repair the machine-local registry
    Doctor {
        #[arg(
            long,
            help = "Rebuild missing registry entries from $GRAFT_HOME/workspaces/*"
        )]
        rebuild_registry: bool,
    },
    /// Collect unreachable .graft objects and stale registry paths; asks before applying by default
    Gc {
        #[arg(
            long,
            help = "Delete unreachable objects and stale registry paths instead of only reporting them"
        )]
        apply: bool,
        #[arg(long, help = "Clear only store/derived evidence bodies")]
        derived_only: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PatchCommand {
    /// List admitted patches by default, candidates with --candidates, or both with --all
    List {
        #[arg(long, help = "List candidate patches that are not yet admitted")]
        candidates: bool,
        #[arg(long, help = "List both admitted patches and unadmitted candidates")]
        all: bool,
        #[arg(long, help = "Filter by constraint primitive name or id")]
        constraint: Option<String>,
        #[arg(long, help = "Filter by provenance producer label")]
        producer: Option<String>,
    },
    /// Create a candidate from an existing scratch id
    FromScratch(crate::candidate::CandidateFromScratchArgs),
    /// Show identity, change summary, constraints and evidence for a candidate or patch
    Show {
        /// Candidate or patch id to inspect
        id: String,
        #[arg(
            long,
            help = "Include the evidence list with verifier and reason details"
        )]
        evidence: bool,
        #[arg(long, help = "Include the per-file change summary")]
        change: bool,
    },
    /// Run verifiers and produce evidence for an explicit candidate, patch, or change
    Validate {
        /// Candidate, patch, or change id to validate
        id: String,
        #[arg(
            long = "expect",
            help = "Validate this whole-state constraint, for example tests_pass (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
    },
    /// Admit a candidate into the registry once required evidence is present
    Admit {
        /// Candidate id to admit
        id: String,
        #[arg(
            long = "require",
            help = "Add a one-shot admission requirement like tests_pass; append to [admission.required] plus candidate constraints as all_of"
        )]
        required: Vec<String>,
    },
    /// Show incoming patch groups from the local public store
    Incoming,
    /// Search admitted patches in the registry by constraint, base or evidence
    Search {
        #[arg(
            long,
            help = "Match patches whose constraint set contains this constraint"
        )]
        constraint: Option<String>,
        #[arg(long, help = "Match patches whose declared base equals this state")]
        base: Option<String>,
        #[arg(
            long,
            help = "Match patches whose provenance producer equals this label"
        )]
        producer: Option<String>,
        #[arg(
            long = "has-evidence",
            help = "Match patches that carry passing evidence for this whole-state constraint"
        )]
        has_evidence: Option<String>,
    },
    /// Show object-to-object changes between materializable refs
    Diff {
        /// Source state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, or repo:<id>@<treeish>
        from: String,
        /// Target state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, or repo:<id>@<treeish>
        to: String,
    },
    /// Compose two sequential patches into a new candidate (target(first) == base(second))
    Compose {
        /// First patch id (its target becomes the composition's base)
        first: String,
        /// Second patch id (its base must equal first's target)
        second: String,
        #[arg(
            long = "expect",
            help = "Whole-state constraint the composed candidate should later satisfy (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
        #[arg(long, help = "Run validators on the composed candidate immediately")]
        validate: bool,
    },
    /// Re-base an admitted patch onto a new state, producing a fresh candidate
    Migrate {
        /// Patch id to migrate
        id: String,
        #[arg(
            long,
            default_value = "HEAD",
            help = "Target state to migrate the patch onto"
        )]
        onto: String,
        #[arg(
            long = "expect",
            help = "Whole-state constraint the migrated candidate should later satisfy (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
        #[arg(long, help = "Run validators on the migrated candidate immediately")]
        validate: bool,
    },
    /// Produce a candidate that reverts an admitted patch
    Revert {
        /// Patch id to revert
        id: String,
        #[arg(
            long = "expect",
            help = "Whole-state constraint the revert candidate should later satisfy (repeatable; repeats compose as all_of)"
        )]
        constraint_primitives: Vec<String>,
        #[arg(long, help = "Run validators on the revert candidate immediately")]
        validate: bool,
    },
    /// Materialize an admitted patch target state into an isolated inspection state
    Materialize {
        /// Patch id or patch:<digest> ref to materialize
        id: String,
        #[arg(
            long,
            help = "Plan the materialization but do not write the inspection state"
        )]
        dry_run: bool,
        #[arg(
            long,
            hide = true,
            help = "Accepted for compatibility; materialize no longer writes cwd"
        )]
        discard: bool,
        #[arg(
            long,
            hide = true,
            help = "Unsupported; materialize only writes inspection states"
        )]
        as_commit: bool,
        #[arg(
            long = "ref",
            hide = true,
            help = "Unsupported; promote writes Git refs"
        )]
        ref_name: Option<String>,
    },
    /// Promote a patch to a Git branch, PR or release; the only command that mutates Git refs
    Promote {
        /// Patch id to promote
        id: String,
        #[arg(long, help = "Target branch or configured promote target to update")]
        to: String,
        #[arg(
            long,
            help = "Branch/ref to update when --to names a configured promote target"
        )]
        branch: Option<String>,
        #[arg(long, help = "Skip the dry-run gate and apply the promotion")]
        yes: bool,
        #[arg(
            long = "require",
            help = "Constraint that must have passing evidence before promotion (repeatable; repeats compose as all_of)"
        )]
        required: Vec<String>,
        #[arg(
            long,
            help = "Open a GitHub Pull Request via gh after the branch is updated"
        )]
        pr: bool,
        #[arg(
            long,
            help = "Also create a release tag ref pointing at the promoted commit"
        )]
        release: Option<String>,
        #[arg(long, help = "Pull request title (when --pr is set)")]
        title: Option<String>,
        #[arg(long, help = "Pull request body (when --pr is set)")]
        body: Option<String>,
        #[arg(long, help = "Pull request head branch (when --pr is set)")]
        head: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum CacheCommand {
    /// Search candidates that live in cache (not yet admitted)
    Search {
        #[arg(
            long,
            help = "Match candidates whose constraint contains this constraint primitive"
        )]
        constraint: Option<String>,
        #[arg(long, help = "Match candidates with at least one failed evidence")]
        failed: bool,
    },
}

pub fn main_entry() -> Result<()> {
    let cli = Cli::parse();
    let route = route_top_level_command(&cli.command);
    let envelope = match route {
        TopLevelRoute::Explain => {
            let Command::Explain { id } = &cli.command else {
                bail!("[E_INTERNAL] explain route requires Command::Explain")
            };
            return run_explain(id, cli.json, &cli.cwd);
        }
        TopLevelRoute::GcPrompt { derived_only } if !cli.json => {
            let envelope = run_local(&cli)?;
            print_human(&envelope);
            if prompt_yes_no_default_no("Apply this garbage collection now?")? {
                let apply_cli = Cli {
                    command: Command::Workspace {
                        command: WorkspaceCommand::Gc {
                            apply: true,
                            derived_only,
                        },
                    },
                    json: cli.json,
                    cwd: cli.cwd.clone(),
                };
                let applied = run_gc_apply(&apply_cli)?;
                print_human(&applied);
            } else {
                println!("gc apply skipped");
            }
            return Ok(());
        }
        TopLevelRoute::GcPrompt { .. } | TopLevelRoute::Local => run_local(&cli)?,
        TopLevelRoute::WorkspaceRegistryWrite => run_workspace_registry_write_via_daemon(&cli)?,
        TopLevelRoute::GcApply => run_gc_apply(&cli)?,
        TopLevelRoute::CliExec => run_via_daemon(&cli)?,
    };
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        print_human(&envelope);
    }
    Ok(())
}

/// Execute a graft CLI argv vector inside graftd's process. This is the
/// daemon-owned write path: the frontend has already sent us an argv frame,
/// and the daemon owns the workspace while it runs the same command logic
/// locally instead of spawning another graft process.
pub fn run_daemon_argv_to_value_for_workspace(
    argv: Vec<String>,
    workspace_id: &str,
) -> Result<serde_json::Value> {
    let cli = Cli::try_parse_from(argv)?;
    DaemonCliExecRouter::ensure_supported(&cli.command)?;
    Ok(serde_json::to_value(run_local_with_workspace_id(
        &cli,
        Some(workspace_id),
    )?)?)
}

pub fn resolve_candidate_constraint_primitives(
    store: &GraftStore,
    names: &[String],
) -> Result<Vec<PlanId>> {
    let config = load_graft_config(store)?;
    needs_revalidation_or(&config, names)
}

pub fn workspace_attach_to_value(
    cwd: &Path,
    workspace: Option<&str>,
    status: bool,
) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(run_attach_command(
        cwd, workspace, status,
    )?)?)
}

pub fn workspace_detach_to_value(cwd: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(run_detach_command(cwd)?)?)
}

fn prompt_yes_no_default_no(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(false);
    }
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn run_gc_apply(cli: &Cli) -> Result<CommandEnvelope> {
    let derived_only = match &cli.command {
        Command::Gc {
            apply: true,
            derived_only,
        }
        | Command::Workspace {
            command:
                WorkspaceCommand::Gc {
                    apply: true,
                    derived_only,
                },
        } => *derived_only,
        _ => bail!("[E_INTERNAL] run_gc_apply requires `graft workspace gc --apply`"),
    };
    match discover_optional_workspace_for_gc(&cli.cwd)? {
        Some((workspace_root, _)) => {
            let envelope = run_via_daemon_with_argv(
                cli,
                Some(gc_apply_daemon_argv(&workspace_root, derived_only)),
            )?;
            Ok(modernize_legacy_gc_apply_message(envelope, derived_only))
        }
        None => run_local(cli),
    }
}

fn run_explain(id: &str, json: bool, cwd: &Path) -> Result<()> {
    let concepts = build_concept_catalog(cwd);
    let result = graft_explain::explain::lookup(id, &concepts);
    if json {
        let payload =
            serde_json::to_string_pretty(&result).context("failed to serialize ExplainResult")?;
        println!("{payload}");
    } else {
        print!("{}", graft_explain::explain::render_human(&result));
    }
    if matches!(
        result,
        graft_explain::explain::ExplainResult::Unknown { .. }
    ) {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
fn run_process(
    program: &std::ffi::OsStr,
    args: &[&str],
    cwd: &Path,
    graft_home: Option<&Path>,
) -> Result<String> {
    let mut command = ProcessCommand::new(program);
    command.args(args).current_dir(cwd);
    if let Some(graft_home) = graft_home {
        command.env("GRAFT_HOME", graft_home);
    }
    let output = command
        .output()
        .with_context(|| format!("run {}", program.to_string_lossy()))?;
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        bail!(
            "command failed ({} {}):\n{}",
            program.to_string_lossy(),
            args.join(" "),
            combined
        );
    }
    Ok(combined)
}

fn run_local(cli: &Cli) -> Result<CommandEnvelope> {
    run_local_with_workspace_id(cli, None)
}

fn discover_optional_workspace_for_gc(cwd: &Path) -> Result<Option<(PathBuf, Option<String>)>> {
    match WorkspaceDiscovery::from_env().discover(cwd) {
        Ok(location) => Ok(Some((
            location.root().to_path_buf(),
            location.id().map(str::to_string),
        ))),
        Err(StoreError::NoWorkspace { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn run_candidate_from_scratch_command(
    store: &GraftStore,
    workspace_root: &Path,
    workspace_id: &str,
    socket: Option<&Path>,
    args: &crate::candidate::CandidateFromScratchArgs,
) -> Result<CommandEnvelope> {
    let command = CandidateCommand::FromScratch(args.clone());
    let mut envelope = run_candidate_command(workspace_root, workspace_id, socket, &command)?;
    if !args.validates_on_create() {
        return Ok(envelope);
    }

    let candidate_id = envelope.candidate_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "[E_CANDIDATE_RESULT_CONTRACT] from-scratch result omitted candidate id; cannot validate --expect constraints"
        )
    })?;
    let candidate = store
        .read_candidate(&candidate_id)
        .with_context(|| format!("read candidate record {candidate_id}"))?;
    let evidence_records = validate_candidate(store, &candidate, &[])?;
    envelope.evidence_ids = evidence_records
        .iter()
        .map(|record| record.id.to_string())
        .collect();
    envelope.evidence = evidence_records.iter().map(evidence_view).collect();
    envelope.next_actions = next_actions_for_candidate(&candidate, &evidence_records);
    envelope.cache_changed = true;
    Ok(envelope)
}

fn run_patch_command(
    command: &PatchCommand,
    cli: &Cli,
    routed_workspace_id: Option<&str>,
    store: &GraftStore,
    workspace_root: &Path,
    workspace_id: Option<&str>,
) -> Result<CommandEnvelope> {
    match route_patch_command(command) {
        PatchCommandRoute::List {
            candidates,
            all,
            constraint,
            producer,
        } => run_patch_list_command(store, candidates, all, constraint, producer),
        PatchCommandRoute::FromScratch(args) => {
            let workspace_id = workspace_id.ok_or_else(|| {
                anyhow::anyhow!(
                    "[E_NO_WORKSPACE_ID] typed daemon op requires a resolved workspace_id"
                )
            })?;
            run_candidate_from_scratch_command(store, workspace_root, workspace_id, None, args)
        }
        PatchCommandRoute::Show {
            id,
            evidence,
            change,
        } => show_record(store, id, evidence, change),
        PatchCommandRoute::Incoming => incoming_command(store),
        PatchCommandRoute::TopLevelAlias(command) => {
            run_patch_top_level_alias(cli, routed_workspace_id, command)
        }
    }
}

fn run_patch_top_level_alias(
    cli: &Cli,
    routed_workspace_id: Option<&str>,
    command: Command,
) -> Result<CommandEnvelope> {
    run_local_with_workspace_id(
        &Cli {
            command,
            json: cli.json,
            cwd: cli.cwd.clone(),
        },
        routed_workspace_id,
    )
}

fn run_local_with_workspace_id(
    cli: &Cli,
    routed_workspace_id: Option<&str>,
) -> Result<CommandEnvelope> {
    let workspace_location = if command_uses_cwd_directly(&cli.command) {
        None
    } else if let Some(workspace_id) = routed_workspace_id {
        Some(resolve_registered_workspace(workspace_id)?)
    } else if command_is_gc(&cli.command) {
        discover_optional_workspace_for_gc(&cli.cwd)?
    } else {
        let location = WorkspaceDiscovery::from_env().discover(&cli.cwd)?;
        Some((
            location.root().to_path_buf(),
            location.id().map(str::to_string),
        ))
    };
    let workspace_root = workspace_location
        .as_ref()
        .map(|(root, _)| root.clone())
        .unwrap_or_else(|| cli.cwd.clone());
    let workspace_id = workspace_location.as_ref().and_then(|(_, id)| id.clone());
    let store = GraftStore::open(&workspace_root);
    if !command_skips_workspace_init_check(&cli.command) {
        ensure_workspace_initialized(&store)?;
    }
    LocalCommandRouter {
        cli,
        routed_workspace_id,
        store: &store,
        workspace_root: &workspace_root,
        workspace_id: workspace_id.as_deref(),
    }
    .execute()
}

struct LocalCommandRouter<'a> {
    cli: &'a Cli,
    routed_workspace_id: Option<&'a str>,
    store: &'a GraftStore,
    workspace_root: &'a Path,
    workspace_id: Option<&'a str>,
}

impl LocalCommandRouter<'_> {
    fn execute(self) -> Result<CommandEnvelope> {
        let Self {
            cli,
            routed_workspace_id,
            store,
            workspace_root,
            workspace_id,
        } = self;
        match &cli.command {
            Command::Get { remote, dir } => clone_command(remote, dir),
            Command::Workspace {
                command: WorkspaceCommand::Status,
            } => workspace_status(&cli.cwd),
            Command::Workspace { command } => run_workspace_command(command, &cli.cwd, store),
            Command::Patch { command } => run_patch_command(
                command,
                cli,
                routed_workspace_id,
                store,
                workspace_root,
                workspace_id,
            ),
            Command::Bundle { command } => run_registry_command(store, &cli.cwd, command),
            Command::Init { register_only } => run_init_command(store, *register_only),
            Command::Attach { workspace, status } => {
                run_attach_command(&cli.cwd, workspace.as_deref(), *status)
            }
            Command::Detach => run_detach_command(&cli.cwd),
            Command::Ps => run_ps_command(),
            Command::Doctor { rebuild_registry } => run_doctor_command(*rebuild_registry),
            Command::Clone { remote, dir } => clone_command(remote, dir),
            Command::Constraint { command } => run_constraint_command(store, command),
            Command::Candidate { command, socket } => {
                let workspace_id = workspace_id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "[E_NO_WORKSPACE_ID] typed daemon op requires a resolved workspace_id"
                    )
                })?;
                match command {
                    CandidateCommand::FromScratch(args) => run_candidate_from_scratch_command(
                        store,
                        workspace_root,
                        workspace_id,
                        socket.as_deref(),
                        args,
                    ),
                }
            }
            Command::Candidates {
                constraint,
                failed,
                producer,
            } => Ok(CommandEnvelope {
                candidates: list_candidate_summaries(store, constraint, *failed, producer)?,
                ..CommandEnvelope::ok()
            }),
            Command::Show {
                id,
                evidence,
                change,
            } => show_record(store, id, *evidence, *change),
            Command::Run {
                state,
                cwd,
                command,
            } => {
                let config = load_graft_config(store)?;
                run_in_state(store, &config, state, cwd.as_deref(), command)
            }
            Command::Validate {
                id,
                constraint_primitives,
            } => match record_ref_kind(id)? {
                RecordRefKind::Candidate => {
                    let candidate = store
                        .read_candidate(id)
                        .with_context(|| format!("read candidate record {id}"))?;
                    let config = load_graft_config(store)?;
                    let validation_constraint = validation_constraint_with_base(
                        &config,
                        constraint_primitives,
                        &candidate.constraint,
                    )?;
                    let evidence_records =
                        validate_candidate(store, &candidate, constraint_primitives)?;
                    let validation_summary =
                        validation_satisfaction_summary(graft_validate::validate_constraint(
                            &graft_validate::ValidationSubject::new(id.clone()),
                            &validation_constraint,
                            &evidence_records,
                        ));
                    Ok(CommandEnvelope {
                        message: Some(format!(
                            "validation completed for {id}; registry unchanged; {validation_summary}"
                        )),
                        candidate_id: Some(id.clone()),
                        evidence_ids: evidence_records
                            .iter()
                            .map(|record| record.id.to_string())
                            .collect(),
                        evidence: evidence_records.iter().map(evidence_view).collect(),
                        cache_changed: true,
                        next_actions: next_actions_for_candidate(&candidate, &evidence_records),
                        ..CommandEnvelope::ok()
                    })
                }
                RecordRefKind::Patch => {
                    let patch = store
                        .read_patch(id)
                        .with_context(|| format!("read patch record {id}"))?;
                    let config = load_graft_config(store)?;
                    let validation_constraint = validation_constraint_with_base(
                        &config,
                        constraint_primitives,
                        &patch.constraint,
                    )?;
                    let evidence_records = validate_patch(store, &patch, constraint_primitives)?;
                    let validation_summary =
                        validation_satisfaction_summary(graft_validate::validate_constraint(
                            &graft_validate::ValidationSubject::new(id.clone()),
                            &validation_constraint,
                            &evidence_records,
                        ));
                    Ok(CommandEnvelope {
                        message: Some(format!(
                            "validation completed for admitted patch {id}; {validation_summary}"
                        )),
                        patch_id: Some(id.clone()),
                        evidence_ids: evidence_records
                            .iter()
                            .map(|record| record.id.to_string())
                            .collect(),
                        evidence: evidence_records.iter().map(evidence_view).collect(),
                        registry_changed: true,
                        git_changed: false,
                        ..CommandEnvelope::ok()
                    })
                }
            },
            Command::Admit { id, required } => {
                store.init_storage()?;
                let candidate = store.read_candidate(id)?;
                let evidence = store.candidate_evidence_records(id)?;
                let config = load_graft_config(store)?;
                ensure_change_integrity(store, &config, &candidate.application)?;
                ensure_candidate_constraint_current(&config, &candidate)?;
                let required_constraint =
                    admission_required_constraint(&config, &candidate, required)?;
                let required = constraint_primitives(&required_constraint);
                let current_evidence =
                    evidence_for_current_verifiers(&config, &required, &evidence, id)?;
                graft_policy::satisfies_subject(id, &required_constraint, &current_evidence)
                    .map_err(render_policy_error)?;
                let mut patch = PatchRecord {
                    id: graft_core::PatchId::new("patch:pending"),
                    application: candidate.application.clone(),
                    constraint: required_constraint.clone(),
                    provenance: candidate.provenance,
                    admission: AdmissionSummary {
                        constraint: required_constraint,
                    },
                };
                patch.id = patch_id(&patch)?;
                store.write_patch(&patch)?;

                let evidence_ids =
                    store.copy_candidate_evidence_index_to_patch(id, patch.id.as_str())?;
                store.remove_candidate_evidence_index(id)?;
                store.remove_candidate(id)?;
                let promoted = store.evidence_records_for_ids(&evidence_ids)?;
                for relation in store.cached_relations_for_subject(id)? {
                    let mut relation = relation;
                    relation.subject = patch.id.to_string();
                    relation.id = relation_id(&relation)?;
                    store.write_relation(&relation)?;
                }

                Ok(CommandEnvelope {
                    message: Some(format!("admitted patch {} from candidate {id}", patch.id)),
                    candidate_id: Some(id.clone()),
                    patch_id: Some(patch.id.to_string()),
                    evidence_ids,
                    evidence: promoted.iter().map(evidence_view).collect(),
                    registry_changed: true,
                    git_changed: false,
                    next_actions: next_search_actions(&patch),
                    ..CommandEnvelope::ok()
                })
            }
            Command::Status => workspace_status(&cli.cwd),
            Command::Diff { from, to } => {
                let summary = object_diff_summary(store, from, to)?;
                Ok(CommandEnvelope {
                    message: Some(summary),
                    ..CommandEnvelope::ok()
                })
            }
            Command::Discard => {
                bail!(
                    "[E_OBSOLETE_CWD_VIEW] graft discard no longer writes cwd because cwd is not a managed Graft view.\n  fix: use `graft patch materialize <patch-id>` to inspect .worktrees/<state-slug>/, or `graft patch promote` to write an explicit external target."
                )
            }
            Command::Incoming => incoming_command(store),
            Command::Search {
                constraint,
                base,
                producer,
                has_evidence,
            } => {
                if let Some(name) = constraint {
                    let config = load_graft_config(store)?;
                    warn_if_constraint_unknown(name, &config);
                }
                let patch_ids = search_patches(store, constraint, base, producer, has_evidence)?;
                Ok(CommandEnvelope {
                    patch_ids,
                    ..CommandEnvelope::ok()
                })
            }
            Command::Compose {
                first,
                second,
                constraint_primitives,
                validate,
            } => {
                store.init_storage()?;
                let first_patch = store.read_patch(first)?;
                let second_patch = store.read_patch(second)?;
                let first_resolved = store.resolve_application(&first_patch.application)?;
                let second_resolved = store.resolve_application(&second_patch.application)?;
                if first_resolved.record.target_state != second_resolved.record.base_state {
                    bail!(
                        "[E_COMPOSE_CONFLICT] cannot compose {first} then {second}: target({first}) = {} but base({second}) = {}; create a new candidate manually from the desired resolution",
                        state_label(&first_resolved.record.target_state),
                        state_label(&second_resolved.record.base_state),
                    );
                }
                let change = Change::compose(&first_resolved.change, &second_resolved.change);
                let config = load_graft_config(store)?;
                let (candidate, evidence) = write_candidate_from_change(
                    store,
                    change,
                    needs_revalidation_or(&config, constraint_primitives)?,
                    "composer",
                    Some(format!("compose {first} {second}")),
                    *validate,
                )?;
                write_cache_relation(
                    store,
                    PatchRelationKind::Composes,
                    candidate.id.as_str(),
                    vec![first.clone(), second.clone()],
                )?;
                Ok(candidate_envelope(
                    store,
                    candidate,
                    evidence,
                    "created composed candidate",
                )?)
            }
            Command::Migrate {
                id,
                onto,
                constraint_primitives,
                validate,
            } => {
                store.init_storage()?;
                let patch = store.read_patch(id)?;
                let resolved = store.resolve_application(&patch.application)?;
                let change = resolved.change;
                let config = load_graft_config(store)?;
                let base_state = resolve_base_state(store, &config, onto)?;
                let Some(base_snapshot) = base_snapshot_for_state(store, &config, &base_state)?
                else {
                    bail!("cannot resolve base snapshot for {onto}");
                };
                let migration = migrate_change(&change, base_state, &base_snapshot)?;
                let migrated = match migration {
                    MigrationOutcome::Clean { change, snapshot } => {
                        store.write_tree_snapshot(&snapshot)?;
                        change
                    }
                    MigrationOutcome::Blocked { reasons } => {
                        bail!(
                            "[E_COMPOSE_CONFLICT] cannot migrate {id} onto {onto}: {}; create a new candidate manually from the desired resolution",
                            reasons.join("; ")
                        );
                    }
                };
                let (candidate, evidence) = write_candidate_from_change(
                    store,
                    migrated,
                    needs_revalidation_or(&config, constraint_primitives)?,
                    "migrator",
                    Some(format!("migrate {id} onto {onto}")),
                    *validate,
                )?;
                write_cache_relation(
                    store,
                    PatchRelationKind::Migrates,
                    candidate.id.as_str(),
                    vec![id.clone(), onto.clone()],
                )?;
                Ok(candidate_envelope(
                    store,
                    candidate,
                    evidence,
                    "created migration candidate",
                )?)
            }
            Command::Revert {
                id,
                constraint_primitives,
                validate,
            } => {
                store.init_storage()?;
                let patch = store.read_patch(id)?;
                let change = store
                    .resolve_application(&patch.application)?
                    .change
                    .reversed();
                let config = load_graft_config(store)?;
                let (candidate, evidence) = write_candidate_from_change(
                    store,
                    change,
                    needs_revalidation_or(&config, constraint_primitives)?,
                    "reverter",
                    Some(format!("revert {id}")),
                    *validate,
                )?;
                write_cache_relation(
                    store,
                    PatchRelationKind::Reverts,
                    candidate.id.as_str(),
                    vec![id.clone()],
                )?;
                Ok(candidate_envelope(
                    store,
                    candidate,
                    evidence,
                    "created revert candidate",
                )?)
            }
            Command::Materialize {
                id,
                dry_run,
                discard: _,
                as_commit,
                ref_name,
            } => {
                if *as_commit || ref_name.is_some() {
                    bail!(
                        "[E_MATERIALIZE_STATE_ONLY] graft patch materialize only writes an isolated inspection state under .worktrees/; use `graft patch promote` for Git refs, branches, PRs, or releases"
                    );
                }
                let config = load_graft_config(store)?;
                materialize_state(store, &config, id, *dry_run)
            }
            Command::Promote {
                id,
                to,
                branch,
                yes,
                required,
                pr,
                release,
                title,
                body,
                head,
            } => {
                validate_promote_ref_args(
                    to,
                    branch.as_deref(),
                    release.as_deref(),
                    head.as_deref(),
                )?;
                let patch = store.read_patch(id)?;
                let evidence = store.registry_evidence_for_subject(id)?;
                let config = load_graft_config(store)?;
                ensure_change_integrity(store, &config, &patch.application)?;
                let configured_target = config.promote_targets.get(to);
                let requirement_plan = promotion_requirement_plan_with_target(
                    &config,
                    required,
                    configured_target.map(|target| &target.required),
                )?;
                let required_constraint = requirement_plan.constraint.clone();
                let required = requirement_plan.constraints.clone();
                if *yes {
                    let current_evidence =
                        evidence_for_current_verifiers(&config, &required, &evidence, id)?;
                    graft_policy::satisfies_subject(id, &required_constraint, &current_evidence)
                        .map_err(render_policy_error)?;
                    let git = GixBackend;
                    if let Some(target_config) = configured_target {
                        let snapshot = target_snapshot_for_patch(store, &config, &patch)?;
                        let target_path = config.promote_target_path(workspace_root, to)?;
                        let branch_name = branch
                            .as_deref()
                            .or(target_config.branch.as_deref())
                            .unwrap_or("main");
                        let materialized = git.materialize_commit(
                            &target_path,
                            &snapshot,
                            store.paths().object_blobs(),
                            &format!("graft patch promote {id} to {to}"),
                            None,
                        )?;
                        let promoted_ref =
                            git.promote_branch(&target_path, branch_name, &materialized.commit_id)?;
                        let mut promotion = PromotionRecord {
                            id: graft_core::PromotionId::new("promotion:pending"),
                            patch_id: patch.id.clone(),
                            target: format!("target:{to}:{promoted_ref}"),
                            dry_run: false,
                            status: format!(
                                "updated {to}:{promoted_ref} to {}",
                                materialized.commit_id
                            ),
                            promoted_at: OffsetDateTime::now_utc().to_string(),
                        };
                        promotion.id = promotion_id(&promotion)?;
                        store.write_promotion(&promotion)?;
                        write_registry_relation(
                            store,
                            PatchRelationKind::Promotes,
                            promotion.id.as_str(),
                            vec![id.clone(), materialized.commit_id.clone()],
                        )?;
                        return Ok(CommandEnvelope {
                            message: Some(format!(
                                "promoted {id} to {to}:{promoted_ref} at {}",
                                materialized.commit_id
                            )),
                            patch_id: Some(id.clone()),
                            promotions: vec![promotion_view(&promotion)],
                            registry_changed: true,
                            git_changed: true,
                            ..CommandEnvelope::ok()
                        });
                    }
                    let commit_id =
                        ensure_materialized_commit(&git, store, &config, &cli.cwd, &patch, id)?;
                    let (status, target, git_message) = if *pr {
                        let head_branch = head.clone().unwrap_or_else(|| {
                            format!("graft/{}", git_ref_component_for_patch_id(id))
                        });
                        git.promote_branch(&cli.cwd, &head_branch, &commit_id)?;
                        let pr = git.create_pull_request(
                            &cli.cwd,
                            &head_branch,
                            to,
                            title.as_deref().unwrap_or(&format!("Graft {id}")),
                            body.as_deref()
                                .unwrap_or("Created by graft patch promote --pr"),
                        )?;
                        (
                            format!("opened pull request {}", pr.url),
                            format!("pr:{to}"),
                            format!("opened PR {}", pr.url),
                        )
                    } else if let Some(tag) = release {
                        let release_ref = git.promote_release(&cli.cwd, tag, &commit_id)?;
                        (
                            format!("updated {release_ref} to {commit_id}"),
                            format!("release:{tag}"),
                            format!("promoted {id} to {release_ref} at {commit_id}"),
                        )
                    } else {
                        let promoted_ref = git.promote_branch(&cli.cwd, to, &commit_id)?;
                        (
                            format!("updated {promoted_ref} to {commit_id}"),
                            to.clone(),
                            format!("promoted {id} to {promoted_ref} at {commit_id}"),
                        )
                    };
                    let mut promotion = PromotionRecord {
                        id: graft_core::PromotionId::new("promotion:pending"),
                        patch_id: patch.id.clone(),
                        target,
                        dry_run: false,
                        status,
                        promoted_at: OffsetDateTime::now_utc().to_string(),
                    };
                    promotion.id = promotion_id(&promotion)?;
                    store.write_promotion(&promotion)?;
                    write_registry_relation(
                        store,
                        PatchRelationKind::Promotes,
                        promotion.id.as_str(),
                        vec![id.clone(), commit_id],
                    )?;
                    Ok(CommandEnvelope {
                        message: Some(git_message),
                        patch_id: Some(id.clone()),
                        promotions: vec![promotion_view(&promotion)],
                        registry_changed: true,
                        git_changed: true,
                        ..CommandEnvelope::ok()
                    })
                } else {
                    let target_kind = if *pr {
                        format!("pull request into {to}")
                    } else if let Some(tag) = release {
                        format!("release tag {tag}")
                    } else {
                        format!("branch {to}")
                    };
                    Ok(CommandEnvelope {
                        message: Some(format!(
                            "promotion dry-run for {id} to {target_kind}; required evidence: {} (source: {})",
                            plan_labels_or_core_only(&required),
                            requirement_plan.source.label()
                        )),
                        patch_id: Some(id.clone()),
                        patches: vec![summarize_patch_with_evidence(store, &patch, &evidence)?],
                        git_changed: false,
                        next_actions: vec![promote_next_action(id, to, *pr, release.as_deref())],
                        ..CommandEnvelope::ok()
                    })
                }
            }
            Command::Sync {
                remote,
                fetch_only,
                push_only,
                on_divergence,
            } => {
                if workspace_id == Some(DEFAULT_WORKSPACE_ID) {
                    bail!(
                        "[E_SYNC_DEFAULT_WORKSPACE] ws:default is machine-local and cannot sync; create or attach a local workspace before running graft sync"
                    );
                }
                if *fetch_only && *push_only {
                    bail!("sync cannot use --fetch-only and --push-only together");
                }
                let config = load_graft_config(store)?;
                if !config.sync.enabled {
                    bail!(
                        "[E_SYNC_DISABLED] sync is disabled for this workspace by [sync] enabled = false; remove that override or set enabled = true before running graft sync"
                    );
                }
                let explicit_remote = remote.is_some();
                let remote = resolve_sync_remote(store, remote.as_deref())?;
                let report = GraftSyncTransport.sync_public_store_with_options(
                    store.paths().root(),
                    &remote,
                    SyncOptions {
                        push: !*fetch_only,
                        fetch: !*push_only,
                        on_divergence: (*on_divergence).into(),
                    },
                )?;
                let default_remote_changed = if explicit_remote {
                    write_default_sync_remote(store, &remote)?
                } else {
                    false
                };
                Ok(CommandEnvelope {
                    message: Some(format!(
                        "synced {}: pushed {} files, fetched {} files, last_synced {}",
                        remote.display(),
                        report.pushed,
                        report.fetched,
                        report.last_synced.as_deref().unwrap_or("unchanged")
                    )),
                    registry_changed: report.fetched > 0
                        || report.state_changed
                        || default_remote_changed,
                    ..CommandEnvelope::ok()
                })
            }
            Command::Repo { command } => {
                let config = load_graft_config(store)?;
                run_repo_command(workspace_root, &config, command)
            }
            Command::Scratch {
                command: ScratchCommand::Status,
                socket,
            } => run_scratch_status(&cli.cwd, socket.as_deref()),
            Command::Scratch { command, socket } => {
                let workspace_id = workspace_id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "[E_NO_WORKSPACE_ID] typed daemon op requires a resolved workspace_id"
                    )
                })?;
                run_scratch_command(workspace_root, workspace_id, socket.as_deref(), command)
            }
            Command::Registry { command } => run_registry_command(store, &cli.cwd, command),
            Command::Cache { command } => match command {
                CacheCommand::Search { constraint, failed } => {
                    let constraint_filter = match constraint.as_deref() {
                        Some(constraint) => {
                            let config = load_graft_config(store)?;
                            warn_if_constraint_unknown(constraint, &config);
                            Some((constraint, config))
                        }
                        None => None,
                    };
                    let mut summaries = Vec::new();
                    for candidate in store.list_candidates()? {
                        let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
                        if let Some((constraint, config)) = constraint_filter.as_ref() {
                            let mut matched = false;
                            for expr in constraint_primitives(&candidate.constraint) {
                                if constraint_matches_request(config, &expr, constraint)? {
                                    matched = true;
                                    break;
                                }
                            }
                            if !matched {
                                for record in &evidence {
                                    if constraint_id_matches(config, &record.plan, constraint)? {
                                        matched = true;
                                        break;
                                    }
                                }
                            }
                            if !matched {
                                continue;
                            }
                        }
                        if *failed
                            && !evidence.iter().any(|record| {
                                matches!(&record.result, EvidenceResult::Failed { .. })
                            })
                        {
                            continue;
                        }
                        summaries.push(summarize_candidate_with_evidence(
                            store, &candidate, &evidence,
                        )?);
                    }
                    Ok(CommandEnvelope {
                        candidates: summaries,
                        ..CommandEnvelope::ok()
                    })
                }
            },
            Command::VerifyPending { patch, limit } => {
                verify_pending_command(store, patch.as_deref(), *limit)
            }
            Command::Evidence { subject } => {
                let mut evidence = store.cached_evidence_for_subject(subject)?;
                evidence.extend(store.registry_evidence_for_subject(subject)?);
                Ok(CommandEnvelope {
                    evidence: evidence.iter().map(evidence_view).collect(),
                    ..CommandEnvelope::ok()
                })
            }
            Command::Gc {
                apply,
                derived_only,
            } => run_gc(store, *apply, *derived_only),
            Command::Explain { .. } => {
                bail!("[E_ROUTE_UNKNOWN] explain is handled by the top-level router")
            }
        }
    }
}

fn resolve_registered_workspace(workspace_id: &str) -> Result<(PathBuf, Option<String>)> {
    let registry = RegistryStore::from_env();
    let workspace = registry.get_workspace(workspace_id)?.ok_or_else(|| {
        anyhow::anyhow!("[E_UNKNOWN_WORKSPACE] workspace {workspace_id} is not registered")
    })?;
    Ok((workspace.root, Some(workspace.id)))
}

fn verify_pending_command(
    store: &GraftStore,
    patch_filter: Option<&str>,
    limit: Option<usize>,
) -> Result<CommandEnvelope> {
    let mut rebuilt = Vec::new();
    let mut checked = 0usize;
    for patch in store.list_patches()? {
        if let Some(filter) = patch_filter
            && patch.id.as_str() != filter
        {
            continue;
        }
        let refs = store.patch_evidence_index(patch.id.as_str())?;
        let missing = refs
            .iter()
            .filter(|id| {
                !store
                    .paths()
                    .object_evidence()
                    .join(format!("{id}.json"))
                    .exists()
            })
            .count();
        if missing == 0 {
            continue;
        }
        if limit.is_some_and(|limit| checked >= limit) {
            break;
        }
        checked += 1;
        let before = store.patch_evidence_index(patch.id.as_str())?;
        let evidence = validate_patch(store, &patch, &[])?;
        let after = store.patch_evidence_index(patch.id.as_str())?;
        let matched = evidence
            .iter()
            .filter(|record| before.iter().any(|id| id == record.id.as_str()))
            .count();
        let appended = after.len().saturating_sub(before.len());
        rebuilt.push(format!(
            "{}: rebuilt {} matching evidence, appended {} new evidence",
            patch.id, matched, appended
        ));
    }
    if rebuilt.is_empty() {
        rebuilt.push("no pending evidence to rebuild".to_string());
    }
    Ok(CommandEnvelope {
        message: Some(rebuilt.join(
            "
",
        )),
        registry_changed: true,
        ..CommandEnvelope::ok()
    })
}

fn clone_command(remote: &Path, dir: &Path) -> Result<CommandEnvelope> {
    if dir.exists() && fs::read_dir(dir)?.next().is_some() {
        bail!(
            "[E_CLONE_DEST_NOT_EMPTY] clone destination {} is not empty",
            dir.display()
        );
    }
    fs::create_dir_all(dir)?;
    if dir.join(".git").exists() || dir.join(".graft").exists() {
        bail!("[E_CLONE_DEST_NOT_EMPTY] clone destination must not contain .git or .graft");
    }
    let store = GraftStore::open(dir);
    init_workspace_files(&store)?;
    let remote = normalize_sync_remote_path(remote);
    let report =
        GraftSyncTransport.sync_public_store(store.paths().root(), &remote, false, true)?;
    write_default_sync_remote(&store, &remote)?;
    Ok(CommandEnvelope {
        message: Some(format!(
            "cloned {} into {}; fetched {} files; cwd left empty; run graft patch incoming or graft patch materialize <patch>",
            remote.display(),
            dir.display(),
            report.fetched
        )),
        registry_changed: true,
        ..CommandEnvelope::ok()
    })
}

fn resolve_sync_remote(store: &GraftStore, remote: Option<&Path>) -> Result<PathBuf> {
    match remote {
        Some(remote) => Ok(normalize_sync_remote_path_from(
            store.paths().workspace(),
            remote,
        )),
        None => read_default_sync_remote(store),
    }
}

fn read_default_sync_remote(store: &GraftStore) -> Result<PathBuf> {
    let path = default_sync_remote_path(store);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "[E_SYNC_REMOTE_REQUIRED] no sync remote was provided and {} does not exist; run `graft sync <remote>` once to record the default remote",
            path.display()
        )
    })?;
    let remote = text.trim();
    if remote.is_empty() {
        bail!(
            "[E_SYNC_REMOTE_REQUIRED] default sync remote at {} is empty; run `graft sync <remote>` to repair it",
            path.display()
        );
    }
    Ok(normalize_sync_remote_path_from(
        store.paths().workspace(),
        Path::new(remote),
    ))
}

fn write_default_sync_remote(store: &GraftStore, remote: &Path) -> Result<bool> {
    let path = default_sync_remote_path(store);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = format!("{}\n", remote.display());
    if fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return Ok(false);
    }
    fs::write(path, body)?;
    Ok(true)
}

fn default_sync_remote_path(store: &GraftStore) -> PathBuf {
    store.paths().default_sync_remote()
}

fn normalize_sync_remote_path(remote: &Path) -> PathBuf {
    normalize_workspace_path(remote)
}

fn normalize_sync_remote_path_from(base: &Path, remote: &Path) -> PathBuf {
    let remote = if remote.is_absolute() {
        remote.to_path_buf()
    } else {
        base.join(remote)
    };
    normalize_workspace_path(&remote)
}

pub(crate) fn ensure_workspace_initialized(store: &GraftStore) -> Result<()> {
    if store.is_initialized() {
        return Ok(());
    }
    bail!(
        "[E_NO_CONFIG] graft.toml not found at {} — this directory is not a graft workspace.\n  fix: run `graft init` here, repair the registry route, or set GRAFT_WORKSPACE",
        store.paths().config().display(),
    );
}

fn validation_satisfaction_summary(
    result: std::result::Result<graft_policy::AdmissionDecision, graft_policy::PolicyError>,
) -> String {
    match result {
        Ok(decision) if decision.accepted => "constraint satisfied".to_string(),
        Ok(_) => "constraint not satisfied".to_string(),
        Err(error) => format!(
            "constraint not satisfied: {}",
            render_policy_error_text(&error)
        ),
    }
}

fn render_policy_error(error: graft_policy::PolicyError) -> anyhow::Error {
    anyhow!(render_policy_error_text(&error))
}

fn render_policy_error_text(error: &graft_policy::PolicyError) -> String {
    match error {
        graft_policy::PolicyError::MissingEvidence { constraint, path } => {
            graft_explain::diagnostics::a001_missing_required_evidence_at(constraint, path)
                .format_reason()
        }
        graft_policy::PolicyError::EvidenceNotPassed {
            constraint,
            evidence,
            path,
        } => {
            graft_explain::diagnostics::a002_failed_required_evidence_at(constraint, evidence, path)
                .format_reason()
        }
        graft_policy::PolicyError::BottomReached
        | graft_policy::PolicyError::ConstraintUnsatisfied { .. } => error.to_string(),
    }
}

#[cfg(test)]
fn require_passed_evidence(
    required: &[PlanId],
    evidence: &[EvidenceRecord],
    subject: &str,
) -> Result<()> {
    for constraint in required {
        let mut matching = evidence
            .iter()
            .filter(|record| record.subject == subject && record.plan == *constraint);
        let label = plan_label(constraint);
        let Some(first) = matching.next() else {
            bail!(
                "{}",
                graft_explain::diagnostics::a001_missing_required_evidence(&label).format_reason()
            );
        };
        if !first.result.satisfies_requirement()
            && !matching.any(|record| record.result.satisfies_requirement())
        {
            bail!(
                "{}",
                graft_explain::diagnostics::a002_failed_required_evidence(&label).format_reason()
            );
        }
    }
    Ok(())
}

fn resolved_application(
    store: &GraftStore,
    application: &ApplicationRef,
) -> Result<ResolvedApplication> {
    store.resolve_application(application).map_err(Into::into)
}

fn ensure_candidate_constraint_current(
    config: &GraftConfig,
    candidate: &GraftCandidate,
) -> Result<()> {
    for primitive in constraint_primitives(&candidate.constraint) {
        let Some(current) = config.plans.get(&primitive) else {
            bail!(
                "[E_CONSTRAINT_DRIFT] candidate constraint primitive `{}` no longer exists in constraints.roto",
                plan_label(&primitive)
            );
        };
        let current_id = current.plan_id()?;
        if current_id != primitive {
            bail!(
                "[E_CONSTRAINT_DRIFT] candidate constraint primitive `{}` drifted: candidate has {}, current plan resolves to {}",
                plan_label(&primitive),
                primitive,
                current_id
            );
        }
    }
    Ok(())
}

fn write_candidate_from_change(
    store: &GraftStore,
    change: Change,
    constraint_primitives: Vec<PlanId>,
    producer: &str,
    message: Option<String>,
    validate: bool,
) -> Result<(GraftCandidate, Vec<EvidenceRecord>)> {
    store.write_change(&change)?;
    let materialized = application_from_change(&change)?;
    let application = store.write_materialized_application(&materialized)?;
    let mut candidate = GraftCandidate {
        id: graft_core::CandidateId::new("candidate:pending"),
        application,
        constraint: constraint_from_plans(&constraint_primitives),
        provenance: Provenance::now(producer, message),
    };
    candidate.id = candidate_id(&candidate)?;
    store.write_candidate(&candidate)?;
    let evidence = if validate {
        validate_candidate(store, &candidate, &[])?
    } else {
        Vec::new()
    };
    Ok((candidate, evidence))
}

enum MigrationOutcome {
    Clean {
        change: Change,
        snapshot: TreeSnapshot,
    },
    Blocked {
        reasons: Vec<String>,
    },
}

fn migrate_change(
    change: &Change,
    base_state: StateId,
    base_snapshot: &TreeSnapshot,
) -> Result<MigrationOutcome> {
    let mut entries = base_snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut blocks = Vec::new();

    for file in change.endpoint_diff() {
        let current = entries.get(&file.path);
        match file.kind {
            FileChangeKind::Added | FileChangeKind::Captured => {
                if let (Some(current), Some(target_hash)) = (current, file.target_hash.as_ref())
                    && current.hash != *target_hash
                {
                    blocks.push(format!("{}: path already exists on new base", file.path));
                    continue;
                }
                if let (Some(hash), Some(size)) = (&file.target_hash, file.target_size) {
                    entries.insert(
                        file.path.clone(),
                        TreeEntry {
                            path: file.path.clone(),
                            hash: hash.clone(),
                            size,
                        },
                    );
                }
            }
            FileChangeKind::Modified => {
                match (current, file.base_hash.as_ref()) {
                    (None, Some(_)) => {
                        blocks.push(format!(
                            "{}: modified path is missing on new base",
                            file.path
                        ));
                        continue;
                    }
                    (Some(current), Some(base_hash)) if current.hash != *base_hash => {
                        blocks.push(format!("{}: path changed on new base", file.path));
                        continue;
                    }
                    _ => {}
                }
                if let (Some(hash), Some(size)) = (&file.target_hash, file.target_size) {
                    entries.insert(
                        file.path.clone(),
                        TreeEntry {
                            path: file.path.clone(),
                            hash: hash.clone(),
                            size,
                        },
                    );
                }
            }
            FileChangeKind::Deleted => {
                if let (Some(current), Some(base_hash)) = (current, file.base_hash.as_ref())
                    && current.hash != *base_hash
                {
                    blocks.push(format!("{}: deleted path changed on new base", file.path));
                    continue;
                }
                entries.remove(&file.path);
            }
            FileChangeKind::Unchanged => {}
        }
    }

    if !blocks.is_empty() {
        return Ok(MigrationOutcome::Blocked { reasons: blocks });
    }

    let snapshot = TreeSnapshot::new(entries.into_values().collect());
    let target_state = StateId::GraftTree(snapshot.id()?);
    let migrated = Change::from_snapshots(base_state, Some(base_snapshot), target_state, &snapshot);
    Ok(MigrationOutcome::Clean {
        change: migrated,
        snapshot,
    })
}

fn write_cache_relation(
    store: &GraftStore,
    kind: PatchRelationKind,
    subject: &str,
    sources: Vec<String>,
) -> Result<()> {
    let relation = relation_record(kind, subject, sources)?;
    store.write_cache_relation(&relation)?;
    Ok(())
}

fn write_registry_relation(
    store: &GraftStore,
    kind: PatchRelationKind,
    subject: &str,
    sources: Vec<String>,
) -> Result<()> {
    let relation = relation_record(kind, subject, sources)?;
    store.write_relation(&relation)?;
    Ok(())
}

fn relation_record(
    kind: PatchRelationKind,
    subject: &str,
    sources: Vec<String>,
) -> Result<PatchRelation> {
    let mut relation = PatchRelation {
        id: graft_core::RelationId::new("relation:pending"),
        kind,
        subject: subject.to_string(),
        sources,
        created_at: OffsetDateTime::now_utc().to_string(),
    };
    relation.id = relation_id(&relation)?;
    Ok(relation)
}

fn candidate_envelope(
    store: &GraftStore,
    candidate: GraftCandidate,
    evidence: Vec<EvidenceRecord>,
    message: &str,
) -> Result<CommandEnvelope> {
    Ok(CommandEnvelope {
        message: Some(format!("{message} {}", candidate.id)),
        candidate_id: Some(candidate.id.to_string()),
        evidence_ids: evidence
            .iter()
            .map(|record| record.id.to_string())
            .collect(),
        evidence: evidence.iter().map(evidence_view).collect(),
        candidates: vec![summarize_candidate(store, &candidate)?],
        cache_changed: true,
        next_actions: next_actions_for_candidate(&candidate, &evidence),
        ..CommandEnvelope::ok()
    })
}

fn show_record(
    store: &GraftStore,
    id: &str,
    include_evidence: bool,
    include_change: bool,
) -> Result<CommandEnvelope> {
    match record_ref_kind(id)? {
        RecordRefKind::Candidate => {
            let candidate = store
                .read_candidate(id)
                .with_context(|| format!("read candidate record {id}"))?;
            let evidence_records = store.cached_evidence_for_subject(id)?;
            let change_view = if include_change {
                change_view_for_application(store, &candidate.application)?
            } else {
                None
            };
            let mut envelope = CommandEnvelope {
                candidate_id: Some(id.to_string()),
                candidates: vec![summarize_candidate_with_evidence(
                    store,
                    &candidate,
                    &evidence_records,
                )?],
                change: change_view,
                ..CommandEnvelope::ok()
            };
            if include_evidence {
                envelope.evidence = evidence_records.iter().map(evidence_view).collect();
            }
            Ok(envelope)
        }
        RecordRefKind::Patch => {
            let patch = store
                .read_patch(id)
                .with_context(|| format!("read patch record {id}"))?;
            let evidence_records = store.registry_evidence_for_subject(id)?;
            let change_view = if include_change {
                change_view_for_application(store, &patch.application)?
            } else {
                None
            };
            let mut envelope = CommandEnvelope {
                patch_id: Some(id.to_string()),
                patches: vec![summarize_patch_with_evidence(
                    store,
                    &patch,
                    &evidence_records,
                )?],
                change: change_view,
                ..CommandEnvelope::ok()
            };
            if include_evidence {
                envelope.evidence = evidence_records.iter().map(evidence_view).collect();
            }
            Ok(envelope)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordRefKind {
    Candidate,
    Patch,
}

fn record_ref_kind(id: &str) -> Result<RecordRefKind> {
    if id
        .strip_prefix("candidate:")
        .is_some_and(|rest| !rest.is_empty())
    {
        return Ok(RecordRefKind::Candidate);
    }
    if id
        .strip_prefix("patch:")
        .is_some_and(|rest| !rest.is_empty())
    {
        return Ok(RecordRefKind::Patch);
    }
    bail!(
        "[E_UNSUPPORTED_RECORD_ID] expected a candidate:<digest> or patch:<digest> id, got `{id}`"
    )
}

#[cfg(test)]
mod tests;
