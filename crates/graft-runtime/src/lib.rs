mod config;
mod repo;
mod requirements;
mod scratch;
mod validation;
mod view;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{
    GraftConfig, ensure_property_lock_current, load_graft_config, load_optional_graft_config,
    load_property_defs, property_lock_drift, read_property_lock, write_property_lock,
};
use graft_client::{request, request_or_spawn, workspace_socket_path};
use graft_core::{
    ChangeRef, ChangeSet, EvidenceRecord, EvidenceResult, FileChangeKind, GraftCandidate,
    PatchRecord, PatchRelation, PatchRelationKind, PromotionRecord, PropertyId, Provenance,
    StateId, TreeEntry, TreeSnapshot, candidate_id, patch_id, promotion_id, relation_id,
};
use graft_explain::NextAction;
use graft_policy::require_passed_evidence;
use graft_promote::GixBackend;
use graft_store::GraftStore;
use graft_sync::GraftSyncTransport;
use repo::{RepoCommand, base_snapshot_for_state, resolve_base_state, run_repo_command};
use requirements::{
    admission_required_properties, expected_properties, needs_revalidation_or, parse_properties,
    promotion_requirement_plan, property_label, property_matches,
};
use scratch::{ScratchCommand, run_scratch_command};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use validation::{evidence_for_current_verifiers, validate_candidate, validate_patch};
use view::{
    CandidateSummary, ChangeView, CommandEnvelope, EvidenceCounts, EvidenceView, PatchSummary,
    PromotionView, print_human,
};

