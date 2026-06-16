use super::*;
use crate::view::render_command_human;
use crate::workspace::{
    daemon_socket_run_dir, git_origin_url, git_origin_url_from_stdout, repo_id_for_url,
};
use clap::CommandFactory;
use graft_core::Constraint;
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

fn write_test_application(
    store: &GraftStore,
    base_state: StateId,
    base: Option<&TreeSnapshot>,
    target_snapshot: &TreeSnapshot,
) -> ApplicationRef {
    let target_state = StateId::GraftTree(target_snapshot.id().unwrap());
    let materialized =
        graft_core::materialize_application(base_state, base, target_state, target_snapshot)
            .unwrap();
    store.write_materialized_application(&materialized).unwrap()
}

fn write_test_application_from_trees(
    store: &GraftStore,
    base_state: StateId,
    base_snapshot: &TreeSnapshot,
    target_snapshot: &TreeSnapshot,
) -> ApplicationRef {
    write_test_application(store, base_state, Some(base_snapshot), target_snapshot)
}

fn corrupt_application_action_body(store: &GraftStore, application: &ApplicationRef) {
    let ApplicationRef::Stored(application_id) = application;
    let record = store.read_application(application_id.as_str()).unwrap();
    fs::write(
        store
            .paths()
            .object_actions()
            .join(format!("{}.json", record.action)),
        serde_json::to_vec(&graft_core::Action::Sequence { steps: Vec::new() }).unwrap(),
    )
    .unwrap();
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

fn test_constraint(name: &str) -> PlanId {
    PlanId::new(format!("plan:{name}"))
}

fn empty_admission() -> AdmissionSummary {
    AdmissionSummary {
        constraint: Constraint::Top,
    }
}

#[test]
fn validation_satisfaction_summary_reports_discarded_policy_result() {
    let constraint = "plan:policy".to_string();
    let satisfied =
        validation_satisfaction_summary(Ok(graft_policy::AdmissionDecision { accepted: true }));
    assert_eq!(satisfied, "constraint satisfied");

    let missing =
        validation_satisfaction_summary(Err(graft_policy::PolicyError::MissingEvidence {
            constraint: constraint.clone(),
            path: "primitive plan:policy".to_string(),
        }));
    assert!(
        missing.starts_with("constraint not satisfied: [A001]"),
        "{missing}"
    );
    assert!(missing.contains("primitive plan:policy"), "{missing}");

    let not_passed =
        validation_satisfaction_summary(Err(graft_policy::PolicyError::EvidenceNotPassed {
            constraint,
            evidence: "evidence:demo".to_string(),
            path: "primitive plan:policy".to_string(),
        }));
    assert!(
        not_passed.starts_with("constraint not satisfied: [A002]"),
        "{not_passed}"
    );
    assert!(not_passed.contains("evidence:demo"), "{not_passed}");
}

#[test]
fn require_passed_evidence_reports_admission_diagnostics() {
    let constraint = test_constraint("policy");
    let missing = require_passed_evidence(std::slice::from_ref(&constraint), &[], "candidate:demo")
        .unwrap_err()
        .to_string();
    assert!(missing.starts_with("[A001]"), "{missing}");
    assert!(missing.contains("plan:policy"), "{missing}");

    let failed = EvidenceRecord::failed(
        "candidate:demo",
        constraint.clone(),
        "test",
        "policy failed",
    )
    .unwrap();
    let failed_error = require_passed_evidence(&[constraint], &[failed], "candidate:demo")
        .unwrap_err()
        .to_string();
    assert!(failed_error.starts_with("[A002]"), "{failed_error}");
    assert!(failed_error.contains("policy"), "{failed_error}");
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
        "constraint",
        "cache",
        "verify-pending",
        "discard",
        "materialize",
        "promote",
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
        Cli::try_parse_from(["graft", "scratch", "open", "--base", "graft:empty"])
            .unwrap()
            .command,
        Command::Scratch {
            command: ScratchCommand::Open { .. },
            ..
        }
    ));
    match Cli::try_parse_from([
        "graft",
        "scratch",
        "write",
        "--base",
        "graft:empty",
        "note.txt",
        "--content",
        "-",
    ])
    .unwrap()
    .command
    {
        Command::Scratch {
            command:
                ScratchCommand::Write {
                    content,
                    content_stdin,
                    ..
                },
            ..
        } => {
            assert_eq!(content.as_deref(), Some("-"));
            assert!(!content_stdin);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    match Cli::try_parse_from([
        "graft",
        "scratch",
        "write",
        "--base",
        "graft:empty",
        "note.txt",
        "--content-stdin",
    ])
    .unwrap()
    .command
    {
        Command::Scratch {
            command:
                ScratchCommand::Write {
                    content,
                    content_stdin,
                    ..
                },
            ..
        } => {
            assert!(content.is_none());
            assert!(content_stdin);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    match Cli::try_parse_from([
        "graft",
        "scratch",
        "edit",
        "--from",
        "scratch:abc",
        "note.txt",
        "--edits-stdin",
    ])
    .unwrap()
    .command
    {
        Command::Scratch {
            command: ScratchCommand::Edit {
                edits, edits_stdin, ..
            },
            ..
        } => {
            assert!(edits.is_none());
            assert!(edits_stdin);
        }
        other => panic!("unexpected command: {other:?}"),
    }
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
        Cli::try_parse_from(["graft", "materialize", "patch:abc"])
            .unwrap()
            .command,
        Command::Materialize { .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["graft", "promote", "patch:abc", "--to", "main"])
            .unwrap()
            .command,
        Command::Promote { .. }
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
            "scratch open",
            &["graft", "scratch", "open", "--base", "graft:empty"],
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
            "scratch write stdin",
            &[
                "graft",
                "scratch",
                "write",
                "--base",
                "graft:empty",
                "a.txt",
                "--content-stdin",
            ],
            TopLevelRoute::Local,
            false,
        ),
        (
            "scratch edit stdin",
            &[
                "graft",
                "scratch",
                "edit",
                "--from",
                "scratch:abc",
                "a.txt",
                "--edits-stdin",
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
            &["graft", "patch", "search", "--constraint", "valid_patch"],
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
            &["graft", "candidates", "--constraint", "valid_patch"],
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
            &["graft", "search", "--constraint", "valid_patch"],
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
            "constraint lock",
            &["graft", "constraint", "lock"],
            TopLevelRoute::Local,
            false,
        ),
        (
            "constraint check",
            &["graft", "constraint", "check"],
            TopLevelRoute::Local,
            false,
        ),
        (
            "constraint list",
            &["graft", "constraint", "list"],
            TopLevelRoute::Local,
            false,
        ),
        (
            "constraint show",
            &["graft", "constraint", "show", "valid_patch"],
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
            &["graft", "cache", "search", "--constraint", "valid_patch"],
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

    let patch_target = TreeSnapshot::new(vec![TreeEntry {
        path: "patch.txt".to_string(),
        hash: store.write_blob(b"patch\n").unwrap(),
        size: 6,
    }]);
    store.write_tree_snapshot(&patch_target).unwrap();
    let patch_application = write_test_application(
        &store,
        StateId::GraftTree("tree:base".to_string()),
        None,
        &patch_target,
    );
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:admitted"),
        application: patch_application,
        constraint: Constraint::Top,
        provenance: Provenance::now("patch-producer", None),
        admission: empty_admission(),
    };
    store.write_patch(&patch).unwrap();
    let candidate_target = TreeSnapshot::new(vec![TreeEntry {
        path: "candidate.txt".to_string(),
        hash: store.write_blob(b"candidate\n").unwrap(),
        size: 10,
    }]);
    store.write_tree_snapshot(&candidate_target).unwrap();
    let candidate_application = write_test_application(
        &store,
        StateId::GraftTree("tree:base".to_string()),
        None,
        &candidate_target,
    );
    let candidate = GraftCandidate {
        id: graft_core::CandidateId::new("candidate:queued"),
        application: candidate_application,
        constraint: Constraint::Top,
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
                upgrade_from_v1: false,
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
fn constraint_lock_is_explicit_before_config_load_succeeds() {
    let dir = test_workspace("graft-cli-config-test");
    fs::create_dir_all(&dir).unwrap();
    let store = GraftStore::open(&dir);
    store.init().unwrap();
    let _ = fs::remove_file(dir.join("graft.lock"));

    let check_error = run_constraint_command(&store, &ConstraintCommand::Check)
        .unwrap_err()
        .to_string();
    assert!(
        check_error.contains("[E_CONSTRAINT_LOCK_MISSING]"),
        "{check_error}"
    );
    assert!(!dir.join("graft.lock").exists());

    run_constraint_command(&store, &ConstraintCommand::Lock).unwrap();
    let config = load_graft_config(&store).unwrap();

    assert!(config.constraints.is_empty());
    assert!(dir.join("graft.lock").exists());
    assert!(!config.constraints.contains_key("Missing"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bundle_import_help_documents_v1_upgrade_flag() {
    let mut command =
        <RegistryCommand as clap::Subcommand>::augment_subcommands(clap::Command::new("bundle"));
    let help = command
        .find_subcommand_mut("import")
        .unwrap()
        .render_long_help()
        .to_string();

    assert!(help.contains("--upgrade-from-v1"), "{help}");
    assert!(help.contains("legacy v1"), "{help}");
    assert!(help.contains("v2 constraints"), "{help}");
}

#[test]
fn explain_promote_uses_default_requirements_without_workspace_config() {
    let dir = test_workspace("graft-cli-explain-default-config-test");
    fs::create_dir_all(&dir).unwrap();

    let line = promote_requirement_explain_line(&dir);

    assert!(line.contains("Promotion require source: missing"), "{line}");
    assert!(line.contains("[E_PROMOTION_REQUIREMENT_MISSING]"), "{line}");

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
fn scratch_payload_flags_require_exactly_one_input_source() {
    for (args, expected) in [
        (
            vec![
                "graft",
                "scratch",
                "write",
                "--base",
                "graft:empty",
                "note.txt",
            ],
            "--content",
        ),
        (
            vec![
                "graft",
                "scratch",
                "write",
                "--base",
                "graft:empty",
                "note.txt",
                "--content",
                "literal",
                "--content-stdin",
            ],
            "cannot be used with",
        ),
        (
            vec![
                "graft",
                "scratch",
                "edit",
                "--from",
                "scratch:abc",
                "note.txt",
            ],
            "--edits",
        ),
        (
            vec![
                "graft",
                "scratch",
                "edit",
                "--from",
                "scratch:abc",
                "note.txt",
                "--edits",
                "[]",
                "--edits-stdin",
            ],
            "cannot be used with",
        ),
    ] {
        let error = Cli::try_parse_from(args).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn materialize_rejects_removed_compatibility_and_git_ref_flags() {
    for (args, flag) in [
        (
            vec!["graft", "materialize", "patch:abc", "--discard"],
            "--discard",
        ),
        (
            vec!["graft", "materialize", "patch:abc", "--as-commit"],
            "--as-commit",
        ),
        (
            vec!["graft", "materialize", "patch:abc", "--ref", "x"],
            "--ref",
        ),
        (
            vec!["graft", "patch", "materialize", "patch:abc", "--discard"],
            "--discard",
        ),
        (
            vec!["graft", "patch", "materialize", "patch:abc", "--as-commit"],
            "--as-commit",
        ),
        (
            vec!["graft", "patch", "materialize", "patch:abc", "--ref", "x"],
            "--ref",
        ),
    ] {
        let error = Cli::try_parse_from(args).unwrap_err().to_string();
        assert!(error.contains(flag), "{error}");
    }
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

    let constraint = PlanId::new("plan:registryexport");
    let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "registry.txt".to_string(),
        hash: source_store.write_blob(b"demo\n").unwrap(),
        size: 5,
    }]);
    source_store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application(
        &source_store,
        StateId::GitTree("base".to_string()),
        None,
        &target_snapshot,
    );
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:registryexport"),
        application,
        constraint: Constraint::primitive(constraint.clone()),
        provenance: Provenance {
            producer: "test".to_string(),
            message: None,
            created_at: "now".to_string(),
        },
        admission: AdmissionSummary {
            constraint: Constraint::primitive(constraint.clone()),
        },
    };
    source_store.write_patch(&patch).unwrap();
    let evidence = EvidenceRecord::passed(patch.id.as_str(), constraint.clone(), "test").unwrap();
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
                upgrade_from_v1: false,
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
                upgrade_from_v1: false,
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
                upgrade_from_v1: false,
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
fn registry_import_rejects_v1_patch_bundle_without_upgrade_flag() {
    let _lock = env_lock();
    let dir = test_workspace("graft-cli-registry-import-v1-reject-test");
    let home = test_workspace("graft-cli-registry-import-v1-reject-home");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set("GRAFT_HOME", &home);
    let store = GraftStore::open(&dir);
    run_init_command(&store, false).unwrap();
    let bundle = dir.join("legacy-registry.json");
    fs::write(
        &bundle,
        include_str!("../tests/fixtures/legacy-v1-registry-bundle.json")
            .replace("APPLICATION_ID", "application:demo"),
    )
    .unwrap();

    let error = run_local(&Cli {
        command: Command::Registry {
            command: RegistryCommand::Import {
                upgrade_from_v1: false,
                path: bundle.clone(),
            },
        },
        json: false,
        cwd: dir.clone(),
    })
    .unwrap_err()
    .to_string();

    assert!(error.contains("[E_UNSUPPORTED_STORE_SCHEMA]"), "{error}");
    assert!(error.contains("--upgrade-from-v1"), "{error}");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn registry_import_upgrades_v1_patch_constraints_to_constraint() {
    let _lock = env_lock();
    let dir = test_workspace("graft-cli-registry-import-v1-upgrade-test");
    let home = test_workspace("graft-cli-registry-import-v1-upgrade-home");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set("GRAFT_HOME", &home);
    let store = GraftStore::open(&dir);
    run_init_command(&store, false).unwrap();
    let target_snapshot = TreeSnapshot::new(Vec::new());
    store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application(
        &store,
        StateId::GraftTree("tree:legacy-base".to_string()),
        None,
        &target_snapshot,
    );
    let ApplicationRef::Stored(application_id) = application;
    let bundle = dir.join("legacy-registry.json");
    fs::write(
        &bundle,
        include_str!("../tests/fixtures/legacy-v1-registry-bundle.json")
            .replace("APPLICATION_ID", application_id.as_str()),
    )
    .unwrap();

    let import = run_local(&Cli {
        command: Command::Registry {
            command: RegistryCommand::Import {
                upgrade_from_v1: true,
                path: bundle.clone(),
            },
        },
        json: false,
        cwd: dir.clone(),
    })
    .unwrap();

    assert!(import.registry_changed);
    assert_eq!(import.patch_ids.len(), 1);
    assert_ne!(import.patch_ids[0], "patch:legacy");
    let patch = store.read_patch(&import.patch_ids[0]).unwrap();
    let constraint = PlanId::new("constraint:legacy");
    assert_eq!(patch.constraint, Constraint::primitive(constraint.clone()));
    assert_eq!(
        patch.admission.constraint,
        Constraint::primitive(constraint)
    );
    let candidates = store.list_candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_ne!(candidates[0].id.as_str(), "candidate:legacy");
    assert_eq!(
        candidates[0].constraint,
        Constraint::primitive(PlanId::new("constraint:legacy-candidate"))
    );
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
            r#"{"patches":[{"id":"patch:demo","application":{"kind":"stored","value":"application:demo"},"constraint":{"kind":"top"},"provenance":{"producer":"test","message":null,"created_at":"now"},"admission":{"constraint":{"kind":"top"}},"surprise":true}],"evidence":[],"relations":[],"promotions":[]}"#,
        )
        .unwrap();

    let error = run_local(&Cli {
        command: Command::Registry {
            command: RegistryCommand::Import {
                upgrade_from_v1: false,
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
            r#"{"patches":[],"evidence":[{"id":"evidence:demo","subject":"patch:demo","plan":"plan:demo","verifier":"test","result":"passed","created_at":"now","surprise":true}],"relations":[],"promotions":[]}"#,
        )
        .unwrap();

    let error = run_local(&Cli {
        command: Command::Registry {
            command: RegistryCommand::Import {
                upgrade_from_v1: false,
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
            constraint_primitives: Vec::new(),
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
            "constraint".to_string(),
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
fn gc_retains_live_application_dependencies_and_collects_unreachable_objects() {
    let _lock = env_lock();
    let dir = test_workspace("graft-cli-gc-application-reachability-test");
    let home = test_workspace("graft-cli-gc-application-reachability-home");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set("GRAFT_HOME", &home);
    let store = GraftStore::open(&dir);
    store.init().unwrap();

    let live_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "live.txt".to_string(),
        hash: store.write_blob(b"live\n").unwrap(),
        size: 5,
    }]);
    let orphan_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "orphan.txt".to_string(),
        hash: store.write_blob(b"orphan\n").unwrap(),
        size: 7,
    }]);
    store.write_tree_snapshot(&live_snapshot).unwrap();
    store.write_tree_snapshot(&orphan_snapshot).unwrap();
    let live_application = write_test_application(
        &store,
        StateId::GraftTree("tree:base".to_string()),
        None,
        &live_snapshot,
    );
    let orphan_application = write_test_application(
        &store,
        StateId::GraftTree("tree:orphan-base".to_string()),
        None,
        &orphan_snapshot,
    );
    let live_resolved = store.resolve_application(&live_application).unwrap();
    let orphan_resolved = store.resolve_application(&orphan_application).unwrap();
    let ApplicationRef::Stored(live_application_id) = &live_application;
    let ApplicationRef::Stored(orphan_application_id) = &orphan_application;
    let candidate = GraftCandidate {
        id: graft_core::CandidateId::new("candidate:live"),
        application: live_application.clone(),
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
    };
    store.write_candidate(&candidate).unwrap();

    let dry_run = render_command_human(&run_gc(&store, false, false).unwrap());
    assert!(dry_run.contains("  orphan_applications: 1"), "{dry_run}");
    assert!(dry_run.contains("  orphan_actions: 1"), "{dry_run}");
    assert!(dry_run.contains("  orphan_changes: 1"), "{dry_run}");

    run_gc(&store, true, false).unwrap();

    assert!(
        store
            .paths()
            .object_applications()
            .join(format!("{live_application_id}.json"))
            .exists()
    );
    assert!(
        store
            .paths()
            .object_actions()
            .join(format!("{}.json", live_resolved.record.action))
            .exists()
    );
    assert!(
        store
            .paths()
            .object_changes()
            .join(format!("{}.json", live_resolved.record.change))
            .exists()
    );
    assert!(
        !store
            .paths()
            .object_applications()
            .join(format!("{orphan_application_id}.json"))
            .exists()
    );
    assert!(
        !store
            .paths()
            .object_actions()
            .join(format!("{}.json", orphan_resolved.record.action))
            .exists()
    );
    assert!(
        !store
            .paths()
            .object_changes()
            .join(format!("{}.json", orphan_resolved.record.change))
            .exists()
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
    let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "corrupt-index.txt".to_string(),
        hash: store.write_blob(b"x\n").unwrap(),
        size: 2,
    }]);
    store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application(
        &store,
        StateId::GraftTree("tree:base".to_string()),
        None,
        &target_snapshot,
    );
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:corrupt-index"),
        application,
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
        admission: empty_admission(),
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
    let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "corrupt-incoming.txt".to_string(),
        hash: store.write_blob(b"x\n").unwrap(),
        size: 2,
    }]);
    store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application(
        &store,
        StateId::GraftTree("tree:base".to_string()),
        None,
        &target_snapshot,
    );
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:corrupt-incoming-index"),
        application,
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
        admission: empty_admission(),
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
        "constraint": [],
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
        "label": "graft patch validate candidate:demo",
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
    write_constraint_lock(&store, &std::collections::BTreeMap::new()).unwrap();
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
    write_constraint_lock(&store, &std::collections::BTreeMap::new()).unwrap();
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
    write_constraint_lock(&store, &std::collections::BTreeMap::new()).unwrap();
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
        command: Command::Constraint {
            command: ConstraintCommand::Check,
        },
        json: false,
        cwd: cwd.clone(),
    })
    .unwrap();
    assert!(
        check.message.unwrap().contains("constraint lock current"),
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
    let error = git_origin_url_from_stdout(Path::new("/tmp/repo"), b"https://bad/\xFF\n".to_vec())
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
fn cli_validate_then_admit_consumes_v2_constraint_evidence() {
    let _lock = env_lock();
    let cwd = test_workspace("graft-cli-v2-validate-admit-test");
    let home = test_workspace("graft-cli-v2-validate-admit-home");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set("GRAFT_HOME", &home);
    let store = GraftStore::open(&cwd);
    run_init_command(&store, false).unwrap();
    fs::write(
        store.paths().constraints_roto_config(),
        r#"
fn v2_cli_check(app: Application) -> Constraint {
    primitive(app.changed_paths(["added.txt"]), any_match, "added.txt is touched")
}

fn v2_extra_cli_check(app: Application) -> Constraint {
    primitive(app.changed_paths(["other.txt"]), any_match, "extra other.txt policy is touched")
}
"#,
    )
    .unwrap();
    let defs = load_constraint_defs(&store).unwrap();
    let constraint = constraint_primitives(&defs["v2_cli_check"].body)[0].clone();
    let extra_constraint = constraint_primitives(&defs["v2_extra_cli_check"].body)[0].clone();
    write_constraint_lock(&store, &defs).unwrap();

    let base_snapshot = TreeSnapshot::new(Vec::new());
    let target_blob = store.write_blob(b"new\n").unwrap();
    let other_blob = store.write_blob(b"other\n").unwrap();
    let target_snapshot = TreeSnapshot::new(vec![
        TreeEntry {
            path: "added.txt".to_string(),
            hash: target_blob.clone(),
            size: 4,
        },
        TreeEntry {
            path: "other.txt".to_string(),
            hash: other_blob,
            size: 6,
        },
    ]);
    let (base_tree_id, _) = store.write_tree_snapshot(&base_snapshot).unwrap();
    let (_target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application_from_trees(
        &store,
        StateId::GraftTree(base_tree_id.clone()),
        &base_snapshot,
        &target_snapshot,
    );
    let mut candidate = GraftCandidate {
        id: graft_core::CandidateId::new("candidate:pending"),
        application,
        constraint: Constraint::primitive(constraint.clone()),
        provenance: Provenance::now("test", None),
    };
    candidate.id = candidate_id(&candidate).unwrap();
    let candidate_id = candidate.id.to_string();
    store.write_candidate(&candidate).unwrap();

    let validate = run_local(&Cli {
        command: Command::Validate {
            id: candidate_id.clone(),
            constraint_primitives: Vec::new(),
        },
        json: false,
        cwd: cwd.clone(),
    })
    .unwrap();
    assert_eq!(validate.evidence.len(), 1);
    assert_eq!(validate.evidence[0].constraint, constraint.as_str());
    assert_eq!(validate.evidence[0].result, "passed");
    assert!(validate.evidence[0].verifier.starts_with("plan@"));

    let missing_extra = run_local(&Cli {
        command: Command::Admit {
            id: candidate_id.clone(),
            required: vec!["v2_extra_cli_check".to_string()],
        },
        json: false,
        cwd: cwd.clone(),
    })
    .unwrap_err()
    .to_string();
    assert!(missing_extra.starts_with("[A001]"), "{missing_extra}");
    assert!(missing_extra.contains("primitive plan:"), "{missing_extra}");

    let validate_extra = run_local(&Cli {
        command: Command::Validate {
            id: candidate_id.clone(),
            constraint_primitives: vec!["v2_extra_cli_check".to_string()],
        },
        json: false,
        cwd: cwd.clone(),
    })
    .unwrap();
    assert!(
        validate_extra.evidence.iter().any(|evidence| {
            evidence.constraint == extra_constraint.as_str() && evidence.result == "passed"
        }),
        "expected passing extra evidence: {:?}",
        validate_extra.evidence
    );

    let admit = run_local(&Cli {
        command: Command::Admit {
            id: candidate_id.clone(),
            required: vec!["v2_extra_cli_check".to_string()],
        },
        json: false,
        cwd: cwd.clone(),
    })
    .unwrap();
    let patch_id = admit.patch_id.unwrap();
    let patch = store.read_patch(&patch_id).unwrap();
    assert_eq!(
        constraint_primitives(&patch.constraint),
        vec![constraint.clone(), extra_constraint.clone()]
    );
    let promoted = store.registry_evidence_for_subject(&patch_id).unwrap();
    assert!(
        promoted.iter().any(|evidence| {
            evidence.plan == constraint && matches!(evidence.result, EvidenceResult::Passed)
        }),
        "expected promoted primary evidence: {promoted:?}"
    );
    assert!(
        promoted.iter().any(|evidence| {
            evidence.plan == extra_constraint && matches!(evidence.result, EvidenceResult::Passed)
        }),
        "expected promoted extra evidence: {promoted:?}"
    );
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
    write_constraint_lock(&store, &std::collections::BTreeMap::new()).unwrap();
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
    let (_target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application_from_trees(
        &store,
        StateId::GraftTree(base_tree_id.clone()),
        &base_snapshot,
        &target_snapshot,
    );
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:materialize-test"),
        application,
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
        admission: empty_admission(),
    };
    store.write_patch(&patch).unwrap();

    let envelope = run_local(&Cli {
        command: Command::Materialize {
            id: patch.id.to_string(),
            dry_run: false,
        },
        json: false,
        cwd: cwd.clone(),
    })
    .unwrap();

    let target_state = store
        .resolve_application(&patch.application)
        .unwrap()
        .record
        .target_state;
    let destination = materialize_worktree_path(&store, &target_state);
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
fn show_validate_and_materialize_surface_application_integrity_failures() {
    let _lock = env_lock();
    let cwd = test_workspace("graft-cli-application-integrity-failure-test");
    let home = test_workspace("graft-cli-application-integrity-failure-home");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&home).unwrap();
    let _guard = EnvGuard::set("GRAFT_HOME", &home);
    let store = GraftStore::open(&cwd);
    store.init().unwrap();
    write_constraint_lock(&store, &std::collections::BTreeMap::new()).unwrap();
    let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "bad.txt".to_string(),
        hash: store.write_blob(b"bad\n").unwrap(),
        size: 4,
    }]);
    store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application(
        &store,
        StateId::GraftTree("tree:base".to_string()),
        None,
        &target_snapshot,
    );
    let candidate = GraftCandidate {
        id: graft_core::CandidateId::new("candidate:integrity-failure"),
        application: application.clone(),
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
    };
    store.write_candidate(&candidate).unwrap();
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:integrity-failure"),
        application: application.clone(),
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
        admission: empty_admission(),
    };
    store.write_patch(&patch).unwrap();
    corrupt_application_action_body(&store, &application);

    let show_error = show_record(&store, candidate.id.as_str(), false, false)
        .unwrap_err()
        .to_string();
    assert!(show_error.contains("[E_CHANGE_INTEGRITY]"), "{show_error}");
    assert!(show_error.contains("action id"), "{show_error}");

    let validate_error = run_local(&Cli {
        command: Command::Validate {
            id: candidate.id.to_string(),
            constraint_primitives: Vec::new(),
        },
        json: false,
        cwd: cwd.clone(),
    })
    .unwrap_err()
    .to_string();
    assert!(
        validate_error.contains("[E_CHANGE_INTEGRITY]"),
        "{validate_error}"
    );
    assert!(validate_error.contains("action id"), "{validate_error}");

    let materialize_error = error_chain_text(
        run_local(&Cli {
            command: Command::Materialize {
                id: patch.id.to_string(),
                dry_run: true,
            },
            json: false,
            cwd: cwd.clone(),
        })
        .unwrap_err(),
    );
    assert!(
        materialize_error.contains("[E_CHANGE_INTEGRITY]"),
        "{materialize_error}"
    );
    assert!(
        materialize_error.contains("action id"),
        "{materialize_error}"
    );

    let _ = fs::remove_dir_all(&cwd);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ensure_materialized_commit_uses_git_safe_patch_ref() {
    let _lock = env_lock();
    let cwd = test_workspace("graft-cli-promote-cache-ref-workspace");
    let target = test_workspace("graft-cli-promote-cache-ref-target");
    let home = test_workspace("graft-cli-promote-cache-ref-home");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::create_dir_all(&home).unwrap();
    run_process(std::ffi::OsStr::new("git"), &["init"], &target, None).unwrap();
    run_process(
        std::ffi::OsStr::new("git"),
        &["config", "user.email", "graft@example.test"],
        &target,
        None,
    )
    .unwrap();
    run_process(
        std::ffi::OsStr::new("git"),
        &["config", "user.name", "Graft Test"],
        &target,
        None,
    )
    .unwrap();
    let _guard = EnvGuard::set("GRAFT_HOME", &home);
    let store = GraftStore::open(&cwd);
    run_init_command(&store, false).unwrap();
    write_constraint_lock(&store, &std::collections::BTreeMap::new()).unwrap();
    let config = load_graft_config(&store).unwrap();

    let base_snapshot = TreeSnapshot::new(Vec::new());
    let target_blob = store.write_blob(b"cached\n").unwrap();
    let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "cached.txt".to_string(),
        hash: target_blob.clone(),
        size: 7,
    }]);
    let (base_tree_id, _) = store.write_tree_snapshot(&base_snapshot).unwrap();
    let (_target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application_from_trees(
        &store,
        StateId::GraftTree(base_tree_id.clone()),
        &base_snapshot,
        &target_snapshot,
    );
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:promote-cache-test"),
        application,
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
        admission: empty_admission(),
    };
    store.write_patch(&patch).unwrap();

    let commit_id = ensure_materialized_commit(
        &GixBackend,
        &store,
        &config,
        &target,
        &patch,
        patch.id.as_str(),
    )
    .unwrap();

    let resolved = run_process(
        std::ffi::OsStr::new("git"),
        &["rev-parse", "refs/graft/patches/promote-cache-test"],
        &target,
        None,
    )
    .unwrap();
    assert_eq!(resolved.trim(), commit_id);
    fs::remove_dir_all(store.paths().object_blobs()).unwrap();
    let cached_commit_id = ensure_materialized_commit(
        &GixBackend,
        &store,
        &config,
        &target,
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
        .current_dir(&target)
        .output()
        .unwrap();
    assert!(
        !invalid_typed_ref.status.success(),
        "typed patch id must not leak into Git ref names"
    );

    let _ = fs::remove_dir_all(&cwd);
    let _ = fs::remove_dir_all(&target);
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
    write_constraint_lock(&store, &std::collections::BTreeMap::new()).unwrap();
    fs::write(cwd.join("unrelated-cwd-file.txt"), "not part of diff\n").unwrap();

    let blob = store.write_blob(b"target\n").unwrap();
    let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "added.txt".to_string(),
        hash: blob,
        size: 7,
    }]);
    let (_target_tree_id, _) = store.write_tree_snapshot(&target_snapshot).unwrap();
    let application = write_test_application(
        &store,
        StateId::GraftTree("tree:empty".to_string()),
        None,
        &target_snapshot,
    );
    let patch = PatchRecord {
        id: graft_core::PatchId::new("patch:diff-test"),
        application,
        constraint: Constraint::Top,
        provenance: Provenance::now("test", None),
        admission: empty_admission(),
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
fn graft_toml_rejects_legacy_inline_constraints() {
    let config = toml::from_str::<GraftConfig>(
        r#"
[constraints.EmptyChange]
kind = "builtin"
check = "changed_paths_any_match"
"#,
    );

    assert!(config.is_err());
}

#[test]
fn migration_blocks_when_modified_path_is_missing_on_new_base() {
    let change = Change {
        base_state: StateId::GraftTree("tree:old".to_string()),
        target_state: StateId::GraftTree("tree:target".to_string()),
        ops: vec![graft_core::ChangeOp::ReplaceFile {
            path: "src/lib.rs".to_string(),
            before: "old".to_string(),
            after: "new".to_string(),
            mode_before: graft_core::FileMode::Regular,
            mode_after: graft_core::FileMode::Regular,
        }],
        capture: false,
    };
    let new_base = TreeSnapshot::new(Vec::new());

    let outcome = migrate_change(
        &change,
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
