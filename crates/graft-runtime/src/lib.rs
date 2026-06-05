mod candidate;
mod config;
mod daemon_client;
mod property;
mod registry;
mod repo;
mod requirements;
mod roto_properties;
mod scratch;
mod validation;
mod view;
mod workspace;

use std::fs;
use std::io::{self, Write};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use candidate::{CandidateCommand, run_candidate_command};
use clap::{Parser, Subcommand, ValueEnum};
#[cfg(test)]
use config::write_property_lock;
use config::{GraftConfig, load_graft_config, load_property_defs};
use daemon_client::workspace_root_wire_string;
use graft_client::{daemon_socket_path, request_result_or_spawn};
use graft_core::{
    ChangeRef, ChangeSet, EvidenceRecord, EvidenceResult, FileChangeKind, GraftCandidate,
    PatchRecord, PatchRelation, PatchRelationKind, PromotionRecord, PropertyId, Provenance,
    ScopedPropertyRef, StateId, TreeEntry, TreeSnapshot, candidate_id, patch_id, promotion_id,
    relation_id,
};
use graft_explain::NextAction;
use graft_promote::GixBackend;
use graft_store::{
    DEFAULT_WORKSPACE_ID, GraftStore, RegistryStore, StoreError, WorkspaceDiscovery,
    default_workspace_root, normalize_workspace_path,
};
#[cfg(test)]
use graft_store::{WorkspaceKind, local_workspace_id_for_root};
use graft_sync::{DivergencePolicy as SyncDivergencePolicy, GraftSyncTransport, SyncOptions};
use property::{PropertyCommand, run_property_command};
use registry::{RegistryCommand, run_registry_command};
use repo::{
    RepoCommand, base_snapshot_for_state, materialized_snapshot_for_state, resolve_base_state,
    run_repo_command,
};
use requirements::{
    admission_required_scoped_properties, needs_revalidation_or, promotion_requirement_plan,
    property_label, property_matches_request, property_refs_for_scoped,
    resolve_scoped_property_ref, scoped_property_label,
};
use scratch::{ScratchCommand, run_scratch_command, run_scratch_status};
use time::OffsetDateTime;
use validation::{
    ensure_change_integrity, evidence_for_current_verifiers, validate_candidate, validate_patch,
};
use view::{
    CandidateSummary, ChangeView, CommandEnvelope, CommandView, EvidenceCounts, EvidenceView,
    PatchSummary, PromotionView, RunView, print_human,
};
use workspace::{
    gc_apply_daemon_argv, init_workspace_files, modernize_legacy_gc_apply_message,
    run_attach_command, run_detach_command, run_doctor_command, run_gc, run_init_command,
    run_ps_command, run_workspace_command, workspace_status,
};

#[derive(Parser, Debug)]
#[command(
    name = "graft",
    about = "Property-aware patch runtime for agent changes",
    long_about = "Draft, validate and admit patch candidates with explicit property obligations and evidence, isolated from .git/ until you explicitly materialize or promote."
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
            help = "Show only candidates whose expected list contains this property"
        )]
        property: Option<String>,
        #[arg(long, help = "Show only candidates with at least one failed evidence")]
        failed: bool,
        #[arg(long, help = "Filter by provenance producer label")]
        producer: Option<String>,
    },
    /// Show identity, change summary, properties and evidence for a candidate or patch
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
            help = "Validate this whole-state property, for example workspace:tests_pass (repeatable)"
        )]
        expected: Vec<String>,
    },
    /// Admit a candidate into the registry once required evidence is present
    #[command(hide = true)]
    Admit {
        /// Candidate id to admit
        id: String,
        #[arg(
            long = "require",
            help = "Add a one-shot admission requirement like workspace:tests_pass; repeats append to [admission.required_properties] and candidate expectations"
        )]
        required: Vec<String>,
    },
    /// Show cwd route and resolved workspace status
    #[command(hide = true)]
    Status,
    /// Show object-to-object changes between materializable refs
    #[command(hide = true)]
    Diff {
        /// Source state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, repo:<id>@<treeish>, or workspace Git treeish
        from: String,
        /// Target state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, repo:<id>@<treeish>, or workspace Git treeish
        to: String,
    },
    /// Obsolete: cwd is not a managed view and cannot be restored by Graft
    #[command(hide = true)]
    Discard,
    /// Show incoming patch groups from the local public store
    #[command(hide = true)]
    Incoming,
    /// Search admitted patches in the registry by property, base or evidence
    #[command(hide = true)]
    Search {
        #[arg(long, help = "Match patches whose property set contains this property")]
        property: Option<String>,
        #[arg(long, help = "Match patches whose declared base equals this state")]
        base: Option<String>,
        #[arg(
            long,
            help = "Match patches whose provenance producer equals this label"
        )]
        producer: Option<String>,
        #[arg(
            long = "has-evidence",
            help = "Match patches that carry passing evidence for this whole-state property"
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
            help = "Whole-state property the composed candidate should later satisfy"
        )]
        expected: Vec<String>,
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
            help = "Whole-state property the migrated candidate should later satisfy"
        )]
        expected: Vec<String>,
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
            help = "Whole-state property the revert candidate should later satisfy"
        )]
        expected: Vec<String>,
        #[arg(long, help = "Run validators on the revert candidate immediately")]
        validate: bool,
    },
    /// Run a command inside a temporary materialized state; writes are discarded
    Run {
        /// State ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, repo:<id>@<treeish>, or workspace Git treeish
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
    Materialize {
        /// State ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, repo:<id>@<treeish>, or workspace Git treeish
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
            help = "Property that must have passing evidence before promotion (repeatable)"
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
    /// Manage explicit property definitions and their lockfile
    #[command(hide = true)]
    Property {
        #[command(subcommand)]
        command: PropertyCommand,
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
        #[arg(long, help = "Filter by property name or id")]
        property: Option<String>,
        #[arg(long, help = "Filter by provenance producer label")]
        producer: Option<String>,
    },
    /// Create a candidate from an existing scratch id
    FromScratch(crate::candidate::CandidateFromScratchArgs),
    /// Show identity, change summary, properties and evidence for a candidate or patch
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
            help = "Validate this whole-state property, for example workspace:tests_pass (repeatable)"
        )]
        expected: Vec<String>,
    },
    /// Admit a candidate into the registry once required evidence is present
    Admit {
        /// Candidate id to admit
        id: String,
        #[arg(
            long = "require",
            help = "Add a one-shot admission requirement like workspace:tests_pass; repeats append to [admission.required_properties] and candidate expectations"
        )]
        required: Vec<String>,
    },
    /// Show incoming patch groups from the local public store
    Incoming,
    /// Search admitted patches in the registry by property, base or evidence
    Search {
        #[arg(long, help = "Match patches whose property set contains this property")]
        property: Option<String>,
        #[arg(long, help = "Match patches whose declared base equals this state")]
        base: Option<String>,
        #[arg(
            long,
            help = "Match patches whose provenance producer equals this label"
        )]
        producer: Option<String>,
        #[arg(
            long = "has-evidence",
            help = "Match patches that carry passing evidence for this whole-state property"
        )]
        has_evidence: Option<String>,
    },
    /// Show object-to-object changes between materializable refs
    Diff {
        /// Source state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, repo:<id>@<treeish>, or workspace Git treeish
        from: String,
        /// Target state ref: graft:empty, tree:<id>, candidate:<id>, patch:<id>, repo:<id>@<treeish>, or workspace Git treeish
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
            help = "Whole-state property the composed candidate should later satisfy"
        )]
        expected: Vec<String>,
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
            help = "Whole-state property the migrated candidate should later satisfy"
        )]
        expected: Vec<String>,
        #[arg(long, help = "Run validators on the migrated candidate immediately")]
        validate: bool,
    },
    /// Produce a candidate that reverts an admitted patch
    Revert {
        /// Patch id to revert
        id: String,
        #[arg(
            long = "expect",
            help = "Whole-state property the revert candidate should later satisfy"
        )]
        expected: Vec<String>,
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
            help = "Property that must have passing evidence before promotion (repeatable)"
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
            help = "Match candidates whose expected list contains this property"
        )]
        property: Option<String>,
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

pub fn resolve_candidate_expected_properties(
    store: &GraftStore,
    names: &[String],
) -> Result<Vec<ScopedPropertyRef>> {
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

fn command_gc_dry_run_derived_only(command: &Command) -> Option<bool> {
    match command {
        Command::Gc {
            apply: false,
            derived_only,
        }
        | Command::Workspace {
            command:
                WorkspaceCommand::Gc {
                    apply: false,
                    derived_only,
                },
        } => Some(*derived_only),
        _ => None,
    }
}

fn command_is_gc_apply(command: &Command) -> bool {
    matches!(
        command,
        Command::Gc { apply: true, .. }
            | Command::Workspace {
                command: WorkspaceCommand::Gc { apply: true, .. },
            }
    )
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

fn run_via_daemon(cli: &Cli) -> Result<CommandEnvelope> {
    run_via_daemon_with_argv(cli, None)
}

fn run_workspace_registry_write_via_daemon(cli: &Cli) -> Result<CommandEnvelope> {
    let socket = daemon_socket_path()?;
    let (op, params) = workspace_registry_write_request(cli)?;
    let daemon_anchor = default_workspace_root();
    let result = request_result_or_spawn(&daemon_anchor, &socket, op, params)?;
    result_to_envelope(result)
}

fn workspace_registry_write_request(cli: &Cli) -> Result<(&'static str, serde_json::Value)> {
    let cwd = cwd_wire_string(&cli.cwd)?;
    match &cli.command {
        Command::Attach {
            workspace,
            status: false,
        }
        | Command::Workspace {
            command:
                WorkspaceCommand::Attach {
                    workspace,
                    status: false,
                },
        } => {
            let mut params = serde_json::json!({ "cwd": cwd });
            if let Some(workspace) = workspace {
                params
                    .as_object_mut()
                    .expect("workspace_attach params are an object")
                    .insert("workspace".to_string(), serde_json::json!(workspace));
            }
            Ok(("workspace_attach", params))
        }
        Command::Detach
        | Command::Workspace {
            command: WorkspaceCommand::Detach,
        } => Ok(("workspace_detach", serde_json::json!({ "cwd": cwd }))),
        _ => bail!("[E_INTERNAL] workspace registry write route received a non-registry command"),
    }
}

fn cwd_wire_string(cwd: &Path) -> Result<&str> {
    let cwd = cwd.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "[E_UNREPRESENTABLE_CWD] cwd contains non-UTF-8 bytes and cannot be encoded in the current daemon JSON wire protocol: {}",
            cwd.display()
        )
    })?;
    if cwd.trim().is_empty() {
        bail!("[E_BAD_PARAMS] cwd must not be empty");
    }
    Ok(cwd)
}

fn run_via_daemon_with_argv(cli: &Cli, argv: Option<Vec<String>>) -> Result<CommandEnvelope> {
    let location = WorkspaceDiscovery::from_env().discover(&cli.cwd)?;
    let workspace_root = location.root().to_path_buf();
    let store = GraftStore::open(&workspace_root);
    ensure_workspace_initialized(&store)?;
    let socket = daemon_socket_path()?;
    let workspace_root_wire = workspace_root_wire_string(&workspace_root)?;
    let argv = argv.unwrap_or_else(|| daemon_argv_with_workspace_root(&workspace_root));
    let workspace_id = location
        .id()
        .ok_or_else(|| {
            anyhow::anyhow!("[E_NO_WORKSPACE_ID] cli_exec requires a resolved workspace_id")
        })?
        .to_string();
    let result = request_result_or_spawn(
        &workspace_root,
        &socket,
        "cli_exec",
        serde_json::json!({
            "argv": argv,
            "workspace_id": workspace_id,
            "workspace_root": workspace_root_wire
        }),
    )?;
    result_to_envelope(result)
}

fn result_to_envelope(result: serde_json::Value) -> Result<CommandEnvelope> {
    serde_json::from_value(result)
        .context("[E_BAD_DAEMON_RESPONSE] daemon result is not a command envelope")
}

fn command_uses_cli_exec(command: &Command) -> bool {
    match command {
        Command::Validate { .. }
        | Command::Admit { .. }
        | Command::Compose { .. }
        | Command::Migrate { .. }
        | Command::Revert { .. }
        | Command::Promote { .. }
        | Command::Sync { .. }
        | Command::VerifyPending { .. }
        | Command::Gc { apply: true, .. }
        | Command::Registry {
            command: RegistryCommand::Import { .. },
        }
        | Command::Bundle {
            command: RegistryCommand::Import { .. },
        } => true,
        Command::Patch { command } => patch_command_uses_cli_exec(command),
        Command::Workspace { command } => {
            matches!(command, WorkspaceCommand::Gc { apply: true, .. })
        }
        Command::Repo {
            command:
                RepoCommand::Add { .. }
                | RepoCommand::Sync { .. }
                | RepoCommand::Lock { .. }
                | RepoCommand::Update { .. },
        } => true,
        Command::Get { .. }
        | Command::Scratch { .. }
        | Command::Candidate { .. }
        | Command::Property { .. }
        | Command::Init { .. }
        | Command::Attach { .. }
        | Command::Detach
        | Command::Ps
        | Command::Doctor { .. }
        | Command::Clone { .. }
        | Command::Candidates { .. }
        | Command::Show { .. }
        | Command::Status
        | Command::Run { .. }
        | Command::Materialize { .. }
        | Command::Diff { .. }
        | Command::Discard
        | Command::Incoming
        | Command::Search { .. }
        | Command::Repo {
            command: RepoCommand::List,
        }
        | Command::Registry {
            command: RegistryCommand::Export { .. },
        }
        | Command::Bundle {
            command: RegistryCommand::Export { .. },
        }
        | Command::Cache { .. }
        | Command::Evidence { .. }
        | Command::Gc { apply: false, .. }
        | Command::Explain { .. } => false,
    }
}

fn command_is_workspace_registry_write(command: &Command) -> bool {
    matches!(
        command,
        Command::Attach { status: false, .. }
            | Command::Detach
            | Command::Workspace {
                command: WorkspaceCommand::Attach { status: false, .. } | WorkspaceCommand::Detach,
            }
    )
}