#[derive(Parser, Debug)]
#[command(
    name = "graft",
    about = "Property-aware patch runtime for agent changes",
    long_about = "Capture, validate and admit patch candidates with explicit property obligations and evidence, isolated from .git/ until you explicitly materialize or promote."
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
    /// Initialize .graft/ store and graft.toml in the current directory
    Init,
    /// Clone a Graft storage partition into a new empty workspace
    Clone { remote: PathBuf, dir: PathBuf },
    /// Capture the worktree as a candidate and stage expected properties
    Create {
        #[arg(
            long = "expect",
            help = "Property the candidate must later satisfy (repeatable)"
        )]
        expected: Vec<String>,
        #[arg(
            long,
            help = "Short human description recorded in candidate provenance"
        )]
        message: Option<String>,
        #[arg(
            long = "from",
            help = "Base state to capture against; defaults to HEAD or [create].default_base"
        )]
        from: Option<String>,
        #[arg(long, help = "Worktree directory to snapshot; defaults to cwd")]
        worktree: Option<PathBuf>,
        #[arg(
            long,
            help = "Run validators for the expected properties immediately after capture"
        )]
        validate: bool,
        #[arg(
            long,
            default_value = "graft-cli",
            help = "Provenance producer label recorded on the candidate"
        )]
        producer: String,
    },
    /// List candidates that are not yet admitted
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
    /// Run verifiers and produce evidence for a candidate's properties
    Validate {
        /// Candidate id to validate
        id: String,
        #[arg(
            long = "expect",
            help = "Validate this property in addition to the candidate's expected set (repeatable)"
        )]
        expected: Vec<String>,
    },
    /// Admit a candidate into the registry once required evidence is present
    Admit {
        /// Candidate id to admit
        id: String,
        #[arg(
            long = "require",
            help = "Property that must have passed evidence before admission (repeatable)"
        )]
        required: Vec<String>,
    },
    /// Show whether cwd matches .graft/state/cwd
    Status,
    /// Show cwd changes relative to .graft/state/cwd
    Diff,
    /// Restore cwd from .graft/state/cwd
    Discard,
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
            help = "Match patches that carry passing evidence for this property"
        )]
        has_evidence: Option<String>,
    },
    /// Compose two sequential patches into a new candidate (target(first) == base(second))
    Compose {
        /// First patch id (its target becomes the composition's base)
        first: String,
        /// Second patch id (its base must equal first's target)
        second: String,
        #[arg(
            long = "expect",
            help = "Property the composed candidate should later satisfy"
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
            help = "Property the migrated candidate should later satisfy"
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
            help = "Property the revert candidate should later satisfy"
        )]
        expected: Vec<String>,
        #[arg(long, help = "Run validators on the revert candidate immediately")]
        validate: bool,
    },
    /// Materialize an admitted patch into cwd (or, with --as-commit/--ref, as a Git object)
    Materialize {
        /// Patch id to materialize
        id: String,
        #[arg(
            long,
            help = "Plan the materialization but do not write cwd or Git objects"
        )]
        dry_run: bool,
        #[arg(long, help = "Overwrite dirty cwd view when materializing into cwd")]
        discard: bool,
        #[arg(
            long,
            help = "Write a detached Git commit object for the patch target instead of cwd"
        )]
        as_commit: bool,
        #[arg(
            long = "ref",
            help = "Also point this Git ref at the materialized commit (e.g. refs/graft/patches/<id>)"
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
    /// Synchronize public graft objects with a storage partition path
    Sync {
        /// Remote storage directory (v2 placeholder for refs/graft/* transport)
        remote: PathBuf,
        #[arg(long, help = "Only fetch remote public objects")]
        fetch_only: bool,
        #[arg(long, help = "Only push local public objects")]
        push_only: bool,
    },
    /// Manage explicit property definitions and their lockfile
    Property {
        #[command(subcommand)]
        command: PropertyCommand,
    },
    /// Manage configured repositories used by repo:<id>@<treeish> base refs
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Send scratch operations to graftd over Unix socket (daemon-only)
    Scratch {
        #[command(subcommand)]
        command: ScratchCommand,
        #[arg(long, help = "graftd Unix socket path")]
        socket: Option<PathBuf>,
    },
    /// Export or import admitted public objects as a portable bundle
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// Inspect or query private candidate state
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Rebuild missing local evidence bodies referenced by public evidence_refs
    VerifyPending {
        #[arg(long)]
        patch: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// List evidence records attached to a candidate or patch
    Evidence {
        /// Candidate or patch subject id
        subject: String,
    },
    /// Collect unreachable .graft objects; dry-run by default
    Gc {
        #[arg(
            long,
            help = "Delete unreachable objects instead of only reporting them"
        )]
        apply: bool,
        #[arg(long, help = "Clear only store/derived evidence bodies")]
        derived_only: bool,
    },
    /// Run an interactive tutorial through init, create, validate, admit and dry-run promote
    Learn {
        #[arg(
            long,
            help = "Run the tutorial without prompts (for smoke tests and demos)"
        )]
        non_interactive: bool,
        #[arg(long, help = "Keep the temporary tutorial sandbox after the run")]
        keep_sandbox: bool,
    },
    /// Explain a concept id, diagnostic code, or builtin property name
    Explain {
        /// Identifier to explain (e.g. `admit`, `V003`, `valid_patch`)
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum PropertyCommand {
    /// Rebuild graft.lock from properties/*.toml
    Lock,
    /// Check that graft.lock matches properties/*.toml
    Check,
    /// List configured properties and locked ids
    List,
    /// Show one configured property and locked id
    Show {
        /// Property name to show
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum RegistryCommand {
    /// Export admitted public objects as a JSON bundle to the given path
    Export {
        /// Output path for the JSON registry bundle
        path: PathBuf,
    },
    /// Import a previously exported registry JSON bundle into public store
    Import {
        /// Input path of the JSON registry bundle
        path: PathBuf,
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

#[derive(Debug, Deserialize, Serialize)]
struct RegistryBundle {
    #[serde(default)]
    trees: Vec<TreeObject>,
    #[serde(default)]
    changes: Vec<ChangeObject>,
    #[serde(default)]
    blobs: Vec<BlobObject>,
    patches: Vec<PatchRecord>,
    evidence: Vec<EvidenceRecord>,
    relations: Vec<PatchRelation>,
    promotions: Vec<PromotionRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TreeObject {
    id: String,
    snapshot: TreeSnapshot,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChangeObject {
    id: String,
    change: ChangeSet,
}

#[derive(Debug, Deserialize, Serialize)]
struct BlobObject {
    hash: String,
    bytes: Vec<u8>,
}

pub fn main_entry() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Explain { id } = &cli.command {
        return run_explain(id, cli.json, &cli.cwd);
    }
    if let Command::Learn {
        non_interactive,
        keep_sandbox,
    } = &cli.command
    {
        return run_learn(*non_interactive, *keep_sandbox);
    }
    let envelope = if command_runs_in_daemon(&cli.command) {
        run_via_daemon(&cli)?
    } else {
        run_local(&cli)?
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
pub fn run_daemon_argv_to_value(argv: Vec<String>) -> Result<serde_json::Value> {
    let cli = Cli::try_parse_from(argv)?;
    if let Command::Explain { id } = &cli.command {
        run_explain(id, cli.json, &cli.cwd)?;
        return Ok(serde_json::json!({"status":"ok"}));
    }
    if let Command::Learn { .. } = &cli.command {
        bail!("learn is an interactive/tutorial command and must run in the frontend");
    }
    Ok(serde_json::to_value(run_local(&cli)?)?)
}

fn run_via_daemon(cli: &Cli) -> Result<CommandEnvelope> {
    let store = GraftStore::open(&cli.cwd);
    ensure_no_git_workspace(&cli.cwd)?;
    ensure_workspace_initialized(&store, &cli.cwd)?;
    let socket = workspace_socket_path(&cli.cwd);
    let argv = std::env::args()
        .map(|arg| {
            if arg.is_empty() {
                "graft".to_string()
            } else {
                arg
            }
        })
        .collect::<Vec<_>>();
    let response = request_or_spawn(
        &cli.cwd,
        &socket,
        "cli_exec",
        serde_json::json!({ "argv": argv }),
    )?;
    wire_response_to_envelope(response)
}

fn wire_response_to_envelope(response: serde_json::Value) -> Result<CommandEnvelope> {
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let error = response.get("error").cloned().unwrap_or_default();
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("E_DAEMON");
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("daemon command failed");
        bail!("{code}: {message}");
    }
    let result = response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("daemon response missing result"))?;
    Ok(serde_json::from_value(result)?)
}

fn command_runs_in_daemon(command: &Command) -> bool {
    match command {
        Command::Create { .. }
        | Command::Validate { .. }
        | Command::Admit { .. }
        | Command::Compose { .. }
        | Command::Migrate { .. }
        | Command::Revert { .. }
        | Command::Discard
        | Command::Materialize { .. }
        | Command::Promote { .. }
        | Command::Sync { .. }
        | Command::VerifyPending { .. }
        | Command::Gc { apply: true, .. }
        | Command::Registry {
            command: RegistryCommand::Import { .. },
        } => true,
        Command::Repo {
            command:
                RepoCommand::Add { .. }
                | RepoCommand::Sync { .. }
                | RepoCommand::Lock { .. }
                | RepoCommand::Update { .. },
        } => true,
        Command::Scratch { .. } => false,
        Command::Property { .. } => false,
        Command::Init
        | Command::Clone { .. }
        | Command::Candidates { .. }
        | Command::Show { .. }
        | Command::Status
        | Command::Diff
        | Command::Incoming
        | Command::Search { .. }
        | Command::Repo {
            command: RepoCommand::List,
        }
        | Command::Registry {
            command: RegistryCommand::Export { .. },
        }
        | Command::Cache { .. }
        | Command::Evidence { .. }
        | Command::Gc { apply: false, .. }
        | Command::Learn { .. }
        | Command::Explain { .. } => false,
    }
}

fn run_property_command(store: &GraftStore, command: &PropertyCommand) -> Result<CommandEnvelope> {
    let defs = load_property_defs(store)?;
    match command {
        PropertyCommand::Lock => {
            let previous = read_property_lock(store)?;
            let new_lock = write_property_lock(store, &defs)?;
            let message = if let Some(previous) = previous {
                let drift = property_lock_drift(&defs, &previous)?;
                if drift.is_clean() {
                    "property lock already current".to_string()
                } else {
                    format!("repaired graft.lock ({})", drift.summary())
                }
            } else {
                "created graft.lock".to_string()
            };
            Ok(CommandEnvelope {
                message: Some(format!(
                    "{message}; {} properties locked",
                    new_lock.properties.len()
                )),
                ..CommandEnvelope::ok()
            })
        }
        PropertyCommand::Check => {
            let lock = ensure_property_lock_current(store, &defs)?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "property lock current; {} properties locked",
                    lock.properties.len()
                )),
                ..CommandEnvelope::ok()
            })
        }
        PropertyCommand::List => {
            let lock = ensure_property_lock_current(store, &defs)?;
            let mut lines = Vec::new();
            for name in defs.keys() {
                let id = lock
                    .properties
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("<missing>");
                lines.push(format!("{name}\t{id}"));
            }
            Ok(CommandEnvelope {
                message: Some(lines.join("\n")),
                ..CommandEnvelope::ok()
            })
        }
        PropertyCommand::Show { name } => {
            let lock = ensure_property_lock_current(store, &defs)?;
            let def = defs.get(name).with_context(|| {
                format!(
                    "[E_UNKNOWN_PROPERTY] property {name} is not configured in properties/*.toml"
                )
            })?;
            let id = lock.properties.get(name).cloned().unwrap_or_else(|| {
                def.property_id()
                    .map(|id| id.to_string())
                    .unwrap_or_default()
            });
            Ok(CommandEnvelope {
                message: Some(format!(
                    "property: {name}\nid: {id}\n{}",
                    toml::to_string_pretty(def).context("serialize property definition")?
                )),
                ..CommandEnvelope::ok()
            })
        }
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

fn run_learn(non_interactive: bool, keep_sandbox: bool) -> Result<()> {
    let sandbox = std::env::temp_dir().join(format!(
        "graft-learn-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    fs::create_dir_all(&sandbox)
        .with_context(|| format!("create learn sandbox {}", sandbox.display()))?;

    println!("graft learn: guided compiler-as-docs walkthrough");
    println!("sandbox: {}", sandbox.display());
    if !non_interactive {
        learn_confirm("This writes .graft/ and demo files only inside the sandbox. Continue?")?;
    }

    fs::write(sandbox.join("hello.txt"), "hello\n")?;
    let graft = std::env::current_exe().context("locate current graft executable")?;

    learn_step("init", &sandbox, non_interactive)?;
    let init = run_graft(&graft, &["init"], &sandbox)?;
    print_command_output(&init);

    fs::write(sandbox.join("hello.txt"), "hello from graft learn\n")?;

    learn_step("create", &sandbox, non_interactive)?;
    let create = run_graft(
        &graft,
        &[
            "create",
            "--from",
            "graft:empty",
            "--expect",
            "ValidPatch",
            "--message",
            "learn-demo",
        ],
        &sandbox,
    )?;
    print_command_output(&create);
    let candidate = find_token_with_prefix(&create, "candidate:")
        .context("learn create produced no candidate id")?;
    println!("learn candidate: {candidate}");

    learn_step("validate", &sandbox, non_interactive)?;
    let validate = run_graft(&graft, &["validate", &candidate], &sandbox)?;
    print_command_output(&validate);

    learn_step("admit", &sandbox, non_interactive)?;
    let admit = run_graft(
        &graft,
        &["admit", &candidate, "--require", "ValidPatch"],
        &sandbox,
    )?;
    print_command_output(&admit);
    let patch =
        find_token_with_prefix(&admit, "patch:").context("learn admit produced no patch id")?;
    println!("learn patch: {patch}");

    learn_step("materialize", &sandbox, non_interactive)?;
    let materialize = run_graft(
        &graft,
        &["materialize", &patch, "--dry-run", "--discard"],
        &sandbox,
    )?;
    print_command_output(&materialize);

    learn_step("promote", &sandbox, non_interactive)?;
    let promote = run_graft(&graft, &["promote", &patch, "--to", "main"], &sandbox)?;
    print_command_output(&promote);

    println!("learn complete: candidate {candidate}; patch {patch}");
    println!("wrote in sandbox: hello.txt, .graft/, graft.toml");
    println!("skipped side effects: no user worktree files, refs, PRs or releases were changed");
    if keep_sandbox {
        println!("kept sandbox: {}", sandbox.display());
    } else {
        let socket = workspace_socket_path(&sandbox);
        let pid = fs::read_to_string(sandbox.join(".graft/run/daemon.pid")).ok();
        if socket.exists() {
            let _ = request(&socket, "shutdown", serde_json::json!({}));
            for _ in 0..50 {
                if !socket.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        if let Some(pid) = pid.as_deref().map(str::trim).filter(|pid| !pid.is_empty()) {
            let _ = ProcessCommand::new("kill").arg(pid).status();
            for _ in 0..50 {
                if !socket.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        fs::remove_dir_all(&sandbox)
            .with_context(|| format!("remove learn sandbox {}", sandbox.display()))?;
        println!("removed sandbox: {}", sandbox.display());
    }
    Ok(())
}

fn learn_step(id: &str, cwd: &Path, non_interactive: bool) -> Result<()> {
    let concepts = build_concept_catalog(cwd);
    let result = graft_explain::explain::lookup(id, &concepts);
    let summary = match result {
        graft_explain::explain::ExplainResult::Concept(c) => c.summary,
        _ => id.to_string(),
    };
    println!();
    println!("step: {id}");
    println!("explain: {summary}");
    if !non_interactive {
        learn_confirm("Press Enter to run this step, or type n to stop.")?;
    }
    Ok(())
}

fn learn_confirm(prompt: &str) -> Result<()> {
    use std::io::Write;

    print!("{prompt} ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "n" | "N" | "no" | "NO" | "No") {
        bail!("learn cancelled by user");
    }
    Ok(())
}

fn run_graft(graft: &Path, args: &[&str], cwd: &Path) -> Result<String> {
    run_process(graft.as_os_str(), args, cwd)
}

fn run_process(program: &std::ffi::OsStr, args: &[&str], cwd: &Path) -> Result<String> {
    let mut command = ProcessCommand::new(program);
    command.args(args).current_dir(cwd);
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

fn print_command_output(output: &str) {
    for line in output.lines() {
        println!("  {line}");
    }
}

fn find_token_with_prefix(text: &str, prefix: &str) -> Option<String> {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':')))
        .find(|token| token.starts_with(prefix))
        .map(ToString::to_string)
}

fn run_local(cli: &Cli) -> Result<CommandEnvelope> {
    let store = GraftStore::open(&cli.cwd);
    if !matches!(
        &cli.command,
        Command::Clone { .. } | Command::Learn { .. } | Command::Explain { .. }
    ) {
        ensure_no_git_workspace(&cli.cwd)?;
    }
    if !matches!(
        &cli.command,
        Command::Init | Command::Clone { .. } | Command::Learn { .. } | Command::Explain { .. }
    ) {
        ensure_workspace_initialized(&store, &cli.cwd)?;
    }
    match &cli.command {
        Command::Init => {
            let outcome = store.init()?;
            let defs = load_property_defs(&store)?;
            let lock_created = read_property_lock(&store)?.is_none();
            ensure_property_lock_current(&store, &defs)?;
            let message = if outcome.changed() || lock_created {
                "initialized .graft, graft.toml, properties/*.toml and graft.lock"
            } else {
                "already initialized; nothing to do"
            };
            Ok(CommandEnvelope {
                message: Some(message.to_string()),
                cache_changed: outcome.layout_created,
                registry_changed: outcome.layout_created,
                ..CommandEnvelope::ok()
            })
        }
        Command::Clone { remote, dir } => clone_command(remote, dir),
        Command::Property { command } => run_property_command(&store, command),
        Command::Create {
            expected,
            message,
            from,
            worktree,
            validate,
            producer,
        } => {
            store.init_storage()?;
            let config = load_optional_graft_config(&store)?;
            ensure_create_mode_supported(&config)?;
            let from = from
                .as_deref()
                .or(config.create.default_base.as_deref())
                .unwrap_or("HEAD");
            let worktree = resolve_path(&cli.cwd, worktree.as_deref().unwrap_or(Path::new(".")));
            let expected = expected_properties(&config, expected)?;
            let base_state = resolve_base_state(&store, &config, from)?;
            let base_snapshot = base_snapshot_for_state(&store, &config, &base_state)?;
            let snapshot = store.capture_worktree_snapshot(&worktree)?;
            let (tree_id, _) = store.write_tree_snapshot(&snapshot)?;
            let target_state = StateId::GraftTree(tree_id);
            let change = ChangeSet::from_snapshots(
                base_state.clone(),
                base_snapshot.as_ref(),
                target_state.clone(),
                &snapshot,
            );
            let (change_id, _) = store.write_change(&change)?;
            let mut candidate = GraftCandidate {
                id: graft_core::CandidateId::new("candidate:pending"),
                base_state,
                target_state,
                change: ChangeRef::Stored(change_id),
                expected,
                provenance: Provenance::now(producer.clone(), message.clone()),
            };
            candidate.id = candidate_id(&candidate)?;
            store.write_candidate(&candidate)?;

            let mut evidence_records = Vec::new();
            if *validate {
                evidence_records = validate_candidate(&store, &candidate, &[])?;
            }
            let candidate_summary = summarize_candidate(&store, &candidate)?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "created candidate {}; git unchanged; registry unchanged",
                    candidate.id
                )),
                candidate_id: Some(candidate.id.to_string()),
                evidence_ids: evidence_records
                    .iter()
                    .map(|record| record.id.to_string())
                    .collect(),
                candidates: vec![candidate_summary],
                cache_changed: true,
                next_actions: next_actions_for_candidate(&candidate, &evidence_records),
                ..CommandEnvelope::ok()
            })
        }
        Command::Candidates {
            property,
            failed,
            producer,
        } => {
            if let Some(name) = property {
                let config = load_optional_graft_config(&store)?;
                warn_if_property_unknown(name, &config);
            }
            let mut summaries = Vec::new();
            for candidate in store.list_candidates()? {
                if let Some(property) = property
                    && !candidate
                        .expected
                        .iter()
                        .any(|expr| property_matches(expr, property))
                {
                    continue;
                }
                if let Some(producer) = producer
                    && candidate.provenance.producer != *producer
                {
                    continue;
                }
                let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
                if *failed
                    && !evidence
                        .iter()
                        .any(|record| matches!(&record.result, EvidenceResult::Failed { .. }))
                {
                    continue;
                }
                summaries.push(summarize_candidate_with_evidence(
                    &store, &candidate, &evidence,
                )?);
            }
            Ok(CommandEnvelope {
                candidates: summaries,
                ..CommandEnvelope::ok()
            })
        }
        Command::Show {
            id,
            evidence,
            change,
        } => show_record(&store, id, *evidence, *change),
        Command::Validate { id, expected } => match store.read_candidate(id) {
            Ok(candidate) => {
                let evidence_records = validate_candidate(&store, &candidate, expected)?;
                Ok(CommandEnvelope {
                    message: Some(format!("validation completed for {id}; registry unchanged")),
                    candidate_id: Some(id.clone()),
                    evidence_ids: evidence_records
                        .iter()
                        .map(|record| record.id.to_string())
                        .collect(),
                    evidence: evidence_records.iter().map(evidence_view).collect(),
                    cache_changed: true,
                    next_actions: {
                        let candidate = store.read_candidate(id)?;
                        next_actions_for_candidate(&candidate, &evidence_records)
                    },
                    ..CommandEnvelope::ok()
                })
            }
            Err(_) => {
                let patch = store.read_patch(id)?;
                let evidence_records = validate_patch(&store, &patch, expected)?;
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
            let config = load_graft_config(&store)?;
            ensure_candidate_expected_aliases_current(&config, &candidate)?;
            let required_properties = admission_required_properties(&config, &candidate, required)?;
            let current_evidence =
                evidence_for_current_verifiers(&config, &required_properties, &evidence)?;
            require_passed_evidence(&required_properties, &current_evidence)?;
            let mut patch = PatchRecord {
                id: graft_core::PatchId::new("patch:pending"),
                base_state: candidate.base_state,
                target_state: candidate.target_state,
                change: candidate.change,
                properties: required_properties.clone(),
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
        Command::Status => {
            let dirty = cwd_dirty(&store)?;
            Ok(CommandEnvelope {
                message: Some(if dirty {
                    "cwd dirty".to_string()
                } else {
                    "cwd clean".to_string()
                }),
                ..CommandEnvelope::ok()
            })
        }
        Command::Diff => {
            let summary = cwd_diff_summary(&store)?;
            Ok(CommandEnvelope {
                message: Some(summary),
                ..CommandEnvelope::ok()
            })
        }
        Command::Discard => {
            let Some(state) = store.read_cwd_state()? else {
                bail!("[E_NO_CWD_STATE] .graft/state/cwd is empty; nothing to discard to");
            };
            let snapshot = store.virtual_tree_for_state(&state)?;
            store.materialize_workspace_view(&snapshot)?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "discarded cwd changes; restored {}",
                    state_label(&state)
                )),
                registry_changed: true,
                ..CommandEnvelope::ok()
            })
        }
        Command::Incoming => incoming_command(&store),
        Command::Search {
            property,
            base,
            producer,
            has_evidence,
        } => {
            if let Some(name) = property {
                let config = load_optional_graft_config(&store)?;
                warn_if_property_unknown(name, &config);
            }
            let patch_ids = search_patches(&store, property, base, producer, has_evidence)?;
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
            let first_change = stored_change(&store, &first_patch.change)?;
            let second_change = stored_change(&store, &second_patch.change)?;
            let change = ChangeSet::compose(&first_change, &second_change);
            let config = load_graft_config(&store)?;
            let (candidate, evidence) = write_candidate_from_change(
                &store,
                change,
                needs_revalidation_or(&config, expected)?,
                "composer",
                Some(format!("compose {first} {second}")),
                *validate,
            )?;
            write_cache_relation(
                &store,
                PatchRelationKind::Composes,
                candidate.id.as_str(),
                vec![first.clone(), second.clone()],
            )?;
            Ok(candidate_envelope(
                &store,
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
            let change = stored_change(&store, &patch.change)?;
            let config = load_optional_graft_config(&store)?;
            let base_state = resolve_base_state(&store, &config, onto)?;
            let Some(base_snapshot) = base_snapshot_for_state(&store, &config, &base_state)? else {
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
                &store,
                migrated,
                needs_revalidation_or(&config, expected)?,
                "migrator",
                Some(format!("migrate {id} onto {onto}")),
                *validate,
            )?;
            write_cache_relation(
                &store,
                PatchRelationKind::Migrates,
                candidate.id.as_str(),
                vec![id.clone(), onto.clone()],
            )?;
            Ok(candidate_envelope(
                &store,
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
            let change = stored_change(&store, &patch.change)?.reversed();
            let config = load_graft_config(&store)?;
            let (candidate, evidence) = write_candidate_from_change(
                &store,
                change,
                needs_revalidation_or(&config, expected)?,
                "reverter",
                Some(format!("revert {id}")),
                *validate,
            )?;
            write_cache_relation(
                &store,
                PatchRelationKind::Reverts,
                candidate.id.as_str(),
                vec![id.clone()],
            )?;
            Ok(candidate_envelope(
                &store,
                candidate,
                evidence,
                "created revert candidate",
            )?)
        }
        Command::Materialize {
            id,
            dry_run,
            discard,
            as_commit,
            ref_name,
        } => {
            let patch = store.read_patch(id)?;
            let evidence = store.registry_evidence_for_subject(id)?;
            let snapshot = target_snapshot_for_patch(&store, &patch)?;
            if *as_commit || ref_name.is_some() {
                let target = materialize_target(*as_commit, ref_name.as_deref());
                if !dry_run {
                    let git = GixBackend;
                    let git_ref = materialize_ref_name(id, ref_name.as_deref());
                    let materialized = git.materialize_commit(
                        &cli.cwd,
                        &snapshot,
                        store.paths().object_blobs(),
                        &format!("graft materialize {id}"),
                        Some(&git_ref),
                    )?;
                    store.write_patch_object(&patch)?;
                    store.write_ref(&format!("graft/patches/{id}"), &materialized.commit_id)?;
                    write_registry_relation(
                        &store,
                        PatchRelationKind::Materializes,
                        id,
                        vec![materialized.commit_id.clone()],
                    )?;
                    return Ok(CommandEnvelope {
                        message: Some(format!(
                            "materialized {id} as {target}; commit {}; branch unchanged",
                            materialized.commit_id
                        )),
                        patch_id: Some(id.clone()),
                        patches: vec![summarize_patch_with_evidence(&store, &patch, &evidence)?],
                        registry_changed: true,
                        git_changed: true,
                        next_actions: next_actions_for_patch(&patch, true, false),
                        ..CommandEnvelope::ok()
                    });
                }
                return Ok(CommandEnvelope {
                    message: Some(format!(
                        "materialization dry-run for {id}: would create {target}; branch unchanged"
                    )),
                    patch_id: Some(id.clone()),
                    patches: vec![summarize_patch_with_evidence(&store, &patch, &evidence)?],
                    git_changed: false,
                    ..CommandEnvelope::ok()
                });
            }
            let dirty = cwd_dirty(&store)?;
            if dirty && !discard {
                bail!(
                    "[E_CWD_DIRTY] cwd has changes relative to .graft/state/cwd; rerun with --discard, run graft discard, or capture first"
                );
            }
            if !dry_run {
                store.materialize_workspace_view(&snapshot)?;
                store.write_cwd_state(&patch.target_state)?;
            }
            Ok(CommandEnvelope {
                message: Some(if *dry_run {
                    format!("materialization dry-run for {id}: would write patch target into cwd")
                } else {
                    format!("materialized {id} into cwd")
                }),
                patch_id: Some(id.clone()),
                patches: vec![summarize_patch_with_evidence(&store, &patch, &evidence)?],
                registry_changed: !*dry_run,
                git_changed: false,
                next_actions: next_actions_for_patch(&patch, false, false),
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
            let patch = store.read_patch(id)?;
            let evidence = store.registry_evidence_for_subject(id)?;
            let config = load_graft_config(&store)?;
            let requirement_plan = promotion_requirement_plan(&config, required)?;
            let mut required_properties = requirement_plan.properties.clone();
            let configured_target = config.promote_targets.get(to);
            if let Some(target) = configured_target {
                required_properties.extend(parse_properties(&config, &target.required_properties)?);
            }
            if *yes {
                let current_evidence =
                    evidence_for_current_verifiers(&config, &required_properties, &evidence)?;
                require_passed_evidence(&required_properties, &current_evidence)?;
                let git = GixBackend;
                if let Some(target_config) = configured_target {
                    let snapshot = target_snapshot_for_patch(&store, &patch)?;
                    let target_path = config.promote_target_path(&cli.cwd, to)?;
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
                        &store,
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
                let commit_id = ensure_materialized_commit(&git, &store, &cli.cwd, &patch, id)?;
                let (status, target, git_message) = if *pr {
                    let head_branch = head.clone().unwrap_or_else(|| format!("graft/{id}"));
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
                    &store,
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
                        required_properties
                            .iter()
                            .map(property_label)
                            .collect::<Vec<_>>()
                            .join(", "),
                        requirement_plan.source.label()
                    )),
                    patch_id: Some(id.clone()),
                    patches: vec![summarize_patch_with_evidence(&store, &patch, &evidence)?],
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
        } => {
            if *fetch_only && *push_only {
                bail!("sync cannot use --fetch-only and --push-only together");
            }
            let report = GraftSyncTransport.sync_public_store(
                store.paths().root(),
                remote,
                !*fetch_only,
                !*push_only,
            )?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "synced {}: pushed {} files, fetched {} files",
                    remote.display(),
                    report.pushed,
                    report.fetched
                )),
                registry_changed: report.fetched > 0,
                ..CommandEnvelope::ok()
            })
        }
        Command::Repo { command } => {
            let config = load_graft_config(&store)?;
            run_repo_command(&cli.cwd, &config, command)
        }
        Command::Scratch { command, socket } => {
            run_scratch_command(&cli.cwd, socket.as_deref(), command)
        }
        Command::Registry { command } => match command {
            RegistryCommand::Export { path } => {
                let bundle = RegistryBundle {
                    trees: store
                        .list_tree_objects()?
                        .into_iter()
                        .map(|(id, snapshot)| TreeObject { id, snapshot })
                        .collect(),
                    changes: store
                        .list_change_objects()?
                        .into_iter()
                        .map(|(id, change)| ChangeObject { id, change })
                        .collect(),
                    blobs: store
                        .list_blob_objects()?
                        .into_iter()
                        .map(|(hash, bytes)| BlobObject { hash, bytes })
                        .collect(),
                    patches: store.list_patches()?,
                    evidence: store.list_registry_evidence()?,
                    relations: store.list_relations()?,
                    promotions: store.list_promotions()?,
                };
                let path = resolve_path(&cli.cwd, path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, serde_json::to_vec_pretty(&bundle)?)?;
                Ok(CommandEnvelope {
                    message: Some(format!("exported registry to {}", path.display())),
                    ..CommandEnvelope::ok()
                })
            }
            RegistryCommand::Import { path } => {
                store.init_storage()?;
                let path = resolve_path(&cli.cwd, path);
                let bytes = fs::read(&path)?;
                let bundle: RegistryBundle = serde_json::from_slice(&bytes)?;
                for blob in &bundle.blobs {
                    store.write_blob_object(&blob.hash, &blob.bytes)?;
                }
                for tree in &bundle.trees {
                    let (id, _) = store.write_tree_snapshot(&tree.snapshot)?;
                    if id != tree.id {
                        bail!(
                            "{}",
                            graft_explain::diagnostics::m001_registry_tree_id_mismatch(&tree.id)
                                .format_reason()
                        );
                    }
                }
                for change in &bundle.changes {
                    let (id, _) = store.write_change(&change.change)?;
                    if id.as_str() != change.id {
                        bail!(
                            "{}",
                            graft_explain::diagnostics::m002_registry_change_id_mismatch(
                                &change.id
                            )
                            .format_reason()
                        );
                    }
                }
                for patch in &bundle.patches {
                    store.write_patch(patch)?;
                }
                for evidence in &bundle.evidence {
                    store.write_registry_evidence(evidence)?;
                }
                for relation in &bundle.relations {
                    store.write_relation(relation)?;
                }
                for promotion in &bundle.promotions {
                    store.write_promotion(promotion)?;
                }
                Ok(CommandEnvelope {
                    message: Some(format!("imported registry from {}", path.display())),
                    patch_ids: bundle
                        .patches
                        .iter()
                        .map(|patch| patch.id.to_string())
                        .collect(),
                    registry_changed: true,
                    ..CommandEnvelope::ok()
                })
            }
        },
        Command::Cache { command } => match command {
            CacheCommand::Search { property, failed } => {
                let config = if property.is_some() {
                    Some(load_optional_graft_config(&store)?)
                } else {
                    None
                };
                if let (Some(name), Some(config)) = (property, config.as_ref()) {
                    warn_if_property_unknown(name, config);
                }
                let mut summaries = Vec::new();
                for candidate in store.list_candidates()? {
                    let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
                    if let Some(property) = property
                        && !candidate
                            .expected
                            .iter()
                            .any(|expr| property_matches(expr, property))
                        && !evidence.iter().any(|record| {
                            config.as_ref().is_some_and(|config| {
                                property_id_matches(config, &record.property, property)
                            })
                        })
                    {
                        continue;
                    }
                    if *failed
                        && !evidence
                            .iter()
                            .any(|record| matches!(&record.result, EvidenceResult::Failed { .. }))
                    {
                        continue;
                    }
                    summaries.push(summarize_candidate_with_evidence(
                        &store, &candidate, &evidence,
                    )?);
                }
                Ok(CommandEnvelope {
                    candidates: summaries,
                    ..CommandEnvelope::ok()
                })
            }
        },
        Command::VerifyPending { patch, limit } => {
            verify_pending_command(&store, patch.as_deref(), *limit)
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
        } => run_gc(&store, *apply, *derived_only),
        Command::Learn { .. } | Command::Explain { .. } => {
            // Handled in main() before run() is called; this arm is
            // unreachable but keeps the match exhaustive.
            unreachable!("Command::Learn/Explain is dispatched in main()")
        }
    }
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
        let refs = store
            .patch_evidence_index(patch.id.as_str())
            .unwrap_or_default();
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
        let before = store
            .patch_evidence_index(patch.id.as_str())
            .unwrap_or_default();
        let evidence = validate_patch(store, &patch, &[])?;
        let after = store
            .patch_evidence_index(patch.id.as_str())
            .unwrap_or_default();
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

fn run_gc(store: &GraftStore, apply: bool, derived_only: bool) -> Result<CommandEnvelope> {
    store.init_storage()?;
    if derived_only {
        let derived = json_file_stems(&store.paths().object_evidence())?;
        if apply {
            for id in &derived {
                let _ = fs::remove_file(store.paths().object_evidence().join(format!("{id}.json")));
            }
        }
        let action = if apply { "deleted" } else { "would delete" };
        return Ok(CommandEnvelope {
            message: Some(format!(
                "gc dry_run={}; derived-only {action} {} evidence body file(s)",
                !apply,
                derived.len()
            )),
            registry_changed: false,
            cache_changed: apply && !derived.is_empty(),
            ..CommandEnvelope::ok()
        });
    }
    let mut reachable_evidence = BTreeSet::new();
    for candidate in store.list_candidates()? {
        for id in store.candidate_evidence_index(candidate.id.as_str())? {
            reachable_evidence.insert(id);
        }
    }
    for patch in store.list_patches()? {
        for id in store.patch_evidence_index(patch.id.as_str())? {
            reachable_evidence.insert(id);
        }
    }

    let mut orphan_evidence = Vec::new();
    for id in json_file_stems(&store.paths().object_evidence())? {
        if !reachable_evidence.contains(&id) {
            orphan_evidence.push(id);
        }
    }

    let mut orphan_candidate_indexes = Vec::new();
    let live_candidates = store
        .list_candidates()?
        .into_iter()
        .map(|candidate| candidate.id.to_string())
        .collect::<BTreeSet<_>>();
    for id in json_file_stems(&store.paths().object_candidate_evidence_index())? {
        if !live_candidates.contains(&id) {
            orphan_candidate_indexes.push(id);
        }
    }

    let mut orphan_patch_indexes = Vec::new();
    let live_patches = store
        .list_patches()?
        .into_iter()
        .map(|patch| patch.id.to_string())
        .collect::<BTreeSet<_>>();
    for id in json_file_stems(&store.paths().object_patch_evidence_index())? {
        if !live_patches.contains(&id) {
            orphan_patch_indexes.push(id);
        }
    }

    let orphan_count =
        orphan_evidence.len() + orphan_candidate_indexes.len() + orphan_patch_indexes.len();
    if apply {
        for id in &orphan_evidence {
            let _ = fs::remove_file(store.paths().object_evidence().join(format!("{id}.json")));
        }
        for id in &orphan_candidate_indexes {
            let _ = fs::remove_file(
                store
                    .paths()
                    .object_candidate_evidence_index()
                    .join(format!("{id}.json")),
            );
        }
        for id in &orphan_patch_indexes {
            let _ = fs::remove_file(
                store
                    .paths()
                    .object_patch_evidence_index()
                    .join(format!("{id}.json")),
            );
        }
    }

    let action = if apply { "deleted" } else { "would delete" };
    Ok(CommandEnvelope {
        message: Some(format!(
            "gc dry_run={}; {action} {orphan_count} orphan object(s): {} evidence, {} candidate evidence index, {} patch evidence index",
            !apply,
            orphan_evidence.len(),
            orphan_candidate_indexes.len(),
            orphan_patch_indexes.len()
        )),
        registry_changed: apply && orphan_count > 0,
        cache_changed: apply && orphan_count > 0,
        ..CommandEnvelope::ok()
    })
}

fn json_file_stems(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json")
            && let Some(stem) = path.file_stem().and_then(|value| value.to_str())
        {
            ids.push(stem.to_string());
        }
    }
    ids.sort();
    Ok(ids)
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
    store.init()?;
    let report = GraftSyncTransport.sync_public_store(store.paths().root(), remote, false, true)?;
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

fn ensure_no_git_workspace(cwd: &Path) -> Result<()> {
    if cwd.join(".git").exists() {
        bail!(
            "[E_GIT_IN_WORKSPACE] cwd root contains .git/; Graft v2 workspaces are Git-independent. Use an empty Graft workspace and configure external repos/promote_targets instead"
        );
    }
    Ok(())
}

fn ensure_workspace_initialized(store: &GraftStore, _cwd: &Path) -> Result<()> {
    if store.is_initialized() {
        return Ok(());
    }
    bail!(
        "[E_NO_CONFIG] graft.toml not found at {} — this directory is not a graft workspace.\n  fix: run `graft init` here, or pass `--cwd <dir>` pointing at an existing graft workspace",
        store.paths().config().display(),
    );
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn current_view_snapshot(store: &GraftStore) -> Result<TreeSnapshot> {
    Ok(store.capture_worktree_snapshot(store.paths().workspace())?)
}

fn cwd_expected_snapshot(store: &GraftStore) -> Result<Option<TreeSnapshot>> {
    match store.read_cwd_state()? {
        Some(state) => Ok(Some(store.virtual_tree_for_state(&state)?)),
        None => Ok(None),
    }
}

fn cwd_dirty(store: &GraftStore) -> Result<bool> {
    let current = current_view_snapshot(store)?;
    let Some(expected) = cwd_expected_snapshot(store)? else {
        return Ok(!current.entries.is_empty());
    };
    Ok(current.id()? != expected.id()?)
}

fn cwd_diff_summary(store: &GraftStore) -> Result<String> {
    let current = current_view_snapshot(store)?;
    let Some(expected) = cwd_expected_snapshot(store)? else {
        if current.entries.is_empty() {
            return Ok("cwd clean (no state)".to_string());
        }
        let change = ChangeSet::from_snapshots(
            StateId::GraftTree("tree:empty".to_string()),
            None,
            StateId::GraftTree(current.id()?),
            &current,
        );
        let summary = change.summary();
        return Ok(format!(
            "cwd dirty (no state): +{} ~{} -{}",
            summary.added, summary.modified, summary.deleted
        ));
    };
    let change = ChangeSet::from_snapshots(
        StateId::GraftTree(expected.id()?),
        Some(&expected),
        StateId::GraftTree(current.id()?),
        &current,
    );
    let summary = change.summary();
    if summary.added == 0 && summary.modified == 0 && summary.deleted == 0 {
        Ok("cwd clean".to_string())
    } else {
        Ok(format!(
            "cwd dirty: +{} ~{} -{}",
            summary.added, summary.modified, summary.deleted
        ))
    }
}

fn target_snapshot_for_patch(
    store: &GraftStore,
    patch: &PatchRecord,
) -> Result<graft_core::TreeSnapshot> {
    match &patch.target_state {
        StateId::GraftTree(id) => Ok(store.read_tree_snapshot(id)?),
        StateId::GitTree(id) => {
            bail!(
                "{}",
                graft_explain::diagnostics::m003_target_not_materializable(
                    &format!("{} (git tree)", id),
                    &format!(
                        "patch {} targets git tree; no graft snapshot stored",
                        patch.id
                    ),
                )
                .format_reason()
            )
        }
        StateId::RepoTree(repo) => {
            bail!(
                "{}",
                graft_explain::diagnostics::m003_target_not_materializable(
                    &format!("{} (repo tree)", repo.display_ref()),
                    &format!(
                        "patch {} targets repo tree; no graft snapshot stored",
                        patch.id
                    ),
                )
                .format_reason()
            )
        }
    }
}

fn ensure_materialized_commit(
    git: &GixBackend,
    store: &GraftStore,
    cwd: &Path,
    patch: &PatchRecord,
    id: &str,
) -> Result<String> {
    let graft_ref = format!("refs/graft/patches/{id}");
    match git.resolve_ref(cwd, &graft_ref) {
        Ok(commit_id) => Ok(commit_id),
        Err(_) => {
            let snapshot = target_snapshot_for_patch(store, patch)?;
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
        Some(name) => format!("refs/graft/patches/{name}"),
        None => format!("refs/graft/patches/{patch_id}"),
    }
}

fn ensure_create_mode_supported(config: &GraftConfig) -> Result<()> {
    let mode = config
        .create
        .default_mode
        .as_deref()
        .unwrap_or("cache-only");
    if mode == "cache-only" {
        Ok(())
    } else {
        bail!(
            "{}",
            graft_explain::diagnostics::c001_unsupported_create_mode(mode).format_reason()
        )
    }
}

fn property_id_matches(config: &GraftConfig, property: &PropertyId, requested: &str) -> bool {
    if property.as_str() == requested {
        return true;
    }
    config
        .properties
        .get(requested)
        .and_then(|def| def.property_id().ok())
        .is_some_and(|id| &id == property)
}

fn warn_if_property_unknown(name: &str, config: &GraftConfig) {
    if config.properties.contains_key(name) {
        return;
    }
    if graft_explain::properties::is_builtin_check(name) {
        return;
    }
    eprintln!("warning: property `{name}` is not declared in properties/*.toml");
    eprintln!("hint:    run `graft property list` for configured properties");
}

fn promote_requirement_explain_line(cwd: &Path) -> String {
    let store = GraftStore::open(cwd);
    match load_optional_graft_config(&store) {
        Ok(config) => match promotion_requirement_plan(&config, &[]) {
            Ok(plan) => {
                let required = plan
                    .properties
                    .iter()
                    .map(property_label)
                    .collect::<Vec<_>>()
                    .join(", ");
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

/// Build the concept catalog used by `graft explain <id>` from the live
/// clap derive: every subcommand's `about` becomes a concept summary, and
/// `long_about` (when distinct) becomes the elaboration. This way the
/// concept catalog has a single source of truth shared with `--help`.
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
        if id == "promote" {
            long_about = Some(match long_about {
                Some(existing) => format!("{existing} {promote_line}"),
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
    // Add a few concept-only ids that are not clap subcommands but show up in
    // diagnostic see-also references; their summaries come from inline copy.
    out.push(graft_explain::explain::ConceptDoc {
        id: "valid-patch".to_string(),
        summary:
            "builtin property: a stored patch must replay from declared base to declared target"
                .to_string(),
        long_about: None,
        see_also: vec![
            "validate".to_string(),
            "V003".to_string(),
            "valid_patch".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "properties".to_string(),
        summary: "how graft.toml declares verifiable properties for candidates and patches"
            .to_string(),
        long_about: Some(
            "Each property maps a name to a verifier (builtin check or external command). graft.toml is the single source of truth; CLI flags only filter or require what the file already declares."
                .to_string(),
        ),
        see_also: vec![
            "validate".to_string(),
            "admit".to_string(),
            "valid_patch".to_string(),
            "paths_none_match".to_string(),
            "paths_all_match".to_string(),
            "has_change".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "graft.toml".to_string(),
        summary:
            "project-level graft configuration: [create], [admission], [promotion], [properties.*]"
                .to_string(),
        long_about: None,
        see_also: vec![
            "create".to_string(),
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
            "valid-patch".to_string(),
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

/// Hand-curated, single-line list of related concept ids per subcommand.
/// Kept tiny on purpose: the structural relations between commands are not
/// derivable from clap, so this is the one place where we accept manual
/// upkeep, in line with the project's "compiler-as-documentation" rule.
fn related_concepts(id: &str) -> Vec<String> {
    let pairs: &[(&str, &[&str])] = &[
        ("init", &["create", "graft.toml"]),
        ("create", &["validate", "candidates", "graft.toml"]),
        ("candidates", &["create", "validate", "show"]),
        ("show", &["create", "evidence"]),
        ("validate", &["create", "admit", "valid_patch", "V003"]),
        (
            "admit",
            &["validate", "search", "materialize", "A001", "A002"],
        ),
        ("search", &["admit", "properties"]),
        ("compose", &["create", "migrate"]),
        ("migrate", &["compose"]),
        ("revert", &["create", "admit"]),
        ("materialize", &["admit", "promote"]),
        ("promote", &["materialize", "admit", "graft.toml"]),
        ("registry", &["admit", "search"]),
        ("cache", &["create", "candidates"]),
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
    let cwd_state = store.read_cwd_state()?.map(|state| state_label(&state));
    let mut lines = Vec::new();
    let mut current_base: Option<String> = None;
    for patch in &patches {
        let base = state_label(&patch.base_state);
        if current_base.as_deref() != Some(base.as_str()) {
            current_base = Some(base.clone());
            let marker = if cwd_state.as_deref() == Some(base.as_str()) {
                " (cwd)"
            } else {
                ""
            };
            lines.push(format!("base {base}{marker}"));
        }
        let evidence_refs = store
            .patch_evidence_index(patch.id.as_str())
            .unwrap_or_default();
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
        patches.retain(|patch| {
            patch
                .properties
                .iter()
                .any(|expr| property_matches(expr, property))
        });
    }
    if let Some(base) = base {
        patches.retain(|patch| state_label(&patch.base_state).contains(base));
    }
    if let Some(producer) = producer {
        patches.retain(|patch| patch.provenance.producer == *producer);
    }
    if let Some(property) = has_evidence {
        let config = load_graft_config(store)?;
        let mut filtered = Vec::new();
        for patch in patches {
            let evidence = store.registry_evidence_for_subject(patch.id.as_str())?;
            if evidence.iter().any(|record| {
                property_id_matches(&config, &record.property, property)
                    && record.result.satisfies_requirement()
            }) {
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

fn ensure_candidate_expected_aliases_current(
    config: &GraftConfig,
    candidate: &GraftCandidate,
) -> Result<()> {
    for expected in &candidate.expected {
        let Some(current) = config.properties.get(&expected.name) else {
            bail!(
                "[E_PROPERTY_DRIFT] candidate expected property `{}` no longer exists in properties/*.toml",
                expected.name
            );
        };
        let current_id = current.property_id()?;
        if current_id != expected.id {
            bail!(
                "[E_PROPERTY_DRIFT] candidate expected property `{}` drifted: candidate has {}, current alias resolves to {}",
                expected.name,
                expected.id,
                current_id
            );
        }
    }
    Ok(())
}

fn write_candidate_from_change(
    store: &GraftStore,
    change: ChangeSet,
    expected: Vec<graft_core::PropertyRef>,
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
    if let Ok(candidate) = store.read_candidate(id) {
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
        return Ok(envelope);
    }

    let patch = store.read_patch(id)?;
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
        expected: candidate.expected.iter().map(property_label).collect(),
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
        expected_properties: candidate.expected.iter().map(property_label).collect(),
    };
    graft_explain::next_actions::next_actions(&ctx)
}

fn materialize_target(as_commit: bool, ref_name: Option<&str>) -> String {
    match (as_commit, ref_name) {
        (true, Some(ref_name)) => format!("a Git commit at {ref_name}"),
        (true, None) => "a detached Git commit object".to_string(),
        (false, Some(ref_name)) => format!("Git ref {ref_name}"),
        (false, None) => "a Git-compatible patch object".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn load_graft_config_reads_split_property_config_and_creates_lock() {
        let dir = test_workspace("graft-cli-config-test");
        fs::create_dir_all(&dir).unwrap();
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let _ = fs::remove_file(dir.join("graft.lock"));

        let config = load_graft_config(&store).unwrap();

        assert!(config.properties.contains_key("ValidPatch"));
        assert!(dir.join("graft.lock").exists());
        assert!(!config.properties.contains_key("Missing"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn graft_toml_rejects_legacy_inline_properties() {
        let config = toml::from_str::<GraftConfig>(
            r#"
[properties.ValidPatch]
kind = "builtin"
check = "valid_patch"
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
