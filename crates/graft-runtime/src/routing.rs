use anyhow::{Result, bail};

use crate::candidate::CandidateFromScratchArgs;
use crate::registry::RegistryCommand;
use crate::repo::RepoCommand;
use crate::scratch::ScratchCommand;
use crate::{Command, PatchCommand, WorkspaceCommand};

pub(crate) fn command_gc_dry_run_derived_only(command: &Command) -> Option<bool> {
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

pub(crate) fn command_is_gc_apply(command: &Command) -> bool {
    matches!(
        command,
        Command::Gc { apply: true, .. }
            | Command::Workspace {
                command: WorkspaceCommand::Gc { apply: true, .. },
            }
    )
}

pub(crate) fn command_uses_cli_exec(command: &Command) -> bool {
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
        | Command::Constraint { .. }
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

pub(crate) fn command_is_workspace_registry_write(command: &Command) -> bool {
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
pub(crate) enum TopLevelRoute {
    Explain,
    GcPrompt { derived_only: bool },
    GcApply,
    WorkspaceRegistryWrite,
    CliExec,
    Local,
}

pub(crate) fn route_top_level_command(command: &Command) -> TopLevelRoute {
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

pub(crate) struct DaemonCliExecRouter;

impl DaemonCliExecRouter {
    pub(crate) fn ensure_supported(command: &Command) -> Result<()> {
        if command_uses_cli_exec(command) {
            Ok(())
        } else {
            bail!(
                "[E_CLI_EXEC_UNSUPPORTED] cli_exec only accepts daemon-owned write commands; use the typed daemon op or local CLI path for this command"
            )
        }
    }
}

pub(crate) fn command_uses_cwd_directly(command: &Command) -> bool {
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

pub(crate) fn command_is_gc(command: &Command) -> bool {
    matches!(
        command,
        Command::Gc { .. }
            | Command::Workspace {
                command: WorkspaceCommand::Gc { .. },
            }
    )
}

pub(crate) fn command_skips_workspace_init_check(command: &Command) -> bool {
    command_uses_cwd_directly(command) || command_is_gc(command)
}

pub(crate) enum PatchCommandRoute<'a> {
    List {
        candidates: bool,
        all: bool,
        constraint: &'a Option<String>,
        producer: &'a Option<String>,
    },
    FromScratch(&'a CandidateFromScratchArgs),
    Show {
        id: &'a str,
        evidence: bool,
        change: bool,
    },
    Incoming,
    TopLevelAlias(Command),
}

pub(crate) fn route_patch_command(command: &PatchCommand) -> PatchCommandRoute<'_> {
    match command {
        PatchCommand::List {
            candidates,
            all,
            constraint,
            producer,
        } => PatchCommandRoute::List {
            candidates: *candidates,
            all: *all,
            constraint,
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
        PatchCommand::Validate {
            id,
            constraint_primitives,
        } => PatchCommandRoute::TopLevelAlias(Command::Validate {
            id: id.clone(),
            constraint_primitives: constraint_primitives.clone(),
        }),
        PatchCommand::Admit { id, required } => PatchCommandRoute::TopLevelAlias(Command::Admit {
            id: id.clone(),
            required: required.clone(),
        }),
        PatchCommand::Search {
            constraint,
            base,
            producer,
            has_evidence,
        } => PatchCommandRoute::TopLevelAlias(Command::Search {
            constraint: constraint.clone(),
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
            constraint_primitives,
            validate,
        } => PatchCommandRoute::TopLevelAlias(Command::Compose {
            first: first.clone(),
            second: second.clone(),
            constraint_primitives: constraint_primitives.clone(),
            validate: *validate,
        }),
        PatchCommand::Migrate {
            id,
            onto,
            constraint_primitives,
            validate,
        } => PatchCommandRoute::TopLevelAlias(Command::Migrate {
            id: id.clone(),
            onto: onto.clone(),
            constraint_primitives: constraint_primitives.clone(),
            validate: *validate,
        }),
        PatchCommand::Revert {
            id,
            constraint_primitives,
            validate,
        } => PatchCommandRoute::TopLevelAlias(Command::Revert {
            id: id.clone(),
            constraint_primitives: constraint_primitives.clone(),
            validate: *validate,
        }),
        PatchCommand::Materialize { id, dry_run } => {
            PatchCommandRoute::TopLevelAlias(Command::Materialize {
                id: id.clone(),
                dry_run: *dry_run,
            })
        }
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