fn patch_command_uses_cli_exec(command: &PatchCommand) -> bool {
    match route_patch_command(command) {
        PatchCommandRoute::TopLevelAlias(command) => command_uses_cli_exec(&command),
        PatchCommandRoute::List { .. }
        | PatchCommandRoute::FromScratch(_)
        | PatchCommandRoute::Show { .. }
        | PatchCommandRoute::Incoming => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopLevelRoute {
    Explain,
    GcPrompt { derived_only: bool },
    GcApply,
    WorkspaceRegistryWrite,
    CliExec,
    Local,
}

fn route_top_level_command(command: &Command) -> TopLevelRoute {
    if matches!(command, Command::Explain { .. }) {
        TopLevelRoute::Explain
    } else if let Some(derived_only) = command_gc_dry_run_derived_only(command) {
        TopLevelRoute::GcPrompt { derived_only }
    } else if command_is_gc_apply(command) {
        TopLevelRoute::GcApply
    } else if command_is_workspace_registry_write(command) {
        TopLevelRoute::WorkspaceRegistryWrite
    } else if command_uses_cli_exec(command) {
        TopLevelRoute::CliExec
    } else {
        TopLevelRoute::Local
    }
}

struct DaemonCliExecRouter;

impl DaemonCliExecRouter {
    fn ensure_supported(command: &Command) -> Result<()> {
        if command_uses_cli_exec(command) {
            Ok(())
        } else {
            bail!(
                "[E_CLI_EXEC_UNSUPPORTED] cli_exec only accepts daemon-owned write commands; use the typed daemon op or local CLI path for this command"
            )
        }
    }
}

fn run_patch_list_command(
    store: &GraftStore,
    candidates: bool,
    all: bool,
    property: &Option<String>,
    producer: &Option<String>,
) -> Result<CommandEnvelope> {
    if candidates && all {
        bail!("patch list cannot use --candidates and --all together");
    }
    let patches = if candidates {
        Vec::new()
    } else {
        list_patch_summaries(store, property, producer)?
    };
    let candidate_summaries = if candidates || all {
        list_candidate_summaries(store, property, false, producer)?
    } else {
        Vec::new()
    };
    let message = if all {
        format!(
            "listed {} admitted patch(es) and {} candidate(s)",
            patches.len(),
            candidate_summaries.len()
        )
    } else if candidates {
        format!("listed {} candidate(s)", candidate_summaries.len())
    } else {
        format!("listed {} admitted patch(es)", patches.len())
    };
    Ok(CommandEnvelope {
        message: Some(message),
        patches,
        candidates: candidate_summaries,
        ..CommandEnvelope::ok()
    })
}

fn list_patch_summaries(
    store: &GraftStore,
    property: &Option<String>,
    producer: &Option<String>,
) -> Result<Vec<PatchSummary>> {
    let mut patches = store.list_patches()?;
    if let Some(property) = property {
        let config = load_graft_config(store)?;
        warn_if_property_unknown(property, &config);
        let mut filtered = Vec::new();
        for patch in patches {
            let mut matched = false;
            for expr in &patch.properties {
                if property_matches_request(&config, expr, property)? {
                    matched = true;
                    break;
                }
            }
            if matched {
                filtered.push(patch);
            }
        }
        patches = filtered;
    }
    if let Some(producer) = producer {
        patches.retain(|patch| patch.provenance.producer == *producer);
    }
    patches.sort_by(|left, right| left.id.cmp(&right.id));
    patches
        .iter()
        .map(|patch| {
            let evidence = store.registry_evidence_for_subject(patch.id.as_str())?;
            summarize_patch_with_evidence(store, patch, &evidence)
        })
        .collect()
}

fn list_candidate_summaries(
    store: &GraftStore,
    property: &Option<String>,
    failed: bool,
    producer: &Option<String>,
) -> Result<Vec<CandidateSummary>> {
    let property_filter = match property.as_deref() {
        Some(property) => {
            let config = load_graft_config(store)?;
            warn_if_property_unknown(property, &config);
            Some((property, config))
        }
        None => None,
    };
    let mut summaries = Vec::new();
    for candidate in store.list_candidates()? {
        if let Some((property, config)) = property_filter.as_ref() {
            let mut matched = false;
            for expr in &candidate.expected {
                if property_matches_request(config, &expr.property, property)? {
                    matched = true;
                    break;
                }
            }
            if !matched {
                continue;
            }
        }
        if let Some(producer) = producer
            && candidate.provenance.producer != *producer
        {
            continue;
        }
        let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
        if failed
            && !evidence
                .iter()
                .any(|record| matches!(&record.result, EvidenceResult::Failed { .. }))
        {
            continue;
        }
        summaries.push(summarize_candidate_with_evidence(
            store, &candidate, &evidence,
        )?);
    }
    summaries.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(summaries)
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

fn command_uses_cwd_directly(command: &Command) -> bool {
    matches!(
        command,
        Command::Init { .. }
            | Command::Attach { .. }
            | Command::Detach
            | Command::Ps
            | Command::Doctor { .. }
            | Command::Clone { .. }
            | Command::Get { .. }
            | Command::Explain { .. }
            | Command::Status
            | Command::Workspace {
                command: WorkspaceCommand::Init { .. }
                    | WorkspaceCommand::Attach { .. }
                    | WorkspaceCommand::Detach
                    | WorkspaceCommand::Status
                    | WorkspaceCommand::Ps
                    | WorkspaceCommand::Doctor { .. },
            }
            | Command::Scratch {
                command: ScratchCommand::Status,
                ..
            }
    )
}

fn command_is_gc(command: &Command) -> bool {
    matches!(
        command,
        Command::Gc { .. }
            | Command::Workspace {
                command: WorkspaceCommand::Gc { .. },
            }
    )
}

fn command_skips_workspace_init_check(command: &Command) -> bool {
    command_uses_cwd_directly(command) || command_is_gc(command)
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

enum PatchCommandRoute<'a> {
    List {
        candidates: bool,
        all: bool,
        property: &'a Option<String>,
        producer: &'a Option<String>,
    },
    FromScratch(&'a crate::candidate::CandidateFromScratchArgs),
    Show {
        id: &'a str,
        evidence: bool,
        change: bool,
    },
    Incoming,
    TopLevelAlias(Command),
}

fn route_patch_command(command: &PatchCommand) -> PatchCommandRoute<'_> {
    match command {
        PatchCommand::List {
            candidates,
            all,
            property,
            producer,
        } => PatchCommandRoute::List {
            candidates: *candidates,
            all: *all,
            property,
            producer,
        },
        PatchCommand::FromScratch(args) => PatchCommandRoute::FromScratch(args),
        PatchCommand::Show {
            id,
            evidence,
            change,
        } => PatchCommandRoute::Show {
            id,
            evidence: *evidence,
            change: *change,
        },
        PatchCommand::Incoming => PatchCommandRoute::Incoming,
        PatchCommand::Validate { id, expected } => {
            PatchCommandRoute::TopLevelAlias(Command::Validate {
                id: id.clone(),
                expected: expected.clone(),
            })
        }
        PatchCommand::Admit { id, required } => PatchCommandRoute::TopLevelAlias(Command::Admit {
            id: id.clone(),
            required: required.clone(),
        }),
        PatchCommand::Search {
            property,
            base,
            producer,
            has_evidence,
        } => PatchCommandRoute::TopLevelAlias(Command::Search {
            property: property.clone(),
            base: base.clone(),
            producer: producer.clone(),
            has_evidence: has_evidence.clone(),
        }),
        PatchCommand::Diff { from, to } => PatchCommandRoute::TopLevelAlias(Command::Diff {
            from: from.clone(),
            to: to.clone(),
        }),
        PatchCommand::Compose {
            first,
            second,
            expected,
            validate,
        } => PatchCommandRoute::TopLevelAlias(Command::Compose {
            first: first.clone(),
            second: second.clone(),
            expected: expected.clone(),
            validate: *validate,
        }),
        PatchCommand::Migrate {
            id,
            onto,
            expected,
            validate,
        } => PatchCommandRoute::TopLevelAlias(Command::Migrate {
            id: id.clone(),
            onto: onto.clone(),
            expected: expected.clone(),
            validate: *validate,
        }),
        PatchCommand::Revert {
            id,
            expected,
            validate,
        } => PatchCommandRoute::TopLevelAlias(Command::Revert {
            id: id.clone(),
            expected: expected.clone(),
            validate: *validate,
        }),
        PatchCommand::Materialize {
            id,
            dry_run,
            discard,
            as_commit,
            ref_name,
        } => PatchCommandRoute::TopLevelAlias(Command::Materialize {
            id: id.clone(),
            dry_run: *dry_run,
            discard: *discard,
            as_commit: *as_commit,
            ref_name: ref_name.clone(),
        }),
        PatchCommand::Promote {
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
        } => PatchCommandRoute::TopLevelAlias(Command::Promote {
            id: id.clone(),
            to: to.clone(),
            branch: branch.clone(),
            yes: *yes,
            required: required.clone(),
            pr: *pr,
            release: release.clone(),
            title: title.clone(),
            body: body.clone(),
            head: head.clone(),
        }),
    }
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
            property,
            producer,
        } => run_patch_list_command(store, candidates, all, property, producer),
        PatchCommandRoute::FromScratch(args) => {
            let workspace_id = workspace_id.ok_or_else(|| {
                anyhow::anyhow!(
                    "[E_NO_WORKSPACE_ID] typed daemon op requires a resolved workspace_id"
                )
            })?;
            let command = CandidateCommand::FromScratch(args.clone());
            run_candidate_command(workspace_root, workspace_id, None, &command)
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
            Command::Property { command } => run_property_command(store, command),
            Command::Candidate { command, socket } => {
                let workspace_id = workspace_id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "[E_NO_WORKSPACE_ID] typed daemon op requires a resolved workspace_id"
                    )
                })?;
                run_candidate_command(workspace_root, workspace_id, socket.as_deref(), command)
            }
            Command::Candidates {
                property,
                failed,
                producer,
            } => Ok(CommandEnvelope {
                candidates: list_candidate_summaries(store, property, *failed, producer)?,
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
            Command::Validate { id, expected } => match record_ref_kind(id)? {
                RecordRefKind::Candidate => {
                    let candidate = store
                        .read_candidate(id)
                        .with_context(|| format!("read candidate record {id}"))?;
                    let evidence_records = validate_candidate(store, &candidate, expected)?;
                    Ok(CommandEnvelope {
                        message: Some(format!("validation completed for {id}; registry unchanged")),
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
                    let evidence_records = validate_patch(store, &patch, expected)?;
                    Ok(CommandEnvelope {
                        message: Some(format!("validation completed for admitted patch {id}")),
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
                ensure_change_integrity(store, &config, &candidate.change)?;
                ensure_candidate_expected_properties_current(&config, &candidate)?;
                let required_properties =
                    admission_required_scoped_properties(&config, &candidate, required)?;
                let current_evidence =
                    evidence_for_current_verifiers(&config, &required_properties, &evidence, id)?;
                require_passed_scoped_evidence(&required_properties, &current_evidence, id)?;
                let mut patch = PatchRecord {
                    id: graft_core::PatchId::new("patch:pending"),
                    base_state: candidate.base_state,
                    target_state: candidate.target_state,
                    change: candidate.change,
                    properties: property_refs_for_scoped(&required_properties),
                    provenance: candidate.provenance,
                    admitted_at: OffsetDateTime::now_utc().to_string(),
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
                    "[E_OBSOLETE_CWD_VIEW] graft discard no longer writes cwd because cwd is not a managed Graft view.\n  fix: use `graft materialize <state-ref>` to inspect .worktrees/<state-slug>/, or `graft promote` to write an explicit external target."
                )
            }
            Command::Incoming => incoming_command(store),
            Command::Search {
                property,
                base,
                producer,
                has_evidence,
            } => {
                if let Some(name) = property {
                    let config = load_graft_config(store)?;
                    warn_if_property_unknown(name, &config);
                }
                let patch_ids = search_patches(store, property, base, producer, has_evidence)?;
                Ok(CommandEnvelope {
                    patch_ids,
                    ..CommandEnvelope::ok()
                })
            }
            Command::Compose {
                first,
                second,
                expected,
                validate,
            } => {
                store.init_storage()?;
                let first_patch = store.read_patch(first)?;
                let second_patch = store.read_patch(second)?;
                if first_patch.target_state != second_patch.base_state {
                    bail!(
                        "[E_COMPOSE_CONFLICT] cannot compose {first} then {second}: target({first}) = {} but base({second}) = {}; create a new candidate manually from the desired resolution",
                        state_label(&first_patch.target_state),
                        state_label(&second_patch.base_state),
                    );
                }
                let first_change = stored_change(store, &first_patch.change)?;
                let second_change = stored_change(store, &second_patch.change)?;
                let change = ChangeSet::compose(&first_change, &second_change);
                let config = load_graft_config(store)?;
                let (candidate, evidence) = write_candidate_from_change(
                    store,
                    change,
                    needs_revalidation_or(&config, expected)?,
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
                expected,
                validate,
            } => {
                store.init_storage()?;
                let patch = store.read_patch(id)?;
                let change = stored_change(store, &patch.change)?;
                let config = load_graft_config(store)?;
                let base_state = resolve_base_state(store, &config, onto)?;
                let Some(base_snapshot) = base_snapshot_for_state(store, &config, &base_state)?
                else {
                    bail!("cannot resolve base snapshot for {onto}");
                };
                let migration = migrate_change(&change, &patch, base_state, &base_snapshot)?;
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
                    needs_revalidation_or(&config, expected)?,
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
                expected,
                validate,
            } => {
                store.init_storage()?;
                let patch = store.read_patch(id)?;
                let change = stored_change(store, &patch.change)?.reversed();
                let config = load_graft_config(store)?;
                let (candidate, evidence) = write_candidate_from_change(
                    store,
                    change,
                    needs_revalidation_or(&config, expected)?,
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
                        "[E_MATERIALIZE_STATE_ONLY] graft materialize only writes an isolated inspection state under .worktrees/; use `graft promote` for Git refs, branches, PRs, or releases"
                    );
                }
                let config = load_graft_config(store)?;
                let resolved = resolve_state_ref(store, &config, id)?;
                let destination = materialize_worktree_path(store, &resolved.state);
                if !dry_run {
                    store.materialize_tree_snapshot(&resolved.snapshot, &destination)?;
                }
                Ok(CommandEnvelope {
                    message: Some(if *dry_run {
                        format!(
                            "materialization dry-run for {id}: resolved {}; would write state into {}",
                            state_label(&resolved.state),
                            destination.display()
                        )
                    } else {
                        format!(
                            "materialized {id}: resolved {} into {}",
                            state_label(&resolved.state),
                            destination.display()
                        )
                    }),
                    registry_changed: false,
                    git_changed: false,
                    ..CommandEnvelope::ok()
                })
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
                ensure_change_integrity(store, &config, &patch.change)?;
                let requirement_plan = promotion_requirement_plan(&config, required)?;
                let mut required_properties = requirement_plan.properties.clone();
                let configured_target = config.promote_targets.get(to);
                if let Some(target) = configured_target {
                    required_properties.extend(crate::requirements::scoped_properties_from_map(
                        &config,
                        &target.required_properties,
                    )?);
                }
                if *yes {
                    let current_evidence = evidence_for_current_verifiers(
                        &config,
                        &required_properties,
                        &evidence,
                        id,
                    )?;
                    require_passed_scoped_evidence(&required_properties, &current_evidence, id)?;
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
                            &format!("graft promote {id} to {to}"),
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
                            body.as_deref().unwrap_or("Created by graft promote --pr"),
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
                            property_labels_or_core_only(&required_properties),
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
                CacheCommand::Search { property, failed } => {
                    let property_filter = match property.as_deref() {
                        Some(property) => {
                            let config = load_graft_config(store)?;
                            warn_if_property_unknown(property, &config);
                            Some((property, config))
                        }
                        None => None,
                    };
                    let mut summaries = Vec::new();
                    for candidate in store.list_candidates()? {
                        let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
                        if let Some((property, config)) = property_filter.as_ref() {
                            let mut matched = false;
                            for expr in &candidate.expected {
                                if property_matches_request(config, &expr.property, property)? {
                                    matched = true;
                                    break;
                                }
                            }
                            if !matched {
                                for record in &evidence {
                                    if property_id_matches(config, &record.property, property)? {
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
            "cloned {} into {}; fetched {} files; cwd left empty; run graft incoming or graft materialize <patch>",
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
    store
        .paths()
        .root()
        .join("state")
        .join("remotes")
        .join("default")
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

fn daemon_argv_with_workspace_root(workspace_root: &Path) -> Vec<String> {
    let mut raw = std::env::args()
        .map(|arg| {
            if arg.is_empty() {
                "graft".to_string()
            } else {
                arg
            }
        })
        .collect::<Vec<_>>();
    if raw.is_empty() {
        raw.push("graft".to_string());
    }

    let mut normalized = Vec::with_capacity(raw.len() + 2);
    normalized.push(raw[0].clone());
    let mut index = 1;
    while index < raw.len() {
        let arg = &raw[index];
        if arg == "--cwd" {
            index += 2;
            continue;
        }
        if arg.starts_with("--cwd=") {
            index += 1;
            continue;
        }
        normalized.push(arg.clone());
        index += 1;
    }
    normalized.insert(1, workspace_root.display().to_string());
    normalized.insert(1, "--cwd".to_string());
    normalized
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

#[derive(Clone, Debug)]
struct ResolvedState {
    input: String,
    state: StateId,
    snapshot: TreeSnapshot,
}

fn resolve_state_ref(
    store: &GraftStore,
    config: &GraftConfig,
    reference: &str,
) -> Result<ResolvedState> {
    let state = resolve_base_state(store, config, reference)
        .with_context(|| format!("resolve state ref `{reference}`"))?;
    let snapshot = materialized_snapshot_for_state(store, config, &state)
        .with_context(|| format!("materialize snapshot for state `{}`", state_label(&state)))?;
    Ok(ResolvedState {
        input: reference.to_string(),
        state,
        snapshot,
    })
}

fn object_diff_summary(store: &GraftStore, from: &str, to: &str) -> Result<String> {
    let config = load_graft_config(store)?;
    let from_state = resolve_state_ref(store, &config, from)?;
    let to_state = resolve_state_ref(store, &config, to)?;
    let change = ChangeSet::from_snapshots(
        from_state.state.clone(),
        Some(&from_state.snapshot),
        to_state.state.clone(),
        &to_state.snapshot,
    );
    let summary = change.summary();
    let mut lines = vec![format!(
        "diff {} ({}) -> {} ({}): +{} ~{} -{}",
        from_state.input,
        state_label(&from_state.state),
        to_state.input,
        state_label(&to_state.state),
        summary.added,
        summary.modified,
        summary.deleted
    )];
    if summary.added == 0 && summary.modified == 0 && summary.deleted == 0 {
        lines.push("clean".to_string());
    }
    for file in &change.files {
        lines.push(format!("{}\t{}", file_change_symbol(file.kind), file.path));
    }
    Ok(lines.join("\n"))
}

fn run_in_state(
    store: &GraftStore,
    config: &GraftConfig,
    state_ref: &str,
    cwd: Option<&Path>,
    command: &[String],
) -> Result<CommandEnvelope> {
    let command = normalized_run_command(command)?;
    let resolved = resolve_state_ref(store, config, state_ref)?;
    let state_root = run_state_temp_root(store, &resolved.state)?;
    let run_cwd_rel = normalize_run_cwd(cwd)?;
    let run_cwd = state_root.join(&run_cwd_rel);
    store.materialize_tree_snapshot(&resolved.snapshot, &state_root)?;
    if !run_cwd.is_dir() {
        let _ = fs::remove_dir_all(&state_root);
        bail!(
            "[E_RUN_CWD_NOT_FOUND] --cwd {} is not a directory inside materialized state {}",
            run_cwd_display(&run_cwd_rel),
            state_label(&resolved.state)
        );
    }
    let output = match ProcessCommand::new(&command[0])
        .args(&command[1..])
        .current_dir(&run_cwd)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_dir_all(&state_root);
            return Err(error)
                .with_context(|| format!("run `{}` in {}", command.join(" "), run_cwd.display()));
        }
    };
    let _ = fs::remove_dir_all(&state_root);
    Ok(CommandEnvelope {
        view: Some(CommandView::Run(RunView {
            state_ref: state_ref.to_string(),
            resolved_state: state_label(&resolved.state),
            cwd: run_cwd_display(&run_cwd_rel),
            command,
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })),
        ..CommandEnvelope::ok()
    })
}

fn normalized_run_command(command: &[String]) -> Result<Vec<String>> {
    let command = if command.first().is_some_and(|arg| arg == "--") {
        &command[1..]
    } else {
        command
    };
    if command.is_empty() {
        bail!("[E_RUN_COMMAND_REQUIRED] graft run requires a command after --");
    }
    Ok(command.to_vec())
}

fn normalize_run_cwd(cwd: Option<&Path>) -> Result<PathBuf> {
    let Some(cwd) = cwd else {
        return Ok(PathBuf::new());
    };
    if cwd.as_os_str().is_empty() {
        bail!("[E_RUN_CWD_EMPTY] --cwd must not be empty");
    }
    if cwd.is_absolute() {
        bail!("[E_RUN_CWD_OUTSIDE_STATE] --cwd must be relative to the materialized state root");
    }
    let mut normalized = PathBuf::new();
    for component in cwd.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "[E_RUN_CWD_OUTSIDE_STATE] --cwd {} escapes the materialized state root",
                    cwd.display()
                );
            }
        }
    }
    Ok(normalized)
}

fn run_cwd_display(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_string()
    } else {
        path.display().to_string()
    }
}

fn run_state_temp_root(store: &GraftStore, state: &StateId) -> Result<PathBuf> {
    let parent = store.paths().cache_tmp();
    fs::create_dir_all(&parent)?;
    let slug = filesystem_safe_state_slug(state);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    for attempt in 0..100 {
        let path = parent.join(format!("run-{slug}-{}-{attempt}", now));
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "[E_RUN_TEMP_UNAVAILABLE] could not allocate temporary state directory under {}",
        parent.display()
    )
}

fn file_change_symbol(kind: FileChangeKind) -> &'static str {
    match kind {
        FileChangeKind::Added | FileChangeKind::Captured => "A",
        FileChangeKind::Modified => "M",
        FileChangeKind::Deleted => "D",
        FileChangeKind::Unchanged => "=",
    }
}

fn target_snapshot_for_patch(
    store: &GraftStore,
    config: &GraftConfig,
    patch: &PatchRecord,
) -> Result<graft_core::TreeSnapshot> {
    materialized_snapshot_for_state(store, config, &patch.target_state).with_context(|| {
        format!(
            "materialize patch {} target state {}",
            patch.id,
            state_label(&patch.target_state)
        )
    })
}

fn ensure_materialized_commit(
    git: &GixBackend,
    store: &GraftStore,
    config: &GraftConfig,
    cwd: &Path,
    patch: &PatchRecord,
    id: &str,
) -> Result<String> {
    let graft_ref = materialize_ref_name(id, None);
    match git.try_resolve_ref(cwd, &graft_ref)? {
        Some(commit_id) => Ok(commit_id),
        None => {
            let snapshot = target_snapshot_for_patch(store, config, patch)?;
            Ok(git
                .materialize_commit(
                    cwd,
                    &snapshot,
                    store.paths().object_blobs(),
                    &format!("graft promote {id}"),
                    Some(&graft_ref),
                )?
                .commit_id)
        }
    }
}

fn promote_next_action(id: &str, to: &str, pr: bool, release: Option<&str>) -> NextAction {
    let label = if pr {
        format!("graft promote {id} --to {to} --pr --yes")
    } else if let Some(tag) = release {
        format!("graft promote {id} --to {to} --release {tag} --yes")
    } else {
        format!("graft promote {id} --to {to} --yes")
    };
    NextAction::new(
        "promote.apply",
        label,
        graft_explain::NextActionKind::Dangerous,
        "applying the promotion will mutate a real Git ref / PR / release",
    )
}

fn materialize_ref_name(patch_id: &str, requested: Option<&str>) -> String {
    match requested {
        Some(name) if name.starts_with("refs/") => name.to_string(),
        Some(name) => format!(
            "refs/graft/patches/{}",
            git_ref_component_for_patch_id(name)
        ),
        None => format!(
            "refs/graft/patches/{}",
            git_ref_component_for_patch_id(patch_id)
        ),
    }
}

fn git_ref_component_for_patch_id(id: &str) -> &str {
    id.strip_prefix("patch:").unwrap_or(id)
}

fn property_id_matches(
    config: &GraftConfig,
    property: &PropertyId,
    requested: &str,
) -> Result<bool> {
    if property.as_str() == requested {
        return Ok(true);
    }
    Ok(match config.properties.get(requested) {
        Some(def) => &def.property_id()? == property,
        None => false,
    })
}

fn warn_if_property_unknown(name: &str, config: &GraftConfig) {
    if config.properties.contains_key(name) {
        return;
    }
    eprintln!("warning: property `{name}` is not declared in properties.roto");
    eprintln!("hint:    run `graft property list` for configured properties");
}

fn promote_requirement_explain_line(cwd: &Path) -> String {
    let store = GraftStore::open(cwd);
    match load_graft_config_for_explain(&store) {
        Ok(config) => match promotion_requirement_plan(&config, &[]) {
            Ok(plan) => {
                let required = property_labels_or_core_only(&plan.properties);
                format!(
                    "Promotion require source: {}; effective required properties: {}; CLI `--require` overrides this for one invocation.",
                    plan.source.label(),
                    required
                )
            }
            Err(error) => format!(
                "Promotion require source: missing; {error}. CLI `--require` can supply requirements for one invocation."
            ),
        },
        Err(error) => format!("Promotion require source: unreadable-config; {error}."),
    }
}

fn load_graft_config_for_explain(store: &GraftStore) -> Result<GraftConfig> {
    if store.paths().config().exists() {
        load_graft_config(store)
    } else {
        Ok(GraftConfig::default())
    }
}

/// Build the concept catalog used by `graft explain <id>` from the live
/// clap derive plus curated workflow topics. Every subcommand's `about`
/// becomes a concept summary, and selected lifecycle commands receive richer
/// repository-maintained elaboration for `graft_help`/agent use.
fn build_concept_catalog(cwd: &Path) -> Vec<graft_explain::explain::ConceptDoc> {
    use clap::CommandFactory;

    let promote_line = promote_requirement_explain_line(cwd);
    let mut out = Vec::new();
    let cmd = Cli::command();
    for sub in cmd.get_subcommands() {
        let id = sub.get_name().to_string();
        if id == "help" || id == "explain" {
            // `help` is auto-generated; `explain` would be self-referential.
            continue;
        }
        let summary = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
        let mut long_about = sub
            .get_long_about()
            .map(|s| s.to_string())
            .filter(|long| long != &summary);
        if let Some(curated) = graft_explain::explain::curated_concept_long_about(&id) {
            long_about = Some(curated.to_string());
        }
        if id == "promote" {
            long_about = Some(match long_about {
                Some(existing) => format!("{existing}\n{promote_line}"),
                None => promote_line.clone(),
            });
        }
        let see_also = related_concepts(&id);
        out.push(graft_explain::explain::ConceptDoc {
            id,
            summary,
            long_about,
            see_also,
        });
    }
    out.extend(graft_explain::explain::agent_help_concepts());
    out.extend(property_concepts(cwd));

    // Add a few concept-only ids that are not clap subcommands but show up in
    // diagnostic see-also references; their summaries come from inline copy.
    out.push(graft_explain::explain::ConceptDoc {
        id: "patch-integrity".to_string(),
        summary: "core invariant: applying a stored change to its base must produce its target"
            .to_string(),
        long_about: Some(
            "Patch integrity is Graft mechanism, not a workspace property. It is checked before validation, admission, materialization, and promotion; properties express additional local policy."
                .to_string(),
        ),
        see_also: vec!["validate".to_string(), "V003".to_string()],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "properties".to_string(),
        summary: "how graft.toml declares verifiable properties for candidates and patches"
            .to_string(),
        long_about: Some(
            "Each property is a top-level `fn name(app: Application) -> Property` in properties.roto. Graft derives its PropertyId from the static property name, checks, and requires; CLI flags only filter or require what the file declares."
                .to_string(),
        ),
        see_also: vec![
            "validate".to_string(),
            "admit".to_string(),
            "changed_paths_any_match".to_string(),
            "changed_paths_all_match".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "graft.toml".to_string(),
        summary:
            "project-level graft configuration: [admission.required_properties], [promotion.required_properties], [repos], [promote_targets]"
                .to_string(),
        long_about: None,
        see_also: vec![
            "admit".to_string(),
            "promote".to_string(),
            "properties".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "evidence-result.unknown".to_string(),
        summary: "evidence verdict: verifier could not decide; treat as not-yet-proven".to_string(),
        long_about: None,
        see_also: vec![
            "validate".to_string(),
            "patch-integrity".to_string(),
            "V003".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "evidence-result.failed".to_string(),
        summary: "evidence verdict: verifier observed the property violated for this candidate"
            .to_string(),
        long_about: None,
        see_also: vec!["validate".to_string(), "candidates".to_string()],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "evidence-result.passed".to_string(),
        summary: "evidence verdict: verifier observed the property holding for this candidate"
            .to_string(),
        long_about: None,
        see_also: vec!["admit".to_string(), "promote".to_string()],
    });
    out
}

fn property_labels_or_core_only(properties: &[ScopedPropertyRef]) -> String {
    if properties.is_empty() {
        "none (core integrity only)".to_string()
    } else {
        properties
            .iter()
            .map(scoped_property_label)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn require_passed_scoped_evidence(
    required: &[ScopedPropertyRef],
    evidence: &[EvidenceRecord],
    subject: &str,
) -> Result<()> {
    for property in required {
        let evidence_subject = property.evidence_subject(subject);
        let mut matching = evidence.iter().filter(|record| {
            record.subject == evidence_subject && record.property == property.property.id
        });
        let label = property.label();
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

fn property_concepts(cwd: &Path) -> Vec<graft_explain::explain::ConceptDoc> {
    let store = GraftStore::open(cwd);
    let Ok(properties) = load_property_defs(&store) else {
        return Vec::new();
    };
    properties
        .into_iter()
        .filter(|(id, _)| graft_explain::properties::metadata_for_evaluator(id).is_none())
        .map(|(id, property)| graft_explain::explain::ConceptDoc {
            id,
            summary: property_summary(&property),
            long_about: Some(property_long_about(&property)),
            see_also: property_see_also(&property),
        })
        .collect()
}

fn property_summary(spec: &graft_core::PropertySpec) -> String {
    format!(
        "configured property: {} check(s), {} require(s), severity {}",
        spec.plan.checks.len(),
        spec.plan.requires.len(),
        severity_label(&spec.severity)
    )
}

fn property_long_about(spec: &graft_core::PropertySpec) -> String {
    format!(
        "Property `{}` is loaded from properties.roto as a static v2 PropertyPlan. Graft derives its PropertyId from the property name, checks, and requires only; description, severity, and source location do not affect identity.",
        spec.name.as_str()
    )
}

fn property_see_also(_spec: &graft_core::PropertySpec) -> Vec<String> {
    vec!["properties".to_string(), "properties.roto".to_string()]
}

fn severity_label(severity: &graft_core::Severity) -> &'static str {
    match severity {
        graft_core::Severity::Blocking => "blocking",
        graft_core::Severity::Warning => "warning",
        graft_core::Severity::Info => "info",
    }
}

/// Hand-curated, single-line list of related concept ids per subcommand.
/// Kept tiny on purpose: the structural relations between commands are not
/// derivable from clap, so this is the one place where we accept manual
/// upkeep, in line with the project's "compiler-as-documentation" rule.
fn related_concepts(id: &str) -> Vec<String> {
    let pairs: &[(&str, &[&str])] = &[
        (
            "init",
            &["agent-workflow", "scratch", "candidate", "graft.toml"],
        ),
        ("scratch", &["agent-workflow", "candidate"]),
        (
            "candidate",
            &["agent-workflow", "scratch", "validate", "candidates"],
        ),
        ("candidates", &["candidate", "validate", "show"]),
        ("show", &["candidate", "evidence"]),
        (
            "validate",
            &[
                "agent-workflow",
                "candidate",
                "admit",
                "patch-integrity",
                "V003",
            ],
        ),
        (
            "admit",
            &[
                "agent-workflow",
                "validate",
                "search",
                "materialize",
                "A001",
                "A002",
            ],
        ),
        ("search", &["admit", "properties"]),
        ("compose", &["candidate", "migrate"]),
        ("migrate", &["compose"]),
        ("revert", &["candidate", "admit"]),
        ("materialize", &["agent-workflow", "admit", "promote"]),
        (
            "promote",
            &["agent-workflow", "materialize", "admit", "graft.toml"],
        ),
        ("registry", &["admit", "search"]),
        ("cache", &["candidate", "candidates"]),
        ("evidence", &["validate", "admit", "properties"]),
        ("gc", &["evidence", "candidates", "registry"]),
    ];
    pairs
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| v.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

fn incoming_command(store: &GraftStore) -> Result<CommandEnvelope> {
    let mut patches = store.list_patches()?;
    patches.sort_by_key(|patch| {
        (
            state_label(&patch.base_state),
            patch.admitted_at.clone(),
            patch.id.to_string(),
        )
    });
    let mut lines = Vec::new();
    let mut current_base: Option<String> = None;
    for patch in &patches {
        let base = state_label(&patch.base_state);
        if current_base.as_deref() != Some(base.as_str()) {
            current_base = Some(base.clone());
            lines.push(format!("base {base}"));
        }
        let evidence_refs = store.patch_evidence_index(patch.id.as_str())?;
        let local_evidence = evidence_refs
            .iter()
            .filter(|id| {
                store
                    .paths()
                    .object_evidence()
                    .join(format!("{id}.json"))
                    .exists()
            })
            .count();
        let local_status = if evidence_refs.is_empty() {
            "no evidence_refs".to_string()
        } else if local_evidence == evidence_refs.len() {
            format!("locally rebuilt {local_evidence}/{}", evidence_refs.len())
        } else {
            format!(
                "not locally rebuilt {local_evidence}/{}",
                evidence_refs.len()
            )
        };
        let properties = if patch.properties.is_empty() {
            "(no properties)".to_string()
        } else {
            patch
                .properties
                .iter()
                .map(property_label)
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(format!(
            "  - {} [{}] {}",
            patch.id, properties, local_status
        ));
    }
    if lines.is_empty() {
        lines.push("no incoming patches".to_string());
    }
    let summaries = patches
        .iter()
        .map(|patch| summarize_patch_with_evidence(store, patch, &[]))
        .collect::<Result<Vec<_>>>()?;
    Ok(CommandEnvelope {
        message: Some(lines.join(
            "
",
        )),
        patches: summaries,
        ..CommandEnvelope::ok()
    })
}

fn search_patches(
    store: &GraftStore,
    property: &Option<String>,
    base: &Option<String>,
    producer: &Option<String>,
    has_evidence: &Option<String>,
) -> Result<Vec<String>> {
    let mut patches = store.list_patches()?;
    if let Some(property) = property {
        let config = load_graft_config(store)?;
        let mut filtered = Vec::new();
        for patch in patches {
            let mut matched = false;
            for expr in &patch.properties {
                if property_matches_request(&config, expr, property)? {
                    matched = true;
                    break;
                }
            }
            if matched {
                filtered.push(patch);
            }
        }
        patches = filtered;
    }
    if let Some(base) = base {
        patches.retain(|patch| state_label(&patch.base_state).contains(base));
    }
    if let Some(producer) = producer {
        patches.retain(|patch| patch.provenance.producer == *producer);
    }
    if let Some(property) = has_evidence {
        let config = load_graft_config(store)?;
        let property = resolve_scoped_property_ref(&config, property)?;
        let mut filtered = Vec::new();
        for patch in patches {
            let evidence = store.registry_evidence_for_subject(patch.id.as_str())?;
            let evidence_subject = property.evidence_subject(patch.id.as_str());
            let mut matched = false;
            for record in &evidence {
                if record.subject == evidence_subject
                    && record.property == property.property.id
                    && record.result.satisfies_requirement()
                {
                    matched = true;
                    break;
                }
            }
            if matched {
                filtered.push(patch);
            }
        }
        patches = filtered;
    }
    patches.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(patches
        .into_iter()
        .map(|patch| patch.id.to_string())
        .collect())
}

fn stored_change(store: &GraftStore, change: &ChangeRef) -> Result<ChangeSet> {
    match change {
        ChangeRef::Stored(id) => Ok(store.read_change(id.as_str())?),
        ChangeRef::InlineSummary(summary) => {
            bail!(
                "{}",
                graft_explain::diagnostics::c002_inline_change_not_transformable(summary)
                    .format_reason()
            )
        }
    }
}

fn ensure_candidate_expected_properties_current(
    config: &GraftConfig,
    candidate: &GraftCandidate,
) -> Result<()> {
    for expected in &candidate.expected {
        let Some(current) = config.properties.get(&expected.property.name) else {
            bail!(
                "[E_PROPERTY_DRIFT] candidate expected property `{}` no longer exists in properties.roto",
                expected.label()
            );
        };
        let current_id = current.property_id()?;
        if current_id != expected.property.id {
            bail!(
                "[E_PROPERTY_DRIFT] candidate expected property `{}` drifted: candidate has {}, current property resolves to {}",
                expected.label(),
                expected.property.id,
                current_id
            );
        }
    }
    Ok(())
}

fn write_candidate_from_change(
    store: &GraftStore,
    change: ChangeSet,
    expected: Vec<ScopedPropertyRef>,
    producer: &str,
    message: Option<String>,
    validate: bool,
) -> Result<(GraftCandidate, Vec<EvidenceRecord>)> {
    let base_state = change.base_state.clone();
    let target_state = change.target_state.clone();
    let (change_id, _) = store.write_change(&change)?;
    let mut candidate = GraftCandidate {
        id: graft_core::CandidateId::new("candidate:pending"),
        base_state,
        target_state,
        change: ChangeRef::Stored(change_id),
        expected,
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
        change: ChangeSet,
        snapshot: TreeSnapshot,
    },
    Blocked {
        reasons: Vec<String>,
    },
}

fn migrate_change(
    change: &ChangeSet,
    _patch: &PatchRecord,
    base_state: StateId,
    base_snapshot: &TreeSnapshot,
) -> Result<MigrationOutcome> {
    let mut entries = base_snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut blocks = Vec::new();

    for file in &change.files {
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
    let migrated =
        ChangeSet::from_snapshots(base_state, Some(base_snapshot), target_state, &snapshot);
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
                change_view_for_ref(store, &candidate.change)?
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
                change_view_for_ref(store, &patch.change)?
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

fn summarize_candidate(store: &GraftStore, candidate: &GraftCandidate) -> Result<CandidateSummary> {
    let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
    summarize_candidate_with_evidence(store, candidate, &evidence)
}

fn summarize_candidate_with_evidence(
    store: &GraftStore,
    candidate: &GraftCandidate,
    evidence: &[EvidenceRecord],
) -> Result<CandidateSummary> {
    Ok(CandidateSummary {
        id: candidate.id.to_string(),
        base_state: state_label(&candidate.base_state),
        target_state: state_label(&candidate.target_state),
        expected: candidate
            .expected
            .iter()
            .map(scoped_property_label)
            .collect(),
        producer: candidate.provenance.producer.clone(),
        message: candidate.provenance.message.clone(),
        created_at: candidate.provenance.created_at.clone(),
        evidence: EvidenceCounts::from_records(evidence),
        change: change_view_for_ref(store, &candidate.change)?,
    })
}

fn summarize_patch_with_evidence(
    store: &GraftStore,
    patch: &PatchRecord,
    evidence: &[EvidenceRecord],
) -> Result<PatchSummary> {
    Ok(PatchSummary {
        id: patch.id.to_string(),
        base_state: state_label(&patch.base_state),
        target_state: state_label(&patch.target_state),
        properties: patch.properties.iter().map(property_label).collect(),
        producer: patch.provenance.producer.clone(),
        message: patch.provenance.message.clone(),
        admitted_at: patch.admitted_at.clone(),
        evidence: EvidenceCounts::from_records(evidence),
        change: change_view_for_ref(store, &patch.change)?,
    })
}

fn promotion_view(promotion: &PromotionRecord) -> PromotionView {
    PromotionView {
        id: promotion.id.to_string(),
        patch_id: promotion.patch_id.to_string(),
        target: promotion.target.clone(),
        dry_run: promotion.dry_run,
        status: promotion.status.clone(),
        promoted_at: promotion.promoted_at.clone(),
    }
}

fn change_view_for_ref(store: &GraftStore, change: &ChangeRef) -> Result<Option<ChangeView>> {
    match change {
        ChangeRef::Stored(id) => {
            let change = store.read_change(id.as_str())?;
            let summary = change.summary();
            Ok(Some(ChangeView {
                id: Some(id.to_string()),
                description: None,
                files: summary.files,
                added: summary.added,
                modified: summary.modified,
                deleted: summary.deleted,
                unchanged: summary.unchanged,
                captured: summary.captured,
                target_bytes: summary.target_bytes,
                sample_paths: change
                    .files
                    .iter()
                    .take(8)
                    .map(|file| file.path.clone())
                    .collect(),
            }))
        }
        ChangeRef::InlineSummary(summary) => Ok(Some(ChangeView {
            id: None,
            description: Some(summary.clone()),
            files: 0,
            added: 0,
            modified: 0,
            deleted: 0,
            unchanged: 0,
            captured: 0,
            target_bytes: 0,
            sample_paths: Vec::new(),
        })),
    }
}

fn evidence_view(record: &EvidenceRecord) -> EvidenceView {
    EvidenceView {
        id: record.id.to_string(),
        subject: record.subject.clone(),
        property: record.property.to_string(),
        verifier: record.verifier.clone(),
        result: result_label(&record.result),
        created_at: record.created_at.clone(),
    }
}

fn result_label(result: &EvidenceResult) -> String {
    match result {
        EvidenceResult::Passed => "passed".to_string(),
        EvidenceResult::Failed { reason } => format!("failed: {reason}"),
        EvidenceResult::Unknown { reason } => format!("unknown: {reason}"),
        EvidenceResult::Skipped { reason } => format!("skipped: {reason}"),
    }
}

fn state_label(state: &StateId) -> String {
    match state {
        StateId::GitTree(value) => format!("git-tree:{value}"),
        StateId::RepoTree(repo) => repo.display_ref(),
        StateId::GraftTree(value) => format!("graft-tree:{value}"),
    }
}

fn next_search_actions(patch: &PatchRecord) -> Vec<NextAction> {
    next_actions_for_patch(patch, false, false)
}

fn next_actions_for_patch(
    patch: &PatchRecord,
    materialized: bool,
    promoted: bool,
) -> Vec<NextAction> {
    let ctx = graft_explain::next_actions::PatchContext {
        id: patch.id.to_string(),
        properties: patch.properties.iter().map(property_label).collect(),
        materialized,
        promoted,
    };
    graft_explain::next_actions::next_actions_patch(&ctx)
}

fn next_actions_for_candidate(
    candidate: &GraftCandidate,
    evidence: &[EvidenceRecord],
) -> Vec<NextAction> {
    let counts = view::EvidenceCounts::from_records(evidence);
    let ctx = graft_explain::next_actions::CandidateContext {
        id: candidate.id.to_string(),
        passed: counts.passed,
        failed: counts.failed,
        unknown: counts.unknown,
        skipped: counts.skipped,
        expected_properties: candidate
            .expected
            .iter()
            .map(scoped_property_label)
            .collect(),
    };
    graft_explain::next_actions::next_actions(&ctx)
}

fn materialize_worktree_path(store: &GraftStore, state: &StateId) -> PathBuf {
    store
        .paths()
        .workspace_worktrees()
        .join(filesystem_safe_state_slug(state))
}

fn filesystem_safe_state_slug(state: &StateId) -> String {
    state_label(state)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn validate_promote_ref_args(
    to: &str,
    branch: Option<&str>,
    release: Option<&str>,
    head: Option<&str>,
) -> Result<()> {
    validate_optional_cli_ref_arg("--to", Some(to))?;
    validate_optional_cli_ref_arg("--branch", branch)?;
    validate_optional_cli_ref_arg("--release", release)?;
    validate_optional_cli_ref_arg("--head", head)?;
    Ok(())
}

fn validate_optional_cli_ref_arg(label: &str, value: Option<&str>) -> Result<()> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        bail!("{label} must not be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::render_command_human;
    use crate::workspace::{
        daemon_socket_run_dir, git_origin_url, git_origin_url_from_stdout, repo_id_for_url,
    };
    use clap::CommandFactory;
    use graft_core::{PropertyRef, PropertyScope};
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvGuard {
        key: &'static str,
        old: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old = std::env::var_os(key);
            // SAFETY: tests holding ENV_LOCK do not concurrently mutate env.
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: tests holding ENV_LOCK do not concurrently mutate env.
            unsafe {
                if let Some(old) = &self.old {
                    std::env::set_var(self.key, old);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn error_chain_text(error: anyhow::Error) -> String {
        error
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn top_level_help() -> String {
        let mut bytes = Vec::new();
        Cli::command().write_long_help(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn help_has_command_row(help: &str, command: &str) -> bool {
        help.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed == command
                || trimmed
                    .strip_prefix(command)
                    .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        })
    }

    fn scoped_test_property(name: &str) -> ScopedPropertyRef {
        ScopedPropertyRef::new(
            PropertyScope::Workspace,
            PropertyRef::new(PropertyId::new(format!("property:{name}")), name),
        )
    }

    #[test]
    fn require_passed_scoped_evidence_reports_admission_diagnostics() {
        let property = scoped_test_property("policy");
        let missing =
            require_passed_scoped_evidence(std::slice::from_ref(&property), &[], "candidate:demo")
                .unwrap_err()
                .to_string();
        assert!(missing.starts_with("[A001]"), "{missing}");
        assert!(missing.contains("workspace:policy"), "{missing}");

        let failed = EvidenceRecord::failed(
            property.evidence_subject("candidate:demo"),
            property.property.id.clone(),
            "test",
            "policy failed",
        )
        .unwrap();
        let failed_error = require_passed_scoped_evidence(&[property], &[failed], "candidate:demo")
            .unwrap_err()
            .to_string();
        assert!(failed_error.starts_with("[A002]"), "{failed_error}");
        assert!(failed_error.contains("workspace:policy"), "{failed_error}");
    }

    #[test]
    fn top_level_help_shows_new_user_commands_and_hides_legacy_entries() {
        let help = top_level_help();
        for visible in [
            "get",
            "sync",
            "workspace",
            "scratch",
            "patch",
            "repo",
            "bundle",
            "explain",
        ] {
            assert!(
                help_has_command_row(&help, visible),
                "missing {visible} in help:\n{help}"
            );
        }
        for hidden in [
            "clone",
            "init",
            "status",
            "ps",
            "doctor",
            "gc",
            "candidates",
            "validate",
            "admit",
            "registry",
            "property",
            "cache",
            "verify-pending",
            "discard",
        ] {
            assert!(
                !help_has_command_row(&help, hidden),
                "legacy {hidden} leaked into help:\n{help}"
            );
        }
    }

    #[test]
    fn new_and_hidden_compatibility_commands_parse() {
        assert!(matches!(
            Cli::try_parse_from(["graft", "get", "remote", "dir"])
                .unwrap()
                .command,
            Command::Get { .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "clone", "remote", "dir"])
                .unwrap()
                .command,
            Command::Clone { .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "sync", "remote", "--fetch-only"])
                .unwrap()
                .command,
            Command::Sync {
                remote: Some(_),
                fetch_only: true,
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "sync", "--fetch-only"])
                .unwrap()
                .command,
            Command::Sync {
                remote: None,
                fetch_only: true,
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "sync", "remote", "--on-divergence", "keep-remote"])
                .unwrap()
                .command,
            Command::Sync {
                on_divergence: OnDivergence::KeepRemote,
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "workspace", "gc", "--apply"])
                .unwrap()
                .command,
            Command::Workspace {
                command: WorkspaceCommand::Gc { apply: true, .. }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "scratch", "status"])
                .unwrap()
                .command,
            Command::Scratch {
                command: ScratchCommand::Status,
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "patch", "list", "--candidates"])
                .unwrap()
                .command,
            Command::Patch {
                command: PatchCommand::List {
                    candidates: true,
                    ..
                }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "candidate", "from-scratch", "scratch:abc"])
                .unwrap()
                .command,
            Command::Candidate {
                command: CandidateCommand::FromScratch(_),
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "candidates", "--failed", "--producer", "test"])
                .unwrap()
                .command,
            Command::Candidates {
                failed: true,
                producer: Some(_),
                ..
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "repo", "list"])
                .unwrap()
                .command,
            Command::Repo {
                command: RepoCommand::List
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "bundle", "export", "bundle.json"])
                .unwrap()
                .command,
            Command::Bundle {
                command: RegistryCommand::Export { .. }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "bundle", "import", "bundle.json"])
                .unwrap()
                .command,
            Command::Bundle {
                command: RegistryCommand::Import { .. }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "registry", "export", "bundle.json"])
                .unwrap()
                .command,
            Command::Registry {
                command: RegistryCommand::Export { .. }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["graft", "explain", "agent-workflow"])
                .unwrap()
                .command,
            Command::Explain { .. }
        ));
    }

    #[test]
    fn local_router_rejects_explain_with_diagnostic() {
        let dir = test_workspace("graft-cli-router-unknown-route-test");
        let store = GraftStore::open(&dir);
        let cli = Cli {
            command: Command::Explain {
                id: "agent-workflow".to_string(),
            },
            json: false,
            cwd: dir.clone(),
        };

        let error = LocalCommandRouter {
            cli: &cli,
            routed_workspace_id: None,
            store: &store,
            workspace_root: &dir,
            workspace_id: Some("ws:test"),
        }
        .execute()
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_ROUTE_UNKNOWN]"), "{error}");
    }

    #[test]
    fn hidden_status_alias_reports_unattached_cwd() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-status-alias-unattached-test");
        let home = test_workspace("graft-cli-status-alias-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);

        let envelope = run_local(&Cli {
            command: Command::Status,
            json: false,
            cwd: dir.clone(),
        })
        .unwrap();
        let output = envelope.message.unwrap();

        assert!(output.contains("route\t<none>"), "{output}");
        assert!(output.contains("workspace\t<none>"), "{output}");
        assert!(output.contains("workspace_id\t<none>"), "{output}");
        assert!(output.contains("daemon_state\tmissing"), "{output}");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn top_level_router_classifies_workspace_boundary_commands() {
        let cases: &[(&str, &[&str], TopLevelRoute, bool)] = &[
            (
                "get",
                &["graft", "get", "remote-store", "dst"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "sync",
                &["graft", "sync", "remote-store"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "workspace init",
                &["graft", "workspace", "init"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "workspace status",
                &["graft", "workspace", "status"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "workspace attach",
                &["graft", "workspace", "attach", "--workspace", "ws:test"],
                TopLevelRoute::WorkspaceRegistryWrite,
                false,
            ),
            (
                "workspace attach status",
                &["graft", "workspace", "attach", "--status"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "workspace detach",
                &["graft", "workspace", "detach"],
                TopLevelRoute::WorkspaceRegistryWrite,
                false,
            ),
            (
                "workspace ps",
                &["graft", "workspace", "ps"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "workspace doctor",
                &["graft", "workspace", "doctor"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "workspace gc dry-run",
                &["graft", "workspace", "gc", "--derived-only"],
                TopLevelRoute::GcPrompt { derived_only: true },
                false,
            ),
            (
                "workspace gc apply",
                &["graft", "workspace", "gc", "--apply"],
                TopLevelRoute::GcApply,
                true,
            ),
            (
                "scratch status",
                &["graft", "scratch", "status"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch read",
                &["graft", "scratch", "read", "--base", "graft:empty", "a.txt"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch write",
                &[
                    "graft",
                    "scratch",
                    "write",
                    "--base",
                    "graft:empty",
                    "a.txt",
                    "--content",
                    "hello",
                ],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch edit",
                &[
                    "graft",
                    "scratch",
                    "edit",
                    "--from",
                    "scratch:abc",
                    "a.txt",
                    "--edits",
                    "[]",
                ],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch delete",
                &[
                    "graft",
                    "scratch",
                    "delete",
                    "--from",
                    "scratch:abc",
                    "a.txt",
                ],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch diff",
                &["graft", "scratch", "diff", "scratch:a", "scratch:b"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch drop",
                &["graft", "scratch", "drop", "scratch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch pin",
                &["graft", "scratch", "pin", "scratch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "scratch unpin",
                &["graft", "scratch", "unpin", "lease:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch list",
                &["graft", "patch", "list", "--all"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch from-scratch",
                &[
                    "graft",
                    "patch",
                    "from-scratch",
                    "scratch:abc",
                    "--expect",
                    "valid_patch",
                ],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch show",
                &["graft", "patch", "show", "patch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch validate",
                &["graft", "patch", "validate", "candidate:abc"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "patch admit",
                &["graft", "patch", "admit", "candidate:abc"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "patch incoming",
                &["graft", "patch", "incoming"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch search",
                &["graft", "patch", "search", "--property", "valid_patch"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch diff",
                &["graft", "patch", "diff", "graft:empty", "patch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch compose",
                &["graft", "patch", "compose", "patch:first", "patch:second"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "patch migrate",
                &["graft", "patch", "migrate", "patch:abc", "--onto", "HEAD"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "patch revert",
                &["graft", "patch", "revert", "patch:abc"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "patch materialize",
                &["graft", "patch", "materialize", "patch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "patch promote",
                &[
                    "graft",
                    "patch",
                    "promote",
                    "patch:abc",
                    "--to",
                    "refs/heads/x",
                ],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "repo add",
                &["graft", "repo", "add", "core", "."],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "repo list",
                &["graft", "repo", "list"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "repo sync",
                &["graft", "repo", "sync"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "repo lock",
                &["graft", "repo", "lock"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "repo update",
                &["graft", "repo", "update"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "bundle export",
                &["graft", "bundle", "export", "bundle.json"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "bundle import",
                &["graft", "bundle", "import", "bundle.json"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "explain",
                &["graft", "explain", "agent-workflow"],
                TopLevelRoute::Explain,
                false,
            ),
            (
                "legacy init",
                &["graft", "init"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "legacy attach",
                &["graft", "attach", "--workspace", "ws:test"],
                TopLevelRoute::WorkspaceRegistryWrite,
                false,
            ),
            (
                "legacy attach status",
                &["graft", "attach", "--status"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "legacy detach",
                &["graft", "detach"],
                TopLevelRoute::WorkspaceRegistryWrite,
                false,
            ),
            ("legacy ps", &["graft", "ps"], TopLevelRoute::Local, false),
            (
                "legacy doctor",
                &["graft", "doctor"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "legacy clone",
                &["graft", "clone", "remote-store", "dst"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "candidate from-scratch",
                &[
                    "graft",
                    "candidate",
                    "from-scratch",
                    "scratch:abc",
                    "--expect",
                    "valid_patch",
                ],
                TopLevelRoute::Local,
                false,
            ),
            (
                "candidates",
                &["graft", "candidates", "--property", "valid_patch"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "show",
                &["graft", "show", "patch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "validate",
                &["graft", "validate", "candidate:abc"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "admit",
                &["graft", "admit", "candidate:abc"],
                TopLevelRoute::CliExec,
                true,
            ),
            ("status", &["graft", "status"], TopLevelRoute::Local, false),
            (
                "diff",
                &["graft", "diff", "graft:empty", "patch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "run",
                &["graft", "run", "patch:abc", "--", "true"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "discard",
                &["graft", "discard"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "incoming",
                &["graft", "incoming"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "search",
                &["graft", "search", "--property", "valid_patch"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "compose",
                &["graft", "compose", "patch:first", "patch:second"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "migrate",
                &["graft", "migrate", "patch:abc", "--onto", "HEAD"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "revert",
                &["graft", "revert", "patch:abc"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "materialize",
                &["graft", "materialize", "patch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "promote",
                &["graft", "promote", "patch:abc", "--to", "refs/heads/x"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "property lock",
                &["graft", "property", "lock"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "property check",
                &["graft", "property", "check"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "property list",
                &["graft", "property", "list"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "property show",
                &["graft", "property", "show", "valid_patch"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "registry export",
                &["graft", "registry", "export", "bundle.json"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "registry import",
                &["graft", "registry", "import", "bundle.json"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "cache search",
                &["graft", "cache", "search", "--property", "valid_patch"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "verify-pending",
                &["graft", "verify-pending"],
                TopLevelRoute::CliExec,
                true,
            ),
            (
                "evidence",
                &["graft", "evidence", "patch:abc"],
                TopLevelRoute::Local,
                false,
            ),
            (
                "gc dry-run",
                &["graft", "gc"],
                TopLevelRoute::GcPrompt {
                    derived_only: false,
                },
                false,
            ),
            (
                "gc apply",
                &["graft", "gc", "--apply"],
                TopLevelRoute::GcApply,
                true,
            ),
        ];

        for (name, argv, expected_route, expected_daemon_cli_exec) in cases {
            let cli = Cli::try_parse_from(argv.iter().copied())
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(
                route_top_level_command(&cli.command),
                *expected_route,
                "{name}"
            );

            assert_eq!(
                DaemonCliExecRouter::ensure_supported(&cli.command).is_ok(),
                *expected_daemon_cli_exec,
                "{name}"
            );
        }
    }

    #[test]
    fn patch_list_dispatches_default_candidates_and_all_modes() {
        let dir = test_workspace("graft-cli-patch-list-modes-test");
        fs::create_dir_all(&dir).unwrap();
        let store = GraftStore::open(&dir);
        store.init().unwrap();

        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:admitted"),
            base_state: StateId::GraftTree("tree:base".to_string()),
            target_state: StateId::GraftTree("tree:patch-target".to_string()),
            change: ChangeRef::InlineSummary("admitted patch".to_string()),
            properties: Vec::new(),
            provenance: Provenance::now("patch-producer", None),
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();
        let candidate = GraftCandidate {
            id: graft_core::CandidateId::new("candidate:queued"),
            base_state: StateId::GraftTree("tree:base".to_string()),
            target_state: StateId::GraftTree("tree:candidate-target".to_string()),
            change: ChangeRef::InlineSummary("queued candidate".to_string()),
            expected: Vec::new(),
            provenance: Provenance::now("candidate-producer", None),
        };
        store.write_candidate(&candidate).unwrap();

        let default = run_patch_list_command(&store, false, false, &None, &None).unwrap();
        assert_eq!(
            default.message.as_deref(),
            Some("listed 1 admitted patch(es)")
        );
        assert_eq!(default.patches.len(), 1);
        assert_eq!(default.patches[0].id, "patch:admitted");
        assert!(default.candidates.is_empty());

        let candidates = run_patch_list_command(&store, true, false, &None, &None).unwrap();
        assert_eq!(candidates.message.as_deref(), Some("listed 1 candidate(s)"));
        assert!(candidates.patches.is_empty());
        assert_eq!(candidates.candidates.len(), 1);
        assert_eq!(candidates.candidates[0].id, "candidate:queued");

        let all = run_patch_list_command(&store, false, true, &None, &None).unwrap();
        assert_eq!(
            all.message.as_deref(),
            Some("listed 1 admitted patch(es) and 1 candidate(s)")
        );
        assert_eq!(all.patches.len(), 1);
        assert_eq!(all.candidates.len(), 1);

        let error = run_patch_list_command(&store, true, true, &None, &None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("patch list cannot use --candidates and --all together"),
            "{error}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundle_export_import_dispatches_through_user_facing_command() {
        let _lock = env_lock();
        let source = test_workspace("graft-cli-bundle-dispatch-source-test");
        let dest = test_workspace("graft-cli-bundle-dispatch-dest-test");
        let home = test_workspace("graft-cli-bundle-dispatch-home-test");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let source_store = GraftStore::open(&source);
        let dest_store = GraftStore::open(&dest);
        source_store.init().unwrap();
        dest_store.init().unwrap();

        let bundle = source.join("bundle.json");
        let export = run_local(&Cli {
            command: Command::Bundle {
                command: RegistryCommand::Export {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: source.clone(),
        })
        .unwrap();
        assert_eq!(
            export.message,
            Some(format!("exported registry to {}", bundle.display()))
        );
        assert!(bundle.exists());

        let import = run_local(&Cli {
            command: Command::Bundle {
                command: RegistryCommand::Import {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: dest.clone(),
        })
        .unwrap();
        assert!(import.registry_changed);

        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&dest);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn property_lock_is_explicit_before_config_load_succeeds() {
        let dir = test_workspace("graft-cli-config-test");
        fs::create_dir_all(&dir).unwrap();
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let _ = fs::remove_file(dir.join("graft.lock"));

        let check_error = run_property_command(&store, &PropertyCommand::Check)
            .unwrap_err()
            .to_string();
        assert!(
            check_error.contains("[E_PROPERTY_LOCK_MISSING]"),
            "{check_error}"
        );
        assert!(!dir.join("graft.lock").exists());

        run_property_command(&store, &PropertyCommand::Lock).unwrap();
        let config = load_graft_config(&store).unwrap();

        assert!(config.properties.is_empty());
        assert!(dir.join("graft.lock").exists());
        assert!(!config.properties.contains_key("Missing"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_promote_uses_default_requirements_without_workspace_config() {
        let dir = test_workspace("graft-cli-explain-default-config-test");
        fs::create_dir_all(&dir).unwrap();

        let line = promote_requirement_explain_line(&dir);

        assert!(line.contains("Promotion require source: config"), "{line}");
        assert!(line.contains("none (core integrity only)"), "{line}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_promote_reports_unreadable_workspace_config() {
        let dir = test_workspace("graft-cli-explain-bad-config-test");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("graft.toml"), "schema = \"bad\"\n").unwrap();

        let line = promote_requirement_explain_line(&dir);

        assert!(
            line.contains("Promotion require source: unreadable-config"),
            "{line}"
        );
        assert!(!line.contains("none (core integrity only)"), "{line}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn promote_rejects_blank_ref_args_before_patch_lookup() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-promote-blank-ref-test");
        let home = test_workspace("graft-cli-promote-blank-ref-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();

        let cases = [
            (
                "--to",
                Command::Promote {
                    id: "patch:missing".to_string(),
                    to: " \t".to_string(),
                    branch: None,
                    yes: false,
                    required: Vec::new(),
                    pr: false,
                    release: None,
                    title: None,
                    body: None,
                    head: None,
                },
            ),
            (
                "--branch",
                Command::Promote {
                    id: "patch:missing".to_string(),
                    to: "release".to_string(),
                    branch: Some(" \t".to_string()),
                    yes: false,
                    required: Vec::new(),
                    pr: false,
                    release: None,
                    title: None,
                    body: None,
                    head: None,
                },
            ),
            (
                "--release",
                Command::Promote {
                    id: "patch:missing".to_string(),
                    to: "main".to_string(),
                    branch: None,
                    yes: false,
                    required: Vec::new(),
                    pr: false,
                    release: Some(" \t".to_string()),
                    title: None,
                    body: None,
                    head: None,
                },
            ),
            (
                "--head",
                Command::Promote {
                    id: "patch:missing".to_string(),
                    to: "main".to_string(),
                    branch: None,
                    yes: false,
                    required: Vec::new(),
                    pr: true,
                    release: None,
                    title: None,
                    body: None,
                    head: Some(" \t".to_string()),
                },
            ),
        ];

        for (label, command) in cases {
            let message = error_chain_text(
                run_local(&Cli {
                    command,
                    json: false,
                    cwd: dir.clone(),
                })
                .unwrap_err(),
            );

            assert!(
                message.contains(&format!("{label} must not be empty")),
                "{label}: {message}"
            );
            assert!(
                !message.contains("read patch record"),
                "{label}: promote ref validation must run before patch lookup: {message}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn materialize_rejects_git_ref_mode_before_state_resolution() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-materialize-blank-ref-test");
        let home = test_workspace("graft-cli-materialize-blank-ref-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();

        let message = error_chain_text(
            run_local(&Cli {
                command: Command::Materialize {
                    id: "patch:missing".to_string(),
                    dry_run: false,
                    discard: false,
                    as_commit: false,
                    ref_name: Some(" \t".to_string()),
                },
                json: false,
                cwd: dir.clone(),
            })
            .unwrap_err(),
        );

        assert!(message.contains("[E_MATERIALIZE_STATE_ONLY]"), "{message}");
        assert!(
            !message.contains("read patch record"),
            "materialize ref rejection must run before patch lookup: {message}"
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn git_ref_derivation_strips_patch_prefix_for_internal_defaults() {
        assert_eq!(
            materialize_ref_name("patch:abc123", None),
            "refs/graft/patches/abc123"
        );
        assert_eq!(
            materialize_ref_name("patch:ignored", Some("patch:manual")),
            "refs/graft/patches/manual"
        );
        assert_eq!(
            materialize_ref_name("patch:ignored", Some("refs/graft/custom")),
            "refs/graft/custom"
        );
        assert_eq!(
            format!("graft/{}", git_ref_component_for_patch_id("patch:abc123")),
            "graft/abc123"
        );
    }

    #[test]
    fn init_registers_workspace_and_is_idempotent() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-init-register-test");
        let home = test_workspace("graft-cli-init-register-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);

        let first = run_init_command(&store, false).unwrap();
        let second = run_init_command(&store, false).unwrap();
        let registry = RegistryStore::new(&home).list_workspaces().unwrap();

        assert!(first.registry_changed);
        assert!(second.registry_changed);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].kind, WorkspaceKind::Local);
        assert_eq!(registry[0].root, dir.canonicalize().unwrap());
        assert!(dir.join("graft.toml").exists());
        assert!(dir.join(".graft").exists());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn registry_export_import_preserves_public_evidence_refs() {
        let _lock = env_lock();
        let source = test_workspace("graft-cli-registry-export-source-test");
        let dest = test_workspace("graft-cli-registry-export-dest-test");
        let home = test_workspace("graft-cli-registry-export-home-test");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let source_store = GraftStore::open(&source);
        let dest_store = GraftStore::open(&dest);
        run_init_command(&source_store, false).unwrap();
        run_init_command(&dest_store, false).unwrap();

        let property =
            PropertyRef::new(PropertyId::new("property:registryexport"), "RegistryExport");
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:registryexport"),
            base_state: StateId::GitTree("base".to_string()),
            target_state: StateId::GraftTree("target".to_string()),
            change: ChangeRef::InlineSummary("demo".to_string()),
            properties: vec![property.clone()],
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "now".to_string(),
            },
            admitted_at: "now".to_string(),
        };
        source_store.write_patch(&patch).unwrap();
        let evidence =
            EvidenceRecord::passed(patch.id.as_str(), property.id.clone(), "test").unwrap();
        let evidence_id = evidence.id.to_string();
        source_store.write_evidence(&evidence).unwrap();
        source_store
            .append_patch_evidence_index(patch.id.as_str(), &evidence_id)
            .unwrap();

        let bundle = source.join("registry.json");
        run_local(&Cli {
            command: Command::Registry {
                command: RegistryCommand::Export {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: source.clone(),
        })
        .unwrap();
        let bundle_text = fs::read_to_string(&bundle).unwrap();
        assert!(bundle_text.contains("\"evidence_refs\""), "{bundle_text}");

        run_local(&Cli {
            command: Command::Registry {
                command: RegistryCommand::Import {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: dest.clone(),
        })
        .unwrap();

        assert_eq!(
            dest_store.patch_evidence_index(patch.id.as_str()).unwrap(),
            vec![evidence_id]
        );
        assert_eq!(
            dest_store
                .registry_evidence_for_subject(patch.id.as_str())
                .unwrap(),
            vec![evidence]
        );

        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&dest);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn registry_import_rejects_unknown_bundle_fields() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-registry-import-schema-test");
        let home = test_workspace("graft-cli-registry-import-schema-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();
        let bundle = dir.join("registry.json");
        fs::write(
            &bundle,
            r#"{"patches":[],"evidence":[],"relations":[],"promotions":[],"surprise":true}"#,
        )
        .unwrap();

        let error = run_local(&Cli {
            command: Command::Registry {
                command: RegistryCommand::Import {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: dir.clone(),
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field `surprise`"), "{error}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn registry_import_rejects_unknown_bundle_object_fields() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-registry-import-object-schema-test");
        let home = test_workspace("graft-cli-registry-import-object-schema-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();
        let bundle = dir.join("registry.json");
        fs::write(
            &bundle,
            r#"{"blobs":[{"hash":"blob:demo","bytes":[],"surprise":true}],"patches":[],"evidence":[],"relations":[],"promotions":[]}"#,
        )
        .unwrap();

        let error = run_local(&Cli {
            command: Command::Registry {
                command: RegistryCommand::Import {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: dir.clone(),
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field `surprise`"), "{error}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn registry_import_rejects_unknown_patch_record_fields() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-registry-import-patch-schema-test");
        let home = test_workspace("graft-cli-registry-import-patch-schema-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();
        let bundle = dir.join("registry.json");
        fs::write(
            &bundle,
            r#"{"patches":[{"id":"patch:demo","base_state":{"kind":"git_tree","value":"base"},"target_state":{"kind":"graft_tree","value":"tree:target"},"change":{"kind":"inline_summary","value":"demo"},"properties":[],"provenance":{"producer":"test","message":null,"created_at":"now"},"admitted_at":"now","surprise":true}],"evidence":[],"relations":[],"promotions":[]}"#,
        )
        .unwrap();

        let error = run_local(&Cli {
            command: Command::Registry {
                command: RegistryCommand::Import {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: dir.clone(),
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field `surprise`"), "{error}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn registry_import_rejects_unknown_evidence_record_fields() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-registry-import-evidence-schema-test");
        let home = test_workspace("graft-cli-registry-import-evidence-schema-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();
        let bundle = dir.join("registry.json");
        fs::write(
            &bundle,
            r#"{"patches":[],"evidence":[{"id":"evidence:demo","subject":"patch:demo","property":"property:demo","verifier":"test","result":"passed","created_at":"now","surprise":true}],"relations":[],"promotions":[]}"#,
        )
        .unwrap();

        let error = run_local(&Cli {
            command: Command::Registry {
                command: RegistryCommand::Import {
                    path: bundle.clone(),
                },
            },
            json: false,
            cwd: dir.clone(),
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field `surprise`"), "{error}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn corrupt_candidate_record_does_not_fallback_to_patch_lookup() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-corrupt-candidate-record-test");
        let home = test_workspace("graft-cli-corrupt-candidate-record-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();
        let id = "candidate:corrupt";
        fs::write(
            store.paths().cache_candidates().join(format!("{id}.json")),
            "not json",
        )
        .unwrap();

        let validate_error = run_local(&Cli {
            command: Command::Validate {
                id: id.to_string(),
                expected: Vec::new(),
            },
            json: false,
            cwd: dir.clone(),
        })
        .unwrap_err()
        .to_string();
        assert!(
            validate_error.contains("read candidate record candidate:corrupt"),
            "{validate_error}"
        );
        assert!(
            !validate_error.contains("read patch record"),
            "{validate_error}"
        );

        let show_error = show_record(&store, id, false, false)
            .unwrap_err()
            .to_string();
        assert!(
            show_error.contains("read candidate record candidate:corrupt"),
            "{show_error}"
        );
        assert!(!show_error.contains("read patch record"), "{show_error}");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_argv_rejects_non_cli_exec_commands() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-daemon-argv-reject-test");
        let home = test_workspace("graft-cli-daemon-argv-reject-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();

        for argv in [
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "scratch".to_string(),
                "status".to_string(),
            ],
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "status".to_string(),
            ],
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "property".to_string(),
                "lock".to_string(),
            ],
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "repo".to_string(),
                "list".to_string(),
            ],
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "registry".to_string(),
                "export".to_string(),
                dir.join("registry.json").display().to_string(),
            ],
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "gc".to_string(),
            ],
        ] {
            let error = run_daemon_argv_to_value_for_workspace(argv, "ws:unused")
                .unwrap_err()
                .to_string();
            assert!(error.contains("[E_CLI_EXEC_UNSUPPORTED]"), "{error}");
        }

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_argv_accepts_cli_exec_commands() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-daemon-argv-accept-test");
        let home = test_workspace("graft-cli-daemon-argv-accept-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        run_init_command(&store, false).unwrap();
        let workspace_id = local_workspace_id_for_root(&dir.canonicalize().unwrap());
        RegistryStore::new(&home)
            .ensure_workspace(&workspace_id, WorkspaceKind::Local, &dir)
            .unwrap();

        let result = run_daemon_argv_to_value_for_workspace(
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "gc".to_string(),
                "--apply".to_string(),
            ],
            &workspace_id,
        )
        .unwrap();

        assert_eq!(result["status"].as_str(), Some("ok"));
        assert_eq!(result["message"], serde_json::Value::Null);
        assert_eq!(result["view"]["type"].as_str(), Some("gc"));
        assert_eq!(result["view"]["data"]["dry_run"].as_bool(), Some(false));
        assert_eq!(
            result["view"]["data"]["workspace"]["scope"].as_str(),
            Some("workspace")
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_daemon_gc_apply_message_is_rendered_as_applied_section() {
        let envelope = CommandEnvelope {
            message: Some(
                "gc dry_run=false; deleted 7 orphan object(s): 2 evidence, 3 candidate evidence index, 2 patch evidence index"
                    .to_string(),
            ),
            ..CommandEnvelope::ok()
        };

        let envelope = modernize_legacy_gc_apply_message(envelope, false);
        let message = render_command_human(&envelope);

        assert!(envelope.message.is_none());
        assert!(message.contains("applied\n"), "{message}");
        assert!(message.contains("  orphan_objects_before: 7"), "{message}");
        assert!(message.contains("  orphan_objects_deleted: 7"), "{message}");
    }

    #[test]
    fn gc_reports_and_applies_stale_registry_records() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-gc-registry-test");
        let home = test_workspace("graft-cli-gc-registry-home");
        let live_repo = home.join("live-repo");
        let stale_route_cwd = home.join("route-to-gone");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&live_repo).unwrap();
        fs::create_dir_all(&stale_route_cwd).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:live", WorkspaceKind::Local, &dir)
            .unwrap();
        registry
            .ensure_workspace(
                "ws:gone",
                WorkspaceKind::Local,
                home.join("missing-workspace"),
            )
            .unwrap();
        registry.upsert_route(&dir, "ws:live").unwrap();
        registry.upsert_route(&stale_route_cwd, "ws:gone").unwrap();
        registry
            .upsert_repo_path("repo:demo", home.join("missing-repo"))
            .unwrap();
        registry.upsert_repo_path("repo:demo", &live_repo).unwrap();

        let dry_run_envelope = run_gc(&store, false, false).unwrap();
        assert!(dry_run_envelope.message.is_none());
        let dry_run = render_command_human(&dry_run_envelope);
        assert!(dry_run.contains("plan\n"), "{dry_run}");
        assert!(
            dry_run.contains("  stale_registry_records_before: 3"),
            "{dry_run}"
        );
        assert!(
            dry_run.contains("  stale_registry_records_to_delete: 3"),
            "{dry_run}"
        );
        assert_eq!(registry.load().unwrap().workspaces.len(), 2);

        let applied = run_gc(&store, true, false).unwrap();
        let message = render_command_human(&applied);
        assert!(message.contains("applied\n"), "{message}");
        assert!(
            message.contains("  stale_registry_records_before: 3"),
            "{message}"
        );
        assert!(
            message.contains("  stale_registry_records_deleted: 3"),
            "{message}"
        );
        assert!(applied.registry_changed);
        let registry_after = registry.load().unwrap();
        assert_eq!(registry_after.workspaces.len(), 1);
        assert_eq!(registry_after.workspaces[0].id, "ws:live");
        assert_eq!(registry_after.routes.len(), 1);
        assert_eq!(registry_after.routes[0].workspace, "ws:live");
        assert_eq!(registry_after.repo_paths.len(), 1);
        assert_eq!(
            registry_after.repo_paths[0].paths,
            vec![live_repo.canonicalize().unwrap()]
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn gc_without_workspace_still_cleans_registry() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-gc-no-workspace-cwd");
        let home = test_workspace("graft-cli-gc-no-workspace-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace(
                "ws:gone",
                WorkspaceKind::Local,
                home.join("missing-workspace"),
            )
            .unwrap();
        let store = GraftStore::open(&cwd);

        let output = run_gc(&store, true, false).unwrap();

        assert!(output.message.is_none());
        let message = render_command_human(&output);
        assert!(
            message.contains("  workspace_objects: skipped (no initialized workspace)"),
            "{message}"
        );
        assert!(output.registry_changed);
        assert!(registry.load().unwrap().workspaces.is_empty());
        assert!(!cwd.join(".graft").exists());

        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn gc_dry_run_reports_invalid_workspace_env_instead_of_falling_back() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-gc-invalid-env-cwd");
        let home = test_workspace("graft-cli-gc-invalid-env-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _home_guard = EnvGuard::set("GRAFT_HOME", &home);
        let _workspace_guard = EnvGuard::set("GRAFT_WORKSPACE", home.join("not-a-workspace"));
        let cli = Cli {
            command: Command::Workspace {
                command: WorkspaceCommand::Gc {
                    apply: false,
                    derived_only: false,
                },
            },
            json: false,
            cwd: cwd.clone(),
        };

        let error = run_local(&cli).unwrap_err().to_string();

        assert!(
            error.contains("GRAFT_WORKSPACE=")
                && error.contains("neither a registered workspace id nor a workspace root"),
            "{error}"
        );
        assert!(
            !cwd.join(".graft").exists(),
            "invalid workspace discovery must not be treated as a registry-only GC"
        );

        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn gc_apply_reports_delete_failures() {
        let dir = test_workspace("graft-cli-gc-delete-failure-test");
        fs::create_dir_all(&dir).unwrap();
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let masquerading_dir = store.paths().object_evidence().join("bad.json");
        fs::create_dir_all(&masquerading_dir).unwrap();

        let error = run_gc(&store, true, true).unwrap_err().to_string();

        assert!(error.contains("remove gc object"), "{error}");
        assert!(
            masquerading_dir.exists(),
            "gc must not delete a directory as if it were an evidence JSON file"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_pending_reports_corrupt_patch_evidence_index() {
        let dir = test_workspace("graft-cli-verify-pending-corrupt-index-test");
        fs::create_dir_all(&dir).unwrap();
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:corrupt-index"),
            base_state: StateId::GraftTree("tree:base".to_string()),
            target_state: StateId::GraftTree("tree:target".to_string()),
            change: ChangeRef::InlineSummary("corrupt index test".to_string()),
            properties: Vec::new(),
            provenance: Provenance::now("test", None),
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();
        fs::create_dir_all(store.paths().object_patch_evidence_index()).unwrap();
        fs::write(
            store
                .paths()
                .object_patch_evidence_index()
                .join(format!("{}.json", patch.id)),
            "not json",
        )
        .unwrap();

        let error = verify_pending_command(&store, None, None)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("expected ident") || error.contains("expected value"),
            "{error}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incoming_reports_corrupt_patch_evidence_index() {
        let dir = test_workspace("graft-cli-incoming-corrupt-index-test");
        fs::create_dir_all(&dir).unwrap();
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:corrupt-incoming-index"),
            base_state: StateId::GraftTree("tree:base".to_string()),
            target_state: StateId::GraftTree("tree:target".to_string()),
            change: ChangeRef::InlineSummary("corrupt incoming index test".to_string()),
            properties: Vec::new(),
            provenance: Provenance::now("test", None),
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();
        fs::create_dir_all(store.paths().object_patch_evidence_index()).unwrap();
        fs::write(
            store
                .paths()
                .object_patch_evidence_index()
                .join(format!("{}.json", patch.id)),
            "not json",
        )
        .unwrap();

        let error = incoming_command(&store).unwrap_err().to_string();

        assert!(
            error.contains("expected ident") || error.contains("expected value"),
            "{error}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn result_to_envelope_requires_command_envelope() {
        let bad_result = result_to_envelope(serde_json::json!({"status": "ok"}))
            .unwrap_err()
            .to_string();
        assert!(
            bad_result.contains("daemon result is not a command envelope"),
            "{bad_result}"
        );
    }

    #[test]
    fn result_to_envelope_rejects_unknown_top_level_fields() {
        let mut result = serde_json::to_value(CommandEnvelope::ok()).unwrap();
        result
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_string(), serde_json::json!(true));

        let error = error_chain_text(result_to_envelope(result).unwrap_err());

        assert!(error.contains("unknown field `surprise`"), "{error}");
    }

    #[test]
    fn result_to_envelope_rejects_unknown_nested_fields() {
        let mut result = serde_json::to_value(CommandEnvelope::ok()).unwrap();
        result["candidates"] = serde_json::json!([{
            "id": "candidate:demo",
            "base_state": "tree:base",
            "target_state": "tree:target",
            "expected": [],
            "producer": "test",
            "message": null,
            "created_at": "now",
            "evidence": {
                "total": 0,
                "passed": 0,
                "failed": 0,
                "unknown": 0,
                "skipped": 0
            },
            "change": null,
            "surprise": true
        }]);
        let candidate_error = error_chain_text(result_to_envelope(result).unwrap_err());
        assert!(
            candidate_error.contains("unknown field `surprise`"),
            "{candidate_error}"
        );

        let mut result = serde_json::to_value(CommandEnvelope::ok()).unwrap();
        result["next_actions"] = serde_json::json!([{
            "id": "validate",
            "label": "graft validate candidate:demo",
            "kind": "recommended",
            "why": "validate before admit",
            "surprise": true
        }]);
        let action_error = error_chain_text(result_to_envelope(result).unwrap_err());
        assert!(
            action_error.contains("unknown field `surprise`"),
            "{action_error}"
        );
    }

    #[test]
    fn daemon_argv_rejects_default_workspace_sync() {
        let _lock = env_lock();
        let home = test_workspace("graft-cli-default-sync-home");
        let default_root = home.join("workspaces/default");
        let remote = home.join("remote.git");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&default_root).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        GraftStore::open(&default_root).init().unwrap();
        RegistryStore::new(&home)
            .ensure_workspace(DEFAULT_WORKSPACE_ID, WorkspaceKind::System, &default_root)
            .unwrap();

        let error = run_daemon_argv_to_value_for_workspace(
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                default_root.display().to_string(),
                "sync".to_string(),
                remote.display().to_string(),
                "--push-only".to_string(),
            ],
            DEFAULT_WORKSPACE_ID,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_SYNC_DEFAULT_WORKSPACE]"), "{error}");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_argv_rejects_sync_when_workspace_explicitly_disables_it() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-sync-disabled-workspace");
        let home = test_workspace("graft-cli-sync-disabled-home");
        let remote = home.join("remote.git");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        fs::write(
            dir.join("graft.toml"),
            "schema = 1\n\n[sync]\nenabled = false\n",
        )
        .unwrap();
        write_property_lock(&store, &std::collections::BTreeMap::new()).unwrap();
        let workspace_id = local_workspace_id_for_root(&dir.canonicalize().unwrap());
        RegistryStore::new(&home)
            .ensure_workspace(&workspace_id, WorkspaceKind::Local, &dir)
            .unwrap();

        let error = run_daemon_argv_to_value_for_workspace(
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "sync".to_string(),
                remote.display().to_string(),
                "--push-only".to_string(),
            ],
            &workspace_id,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_SYNC_DISABLED]"), "{error}");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_argv_sync_uses_recorded_default_remote() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-sync-default-remote-workspace");
        let home = test_workspace("graft-cli-sync-default-remote-home");
        let remote = home.join("remote.git");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        write_property_lock(&store, &std::collections::BTreeMap::new()).unwrap();
        let workspace_id = local_workspace_id_for_root(&dir.canonicalize().unwrap());
        RegistryStore::new(&home)
            .ensure_workspace(&workspace_id, WorkspaceKind::Local, &dir)
            .unwrap();
        let default_remote = default_sync_remote_path(&store);
        fs::create_dir_all(default_remote.parent().unwrap()).unwrap();
        fs::write(&default_remote, format!("{}\n", remote.display())).unwrap();

        let result = run_daemon_argv_to_value_for_workspace(
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "sync".to_string(),
                "--push-only".to_string(),
            ],
            &workspace_id,
        )
        .unwrap();
        let envelope: CommandEnvelope = serde_json::from_value(result).unwrap();

        assert!(
            envelope
                .message
                .as_deref()
                .is_some_and(|message| message.contains(&remote.display().to_string())),
            "{envelope:?}"
        );
        assert!(remote.join("HEAD").exists());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_argv_sync_without_remote_requires_recorded_default() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-sync-missing-default-workspace");
        let home = test_workspace("graft-cli-sync-missing-default-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        write_property_lock(&store, &std::collections::BTreeMap::new()).unwrap();
        let workspace_id = local_workspace_id_for_root(&dir.canonicalize().unwrap());
        RegistryStore::new(&home)
            .ensure_workspace(&workspace_id, WorkspaceKind::Local, &dir)
            .unwrap();

        let error = run_daemon_argv_to_value_for_workspace(
            vec![
                "graft".to_string(),
                "--cwd".to_string(),
                dir.display().to_string(),
                "sync".to_string(),
                "--push-only".to_string(),
            ],
            &workspace_id,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_SYNC_REMOTE_REQUIRED]"), "{error}");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn init_register_only_requires_existing_workspace_and_writes_no_files() {
        let _lock = env_lock();
        let dir = test_workspace("graft-cli-register-only-test");
        let home = test_workspace("graft-cli-register-only-home");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&dir);

        assert!(run_init_command(&store, true).is_err());
        assert!(!dir.join("graft.toml").exists());
        assert!(!dir.join(".graft").exists());

        store.init().unwrap();
        run_init_command(&store, true).unwrap();
        let registry = RegistryStore::new(&home).list_workspaces().unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].root, dir.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn attach_status_and_detach_manage_routes() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-attach-route-test");
        let home = test_workspace("graft-cli-attach-route-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);

        let attached = run_attach_command(&cwd, None, false).unwrap();
        assert!(attached.registry_changed);
        assert!(attached.message.unwrap().contains("ws:default"));
        let status = run_attach_command(&cwd, None, true).unwrap();
        assert!(status.message.unwrap().contains("route"));
        assert_eq!(
            RegistryStore::new(&home)
                .lookup_workspace_for_cwd(&cwd)
                .unwrap(),
            Some(graft_store::DEFAULT_WORKSPACE_ID.to_string())
        );
        let default_root = home.join("workspaces/default");
        assert!(default_root.join("graft.lock").exists());
        let check = run_local(&Cli {
            command: Command::Property {
                command: PropertyCommand::Check,
            },
            json: false,
            cwd: cwd.clone(),
        })
        .unwrap();
        assert!(
            check.message.unwrap().contains("property lock current"),
            "attached default workspace must be usable by ordinary config readers"
        );

        let detached = run_detach_command(&cwd).unwrap();
        assert!(detached.registry_changed);
        assert_eq!(
            RegistryStore::new(&home)
                .lookup_workspace_for_cwd(&cwd)
                .unwrap(),
            None
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn repo_list_resolves_managed_cache_from_attached_workspace_root() {
        let _lock = env_lock();
        let workspace = test_workspace("graft-cli-repo-list-workspace-root");
        let attached_cwd = test_workspace("graft-cli-repo-list-attached-cwd");
        let home = test_workspace("graft-cli-repo-list-home");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&attached_cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&workspace);
        run_init_command(&store, false).unwrap();

        let mut config_text = fs::read_to_string(workspace.join("graft.toml")).unwrap();
        config_text.push_str("\n[repos.demo]\nurl = \"https://example.test/demo.git\"\n");
        fs::write(workspace.join("graft.toml"), config_text).unwrap();

        let registry = RegistryStore::new(&home);
        let workspace_id = registry.list_workspaces().unwrap()[0].id.clone();
        registry.upsert_route(&attached_cwd, &workspace_id).unwrap();

        let output = run_local(&Cli {
            command: Command::Repo {
                command: RepoCommand::List,
            },
            json: false,
            cwd: attached_cwd.clone(),
        })
        .unwrap()
        .message
        .unwrap();

        let expected = workspace
            .canonicalize()
            .unwrap()
            .join(".graft/repos/demo")
            .display()
            .to_string();
        let attached_cache = attached_cwd
            .canonicalize()
            .unwrap()
            .join(".graft/repos/demo")
            .display()
            .to_string();

        assert!(output.contains(&expected), "{output}");
        assert!(!output.contains(&attached_cache), "{output}");

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&attached_cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn attach_normalizes_missing_cwd_suffix_under_existing_parent() {
        let _lock = env_lock();
        let parent = test_workspace("graft-cli-attach-missing-parent");
        let home = test_workspace("graft-cli-attach-missing-home");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let cwd = parent.join("missing").join("workspace");

        let attached = run_attach_command(&cwd, None, false).unwrap();

        assert!(attached.registry_changed);
        let expected = parent
            .canonicalize()
            .unwrap()
            .join("missing")
            .join("workspace");
        let registry = RegistryStore::new(&home);
        assert_eq!(
            registry.lookup_workspace_for_cwd(&cwd).unwrap(),
            Some(graft_store::DEFAULT_WORKSPACE_ID.to_string())
        );
        let route = registry.lookup_route_for_cwd(&cwd).unwrap().unwrap();
        assert_eq!(route.cwd, expected);

        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn attach_git_repo_records_repo_path() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-attach-git-test");
        let home = test_workspace("graft-cli-attach-git-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        run_process(std::ffi::OsStr::new("git"), &["init"], &cwd, None).unwrap();
        run_process(
            std::ffi::OsStr::new("git"),
            &[
                "remote",
                "add",
                "origin",
                "https://example.test/Owner/Repo.git",
            ],
            &cwd,
            None,
        )
        .unwrap();

        run_attach_command(&cwd, None, false).unwrap();
        let repo_id = repo_id_for_url("https://example.test/Owner/Repo.git");
        assert_eq!(
            RegistryStore::new(&home)
                .lookup_paths_for_repo(&repo_id)
                .unwrap(),
            vec![cwd.canonicalize().unwrap()]
        );

        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn attach_git_subdirectory_records_repo_root_path() {
        let _lock = env_lock();
        let root = test_workspace("graft-cli-attach-git-subdir-test");
        let subdir = root.join("src").join("nested");
        let home = test_workspace("graft-cli-attach-git-subdir-home");
        fs::create_dir_all(&subdir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        run_process(std::ffi::OsStr::new("git"), &["init"], &root, None).unwrap();
        run_process(
            std::ffi::OsStr::new("git"),
            &[
                "remote",
                "add",
                "origin",
                "https://example.test/Owner/SubdirRepo.git",
            ],
            &root,
            None,
        )
        .unwrap();

        run_attach_command(&subdir, None, false).unwrap();
        let repo_id = repo_id_for_url("https://example.test/Owner/SubdirRepo.git");
        assert_eq!(
            RegistryStore::new(&home)
                .lookup_paths_for_repo(&repo_id)
                .unwrap(),
            vec![root.canonicalize().unwrap()]
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn git_origin_url_resolves_from_worktree_subdirectory() {
        let root = test_workspace("graft-cli-git-origin-subdir");
        let subdir = root.join("src").join("nested");
        fs::create_dir_all(&subdir).unwrap();
        run_process(std::ffi::OsStr::new("git"), &["init"], &root, None).unwrap();
        run_process(
            std::ffi::OsStr::new("git"),
            &[
                "remote",
                "add",
                "origin",
                "https://example.test/Owner/SubdirOrigin.git",
            ],
            &root,
            None,
        )
        .unwrap();

        assert_eq!(
            git_origin_url(&subdir).unwrap().as_deref(),
            Some("https://example.test/Owner/SubdirOrigin.git")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_origin_url_returns_none_when_origin_is_missing() {
        let cwd = test_workspace("graft-cli-git-origin-missing");
        fs::create_dir_all(&cwd).unwrap();
        run_process(std::ffi::OsStr::new("git"), &["init"], &cwd, None).unwrap();

        assert_eq!(git_origin_url(&cwd).unwrap(), None);

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn git_origin_url_rejects_git_config_errors() {
        let cwd = test_workspace("graft-cli-git-origin-invalid-config");
        fs::create_dir_all(&cwd).unwrap();
        run_process(std::ffi::OsStr::new("git"), &["init"], &cwd, None).unwrap();
        fs::write(cwd.join(".git/config"), "[bad").unwrap();

        let error = git_origin_url(&cwd).unwrap_err().to_string();

        assert!(error.contains("[E_GIT_ORIGIN_LOOKUP_FAILED]"), "{error}");
        assert!(error.contains("remote.origin.url"), "{error}");

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn git_origin_stdout_parses_utf8_url_and_empty_output() {
        let cwd = Path::new("/tmp/repo");

        assert_eq!(
            git_origin_url_from_stdout(cwd, b"https://example.test/Owner/Repo.git\n".to_vec())
                .unwrap()
                .as_deref(),
            Some("https://example.test/Owner/Repo.git")
        );
        assert_eq!(
            git_origin_url_from_stdout(cwd, b"\n".to_vec()).unwrap(),
            None
        );
    }

    #[test]
    fn git_origin_stdout_preserves_url_whitespace_except_line_ending() {
        let cwd = Path::new("/tmp/repo");

        assert_eq!(
            git_origin_url_from_stdout(cwd, b"https://example.test/Owner/Repo.git \n".to_vec())
                .unwrap()
                .as_deref(),
            Some("https://example.test/Owner/Repo.git ")
        );
        assert_eq!(
            git_origin_url_from_stdout(cwd, b"https://example.test/Owner/Repo.git\t\r\n".to_vec())
                .unwrap()
                .as_deref(),
            Some("https://example.test/Owner/Repo.git\t")
        );
    }

    #[test]
    fn repo_id_for_url_preserves_whitespace_identity() {
        assert_ne!(
            repo_id_for_url("https://example.test/Owner/Repo.git"),
            repo_id_for_url("https://example.test/Owner/Repo.git ")
        );
    }

    #[test]
    fn git_origin_stdout_rejects_non_utf8_url() {
        let error =
            git_origin_url_from_stdout(Path::new("/tmp/repo"), b"https://bad/\xFF\n".to_vec())
                .unwrap_err()
                .to_string();

        assert!(error.contains("[E_NON_UTF8_GIT_ORIGIN]"), "{error}");
        assert!(error.contains("remote.origin.url"), "{error}");
    }

    #[test]
    fn ps_reports_global_socket_and_registry_counts() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-ps-test");
        let home = test_workspace("graft-cli-ps-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        RegistryStore::new(&home)
            .ensure_workspace("ws:test", WorkspaceKind::Local, &cwd)
            .unwrap();

        let envelope = run_ps_command().unwrap();
        assert!(envelope.message.is_none());
        let output = render_command_human(&envelope);
        assert!(output.contains("daemon\n"), "{output}");
        assert!(output.contains("registry\n"), "{output}");
        let expected_socket = home.join("run/daemon.sock").display().to_string();
        assert!(output.contains(&expected_socket));
        assert!(output.contains("  socket_state: missing"), "{output}");
        assert!(output.contains("  workspaces: 1"), "{output}");
        assert!(output.contains("  - ws:test"), "{output}");

        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["view"]["type"].as_str(), Some("ps"));
        assert_eq!(json["view"]["data"]["registry"]["workspaces"], 1);
        assert_eq!(
            json["view"]["data"]["daemon"]["socket"].as_str(),
            Some(expected_socket.as_str())
        );
        assert_eq!(
            json["view"]["data"]["daemon"]["socket_state"].as_str(),
            Some("missing")
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn ps_reports_stale_daemon_socket_state() {
        let _lock = env_lock();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(format!("/tmp/gps-{}-{nanos}", std::process::id()));
        let cwd = root.join("w");
        let home = root.join("h");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(home.join("run")).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        RegistryStore::new(&home)
            .ensure_workspace("ws:test", WorkspaceKind::Local, &cwd)
            .unwrap();
        let socket = home.join("run/daemon.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        }

        let envelope = run_ps_command().unwrap();
        let output = render_command_human(&envelope);
        assert!(output.contains("  socket_state: stale"), "{output}");
        assert!(output.contains("  socket_exists: true"), "{output}");

        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            json["view"]["data"]["daemon"]["socket_state"].as_str(),
            Some("stale")
        );
        assert_eq!(
            json["view"]["data"]["daemon"]["socket_exists"].as_bool(),
            Some(true)
        );
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ps_hides_missing_workspaces_by_default() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-ps-hide-missing-test");
        let home = test_workspace("graft-cli-ps-hide-missing-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:live", WorkspaceKind::Local, &cwd)
            .unwrap();
        registry
            .ensure_workspace(
                "ws:gone",
                WorkspaceKind::Local,
                home.join("missing-workspace"),
            )
            .unwrap();

        let envelope = run_ps_command().unwrap();
        let output = render_command_human(&envelope);

        assert!(envelope.message.is_none());
        assert!(output.contains("  workspaces: 1"), "{output}");
        assert!(
            output.contains("  workspaces_hidden_missing: 1"),
            "{output}"
        );
        assert!(output.contains("  - ws:live"), "{output}");
        assert!(!output.contains("ws:gone"), "{output}");
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_socket_run_dir_requires_explicit_parent() {
        let error = daemon_socket_run_dir(Path::new("daemon.sock"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_SOCKET_PARENT_REQUIRED]"), "{error}");
        assert!(error.contains("daemon.sock"), "{error}");
        assert_eq!(
            daemon_socket_run_dir(Path::new("run/daemon.sock")).unwrap(),
            Path::new("run")
        );
    }

    #[test]
    fn ps_reports_corrupt_registry_instead_of_using_backup() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-ps-corrupt-registry-test");
        let home = test_workspace("graft-cli-ps-corrupt-registry-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:first", WorkspaceKind::Local, &cwd)
            .unwrap();
        registry
            .ensure_workspace("ws:second", WorkspaceKind::Local, home.join("second"))
            .unwrap();
        assert!(registry.backup_path().exists());
        fs::write(registry.registry_path(), "not = [valid").unwrap();

        let error = run_ps_command().unwrap_err().to_string();

        assert!(error.contains("toml deserialize error"), "{error}");
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn doctor_rebuild_registry_recovers_corrupt_primary() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-doctor-corrupt-registry-cwd");
        let home = test_workspace("graft-cli-doctor-corrupt-registry-home");
        let default_root = home.join("workspaces/default");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&default_root).unwrap();
        GraftStore::open(&default_root).init().unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let registry = RegistryStore::new(&home);
        fs::write(registry.registry_path(), "not = [valid").unwrap();

        let output = run_doctor_command(true).unwrap().message.unwrap();

        assert!(output.contains("rebuilt\ttrue"), "{output}");
        assert!(output.contains("workspaces\t1"), "{output}");
        assert_eq!(
            fs::read_to_string(registry.corrupt_path()).unwrap(),
            "not = [valid"
        );
        assert!(
            registry
                .get_workspace(graft_store::DEFAULT_WORKSPACE_ID)
                .unwrap()
                .is_some()
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn doctor_reports_broken_records_and_rebuilds_default_workspace() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-doctor-route-test");
        let home = test_workspace("graft-cli-doctor-home");
        let default_root = home.join("workspaces/default");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&default_root).unwrap();
        GraftStore::open(&default_root).init().unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let registry = RegistryStore::new(&home);
        registry.upsert_route(&cwd, "ws:missing").unwrap();
        registry
            .upsert_repo_path("repo:missing", home.join("missing-repo"))
            .unwrap();

        let output = run_doctor_command(true).unwrap().message.unwrap();
        assert!(output.contains("rebuilt\ttrue"));
        assert!(output.contains("workspace"));
        assert!(output.contains("route points to unknown workspace"));
        assert!(output.contains("missing repo path"));
        assert!(
            registry
                .get_workspace(graft_store::DEFAULT_WORKSPACE_ID)
                .unwrap()
                .is_some()
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn cli_validate_then_admit_consumes_v2_property_evidence() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-v2-validate-admit-test");
        let home = test_workspace("graft-cli-v2-validate-admit-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&cwd);
        run_init_command(&store, false).unwrap();
        fs::write(
            store.paths().properties_roto_config(),
            r#"
fn v2_cli_check(app: Application) -> Property {
    property(
        [app.changed_paths().any_match(["added.txt"]).success()],
        "added.txt is touched",
        Severity.Blocking,
        [],
    )
}
"#,
        )
        .unwrap();
        let defs = load_property_defs(&store).unwrap();
        let property = defs["v2_cli_check"].property_ref().unwrap();
        write_property_lock(&store, &defs).unwrap();

        let base_snapshot = TreeSnapshot::new(Vec::new());
        let target_blob = store.write_blob(b"new\n").unwrap();
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "added.txt".to_string(),
            hash: target_blob.clone(),
            size: 4,
        }]);
        let (base_tree_id, _) = store.write_tree_snapshot(&base_snapshot).unwrap();
        let (target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
        let change = ChangeSet::from_snapshots(
            StateId::GraftTree(base_tree_id.clone()),
            Some(&base_snapshot),
            StateId::GraftTree(target_tree_id.clone()),
            &target_snapshot,
        );
        let (change_id, _) = store.write_change(&change).unwrap();
        let mut candidate = GraftCandidate {
            id: graft_core::CandidateId::new("candidate:pending"),
            base_state: StateId::GraftTree(base_tree_id),
            target_state: StateId::GraftTree(target_tree_id),
            change: ChangeRef::Stored(change_id),
            expected: vec![ScopedPropertyRef::new(
                PropertyScope::Workspace,
                property.clone(),
            )],
            provenance: Provenance::now("test", None),
        };
        candidate.id = candidate_id(&candidate).unwrap();
        let candidate_id = candidate.id.to_string();
        store.write_candidate(&candidate).unwrap();

        let validate = run_local(&Cli {
            command: Command::Validate {
                id: candidate_id.clone(),
                expected: Vec::new(),
            },
            json: false,
            cwd: cwd.clone(),
        })
        .unwrap();
        assert_eq!(validate.evidence.len(), 1);
        assert_eq!(validate.evidence[0].property, property.id.as_str());
        assert_eq!(validate.evidence[0].result, "passed");
        assert!(validate.evidence[0].verifier.starts_with("v2-plan:"));

        let admit = run_local(&Cli {
            command: Command::Admit {
                id: candidate_id.clone(),
                required: Vec::new(),
            },
            json: false,
            cwd: cwd.clone(),
        })
        .unwrap();
        let patch_id = admit.patch_id.unwrap();
        let patch = store.read_patch(&patch_id).unwrap();
        assert_eq!(patch.properties, vec![property]);
        let promoted = store.registry_evidence_for_subject(&patch_id).unwrap();
        assert_eq!(promoted.len(), 1);
        assert!(matches!(promoted[0].result, EvidenceResult::Passed));
        assert!(store.read_candidate(&candidate_id).is_err());
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn materialize_writes_isolated_worktree_without_touching_cwd() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-materialize-worktree-test");
        let home = test_workspace("graft-cli-materialize-worktree-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&cwd);
        store.init().unwrap();
        write_property_lock(&store, &std::collections::BTreeMap::new()).unwrap();
        fs::write(cwd.join("foo.txt"), "old\n").unwrap();
        let old_blob = store.write_blob(b"old\n").unwrap();
        let new_blob = store.write_blob(b"new\n").unwrap();
        let base_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "foo.txt".to_string(),
            hash: old_blob.clone(),
            size: 4,
        }]);
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "foo.txt".to_string(),
            hash: new_blob.clone(),
            size: 4,
        }]);
        let (base_tree_id, _) = store.write_tree_snapshot(&base_snapshot).unwrap();
        let (target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
        let change = ChangeSet {
            base_state: StateId::GraftTree(base_tree_id.clone()),
            target_state: StateId::GraftTree(target_tree_id.clone()),
            files: vec![graft_core::FileChange {
                path: "foo.txt".to_string(),
                kind: FileChangeKind::Modified,
                base_hash: Some(old_blob),
                target_hash: Some(new_blob),
                base_size: Some(4),
                target_size: Some(4),
            }],
        };
        let (change_id, _) = store.write_change(&change).unwrap();
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:materialize-test"),
            base_state: StateId::GraftTree(base_tree_id),
            target_state: StateId::GraftTree(target_tree_id),
            change: ChangeRef::Stored(change_id),
            properties: Vec::new(),
            provenance: Provenance::now("test", None),
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();

        let envelope = run_local(&Cli {
            command: Command::Materialize {
                id: patch.id.to_string(),
                dry_run: false,
                discard: false,
                as_commit: false,
                ref_name: None,
            },
            json: false,
            cwd: cwd.clone(),
        })
        .unwrap();

        let destination = materialize_worktree_path(&store, &patch.target_state);
        assert!(
            envelope
                .message
                .unwrap()
                .contains(&destination.display().to_string())
        );
        assert!(envelope.patch_id.is_none());
        assert!(!envelope.registry_changed);
        assert_eq!(fs::read_to_string(cwd.join("foo.txt")).unwrap(), "old\n");
        assert_eq!(
            fs::read_to_string(destination.join("foo.txt")).unwrap(),
            "new\n"
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn materialize_rejects_as_commit() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-materialize-commit-test");
        let home = test_workspace("graft-cli-materialize-commit-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        run_process(std::ffi::OsStr::new("git"), &["init"], &cwd, None).unwrap();
        run_process(
            std::ffi::OsStr::new("git"),
            &["config", "user.email", "graft@example.test"],
            &cwd,
            None,
        )
        .unwrap();
        run_process(
            std::ffi::OsStr::new("git"),
            &["config", "user.name", "Graft Test"],
            &cwd,
            None,
        )
        .unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&cwd);
        run_init_command(&store, false).unwrap();
        write_property_lock(&store, &std::collections::BTreeMap::new()).unwrap();

        let base_snapshot = TreeSnapshot::new(Vec::new());
        let target_blob = store.write_blob(b"detached\n").unwrap();
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "detached.txt".to_string(),
            hash: target_blob.clone(),
            size: 9,
        }]);
        let (base_tree_id, _) = store.write_tree_snapshot(&base_snapshot).unwrap();
        let (target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
        let change = ChangeSet {
            base_state: StateId::GraftTree(base_tree_id.clone()),
            target_state: StateId::GraftTree(target_tree_id.clone()),
            files: vec![graft_core::FileChange {
                path: "detached.txt".to_string(),
                kind: FileChangeKind::Added,
                base_hash: None,
                target_hash: Some(target_blob),
                base_size: None,
                target_size: Some(9),
            }],
        };
        let (change_id, _) = store.write_change(&change).unwrap();
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:materialize-commit-test"),
            base_state: StateId::GraftTree(base_tree_id),
            target_state: StateId::GraftTree(target_tree_id),
            change: ChangeRef::Stored(change_id),
            properties: Vec::new(),
            provenance: Provenance::now("test", None),
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();

        let message = error_chain_text(
            run_local(&Cli {
                command: Command::Materialize {
                    id: patch.id.to_string(),
                    dry_run: false,
                    discard: false,
                    as_commit: true,
                    ref_name: None,
                },
                json: false,
                cwd: cwd.clone(),
            })
            .unwrap_err(),
        );

        assert!(message.contains("[E_MATERIALIZE_STATE_ONLY]"), "{message}");
        assert!(!cwd.join(".git/refs/graft").exists());

        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn ensure_materialized_commit_uses_git_safe_patch_ref() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-promote-cache-ref-test");
        let home = test_workspace("graft-cli-promote-cache-ref-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        run_process(std::ffi::OsStr::new("git"), &["init"], &cwd, None).unwrap();
        run_process(
            std::ffi::OsStr::new("git"),
            &["config", "user.email", "graft@example.test"],
            &cwd,
            None,
        )
        .unwrap();
        run_process(
            std::ffi::OsStr::new("git"),
            &["config", "user.name", "Graft Test"],
            &cwd,
            None,
        )
        .unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&cwd);
        run_init_command(&store, false).unwrap();
        write_property_lock(&store, &std::collections::BTreeMap::new()).unwrap();
        let config = load_graft_config(&store).unwrap();

        let base_snapshot = TreeSnapshot::new(Vec::new());
        let target_blob = store.write_blob(b"cached\n").unwrap();
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "cached.txt".to_string(),
            hash: target_blob.clone(),
            size: 7,
        }]);
        let (base_tree_id, _) = store.write_tree_snapshot(&base_snapshot).unwrap();
        let (target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
        let change = ChangeSet {
            base_state: StateId::GraftTree(base_tree_id.clone()),
            target_state: StateId::GraftTree(target_tree_id.clone()),
            files: vec![graft_core::FileChange {
                path: "cached.txt".to_string(),
                kind: FileChangeKind::Added,
                base_hash: None,
                target_hash: Some(target_blob),
                base_size: None,
                target_size: Some(7),
            }],
        };
        let (change_id, _) = store.write_change(&change).unwrap();
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:promote-cache-test"),
            base_state: StateId::GraftTree(base_tree_id),
            target_state: StateId::GraftTree(target_tree_id),
            change: ChangeRef::Stored(change_id),
            properties: Vec::new(),
            provenance: Provenance::now("test", None),
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();

        let commit_id = ensure_materialized_commit(
            &GixBackend,
            &store,
            &config,
            &cwd,
            &patch,
            patch.id.as_str(),
        )
        .unwrap();

        let resolved = run_process(
            std::ffi::OsStr::new("git"),
            &["rev-parse", "refs/graft/patches/promote-cache-test"],
            &cwd,
            None,
        )
        .unwrap();
        assert_eq!(resolved.trim(), commit_id);
        fs::remove_dir_all(store.paths().object_blobs()).unwrap();
        let cached_commit_id = ensure_materialized_commit(
            &GixBackend,
            &store,
            &config,
            &cwd,
            &patch,
            patch.id.as_str(),
        )
        .unwrap();
        assert_eq!(cached_commit_id, commit_id);
        let invalid_typed_ref = std::process::Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "refs/graft/patches/patch:promote-cache-test",
            ])
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert!(
            !invalid_typed_ref.status.success(),
            "typed patch id must not leak into Git ref names"
        );

        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn diff_compares_explicit_objects_without_reading_cwd_view() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-object-diff-test");
        let home = test_workspace("graft-cli-object-diff-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let store = GraftStore::open(&cwd);
        store.init().unwrap();
        write_property_lock(&store, &std::collections::BTreeMap::new()).unwrap();
        fs::write(cwd.join("unrelated-cwd-file.txt"), "not part of diff\n").unwrap();

        let blob = store.write_blob(b"target\n").unwrap();
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "added.txt".to_string(),
            hash: blob,
            size: 7,
        }]);
        let (target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
        let change = ChangeSet::from_snapshots(
            StateId::GraftTree("tree:empty".to_string()),
            None,
            StateId::GraftTree(target_tree_id.clone()),
            &target_snapshot,
        );
        let (change_id, _) = store.write_change(&change).unwrap();
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:diff-test"),
            base_state: StateId::GraftTree("tree:empty".to_string()),
            target_state: StateId::GraftTree(target_tree_id),
            change: ChangeRef::Stored(change_id),
            properties: Vec::new(),
            provenance: Provenance::now("test", None),
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();

        let envelope = run_local(&Cli {
            command: Command::Diff {
                from: "graft:empty".to_string(),
                to: patch.id.to_string(),
            },
            json: false,
            cwd: cwd.clone(),
        })
        .unwrap();

        let message = envelope.message.unwrap();
        assert!(
            message.contains("diff graft:empty (graft-tree:tree:"),
            "{message}"
        );
        assert!(
            message.contains("-> patch:diff-test (graft-tree:tree:"),
            "{message}"
        );
        assert!(message.contains("+1 ~0 -0"), "{message}");
        assert!(message.contains("A\tadded.txt"));
        assert!(!message.contains("unrelated-cwd-file.txt"));

        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn discard_is_obsolete_and_does_not_write_cwd() {
        let _lock = env_lock();
        let cwd = test_workspace("graft-cli-discard-obsolete-test");
        let home = test_workspace("graft-cli-discard-obsolete-home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        GraftStore::open(&cwd).init().unwrap();
        fs::write(cwd.join("important.txt"), "keep me\n").unwrap();

        let error = run_local(&Cli {
            command: Command::Discard,
            json: false,
            cwd: cwd.clone(),
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_OBSOLETE_CWD_VIEW]"), "{error}");
        assert_eq!(
            fs::read_to_string(cwd.join("important.txt")).unwrap(),
            "keep me\n"
        );

        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn graft_toml_rejects_legacy_inline_properties() {
        let config = toml::from_str::<GraftConfig>(
            r#"
[properties.EmptyChange]
kind = "builtin"
check = "changed_paths_any_match"
"#,
        );

        assert!(config.is_err());
    }

    #[test]
    fn migration_blocks_when_modified_path_is_missing_on_new_base() {
        let change = ChangeSet {
            base_state: StateId::GraftTree("tree:old".to_string()),
            target_state: StateId::GraftTree("tree:target".to_string()),
            files: vec![graft_core::FileChange {
                path: "src/lib.rs".to_string(),
                kind: FileChangeKind::Modified,
                base_hash: Some("old".to_string()),
                target_hash: Some("new".to_string()),
                base_size: Some(3),
                target_size: Some(3),
            }],
        };
        let patch = PatchRecord {
            id: graft_core::PatchId::new("patch:test"),
            base_state: StateId::GraftTree("tree:old".to_string()),
            target_state: StateId::GraftTree("tree:target".to_string()),
            change: ChangeRef::InlineSummary("test".to_string()),
            properties: Vec::new(),
            provenance: Provenance::now("test", None),
            admitted_at: "now".to_string(),
        };
        let new_base = TreeSnapshot::new(Vec::new());

        let outcome = migrate_change(
            &change,
            &patch,
            StateId::GraftTree(new_base.id().unwrap()),
            &new_base,
        )
        .unwrap();

        let MigrationOutcome::Blocked { reasons } = outcome else {
            panic!("modified path missing on the new base must not migrate as an added file");
        };
        assert_eq!(
            reasons[0],
            "src/lib.rs: modified path is missing on new base"
        );
    }

    fn test_workspace(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{nanos}", std::process::id()))
    }
}
