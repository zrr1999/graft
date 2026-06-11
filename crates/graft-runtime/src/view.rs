use std::fmt::Display;

use graft_core::EvidenceRecord;
use graft_explain::NextAction;
use serde::{Deserialize, Serialize};

/// Structured command result shared by local handlers, daemon cli_exec, JSON output,
/// and human rendering. Handlers should prefer typed fields (including `view` for
/// command-specific display models) over pre-rendered `message` strings. `message`
/// remains as a compatibility escape hatch for command families that have not yet
/// migrated to typed views.
///
/// Migration backlog after the initial `ps`/`gc` slice: workspace status/attach,
/// doctor, repo, property, bundle/registry, scratch/candidate, patch lifecycle,
/// sync/clone, diff/incoming/search, and promote/materialize command families.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandEnvelope {
    pub(crate) status: String,
    pub(crate) message: Option<String>,
    pub(crate) view: Option<CommandView>,
    pub(crate) candidate_id: Option<String>,
    pub(crate) patch_id: Option<String>,
    pub(crate) evidence_ids: Vec<String>,
    pub(crate) patch_ids: Vec<String>,
    pub(crate) candidates: Vec<CandidateSummary>,
    pub(crate) patches: Vec<PatchSummary>,
    pub(crate) evidence: Vec<EvidenceView>,
    pub(crate) change: Option<ChangeView>,
    pub(crate) promotions: Vec<PromotionView>,
    pub(crate) cache_changed: bool,
    pub(crate) registry_changed: bool,
    pub(crate) git_changed: bool,
    pub(crate) next_actions: Vec<NextAction>,
}

impl CommandEnvelope {
    pub(crate) fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            message: None,
            view: None,
            candidate_id: None,
            patch_id: None,
            evidence_ids: Vec::new(),
            patch_ids: Vec::new(),
            candidates: Vec::new(),
            patches: Vec::new(),
            evidence: Vec::new(),
            change: None,
            promotions: Vec::new(),
            cache_changed: false,
            registry_changed: false,
            git_changed: false,
            next_actions: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum CommandView {
    Ps(PsView),
    Gc(GcView),
    Run(RunView),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunView {
    pub(crate) state_ref: String,
    pub(crate) resolved_state: String,
    pub(crate) cwd: String,
    pub(crate) command: Vec<String>,
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PsView {
    pub(crate) daemon: DaemonView,
    pub(crate) registry: RegistryOverviewView,
    pub(crate) workspaces: Vec<WorkspaceSummaryView>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DaemonView {
    pub(crate) graft_home: String,
    pub(crate) socket: String,
    pub(crate) socket_state: String,
    pub(crate) socket_exists: bool,
    pub(crate) pid_file: String,
    pub(crate) pid: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryOverviewView {
    pub(crate) workspaces: usize,
    pub(crate) workspaces_hidden_missing: usize,
    pub(crate) routes: usize,
    pub(crate) routes_hidden_stale: usize,
    pub(crate) repo_paths: usize,
    pub(crate) repo_paths_hidden_missing: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceSummaryView {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) root: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GcView {
    pub(crate) dry_run: bool,
    pub(crate) workspace: GcWorkspaceView,
    pub(crate) registry: Option<GcRegistryView>,
    pub(crate) apply_hint: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum GcWorkspaceView {
    DerivedEvidenceBodies {
        evidence_bodies_before: usize,
        evidence_bodies_selected: usize,
    },
    RegistryOnly {
        workspace_objects: String,
    },
    Workspace {
        orphan_objects_before: usize,
        orphan_evidence_bodies: usize,
        orphan_candidate_evidence_indexes: usize,
        orphan_patch_evidence_indexes: usize,
        orphan_applications: usize,
        orphan_actions: usize,
        orphan_changes: usize,
        orphan_objects_selected: usize,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GcRegistryView {
    pub(crate) stale_registry_records_before: usize,
    pub(crate) missing_workspaces: usize,
    pub(crate) stale_routes: usize,
    pub(crate) missing_repo_paths: usize,
    pub(crate) stale_registry_records_selected: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateSummary {
    pub(crate) id: String,
    pub(crate) base_state: String,
    pub(crate) target_state: String,
    pub(crate) constraint: Vec<String>,
    pub(crate) producer: String,
    pub(crate) message: Option<String>,
    pub(crate) created_at: String,
    pub(crate) evidence: EvidenceCounts,
    pub(crate) change: Option<ChangeView>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PatchSummary {
    pub(crate) id: String,
    pub(crate) base_state: String,
    pub(crate) target_state: String,
    pub(crate) constraint: Vec<String>,
    pub(crate) producer: String,
    pub(crate) message: Option<String>,
    pub(crate) admitted_at: String,
    pub(crate) evidence: EvidenceCounts,
    pub(crate) change: Option<ChangeView>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromotionView {
    pub(crate) id: String,
    pub(crate) patch_id: String,
    pub(crate) target: String,
    pub(crate) dry_run: bool,
    pub(crate) status: String,
    pub(crate) promoted_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceView {
    pub(crate) id: String,
    pub(crate) subject: String,
    pub(crate) property: String,
    pub(crate) verifier: String,
    pub(crate) result: String,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceCounts {
    pub(crate) total: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) unknown: usize,
    pub(crate) skipped: usize,
}

impl EvidenceCounts {
    pub(crate) fn from_records(records: &[EvidenceRecord]) -> Self {
        let mut counts = Self {
            total: records.len(),
            ..Self::default()
        };
        for record in records {
            match &record.result {
                graft_core::EvidenceResult::Passed => counts.passed += 1,
                graft_core::EvidenceResult::Failed { .. } => counts.failed += 1,
                graft_core::EvidenceResult::Unknown { .. } => counts.unknown += 1,
                graft_core::EvidenceResult::Skipped { .. } => counts.skipped += 1,
            }
        }
        counts
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeView {
    pub(crate) id: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) files: usize,
    pub(crate) added: usize,
    pub(crate) modified: usize,
    pub(crate) deleted: usize,
    pub(crate) unchanged: usize,
    pub(crate) captured: usize,
    pub(crate) target_bytes: u64,
    pub(crate) sample_paths: Vec<String>,
}

pub(crate) fn push_human_section(lines: &mut Vec<String>, title: &str) {
    lines.push(title.to_string());
}

pub(crate) fn push_human_kv(lines: &mut Vec<String>, key: &str, value: impl Display) {
    lines.push(format!("  {key}: {value}"));
}

pub(crate) fn push_human_item(lines: &mut Vec<String>, value: impl Display) {
    lines.push(format!("  - {value}"));
}

pub(crate) fn print_human(envelope: &CommandEnvelope) {
    println!("{}", render_command_human(envelope));
}

pub(crate) fn render_command_human(envelope: &CommandEnvelope) -> String {
    let mut lines = vec![format!("status: {}", envelope.status)];
    if let Some(view) = &envelope.view {
        push_command_view(&mut lines, view);
    } else if let Some(message) = &envelope.message {
        lines.extend(message.lines().map(str::to_string));
    }
    if let Some(candidate_id) = &envelope.candidate_id {
        lines.push(format!("candidate: {candidate_id}"));
    }
    if let Some(patch_id) = &envelope.patch_id {
        lines.push(format!("patch: {patch_id}"));
    }
    for candidate in &envelope.candidates {
        push_candidate(&mut lines, candidate);
    }
    for patch in &envelope.patches {
        push_patch(&mut lines, patch);
    }
    for promotion in &envelope.promotions {
        push_promotion(&mut lines, promotion);
    }
    for patch_id in &envelope.patch_ids {
        lines.push(format!("patch: {patch_id}"));
    }
    if let Some(change) = &envelope.change {
        push_change(&mut lines, change);
    }
    for evidence in &envelope.evidence {
        lines.push(format!(
            "evidence: {} {} {} ({})",
            evidence.id, evidence.property, evidence.result, evidence.verifier
        ));
    }
    lines.push(format!("cache {}", changed_word(envelope.cache_changed)));
    lines.push(format!(
        "registry {}",
        changed_word(envelope.registry_changed)
    ));
    lines.push(format!("git {}", changed_word(envelope.git_changed)));
    push_hole_report(&mut lines, &envelope.next_actions);
    lines.join("\n")
}

fn push_command_view(lines: &mut Vec<String>, view: &CommandView) {
    match view {
        CommandView::Ps(view) => push_ps_view(lines, view),
        CommandView::Gc(view) => push_gc_view(lines, view),
        CommandView::Run(view) => push_run_view(lines, view),
    }
}

fn push_run_view(lines: &mut Vec<String>, view: &RunView) {
    push_human_section(lines, "run");
    push_human_kv(lines, "state_ref", &view.state_ref);
    push_human_kv(lines, "resolved_state", &view.resolved_state);
    push_human_kv(lines, "cwd", &view.cwd);
    push_human_kv(lines, "command", shell_words(&view.command));
    push_human_kv(lines, "exit_code", view.exit_code);
    if !view.stdout.is_empty() {
        push_human_section(lines, "stdout");
        lines.extend(view.stdout.lines().map(str::to_string));
        if view.stdout.ends_with('\n') {
            lines.push(String::new());
        }
    }
    if !view.stderr.is_empty() {
        push_human_section(lines, "stderr");
        lines.extend(view.stderr.lines().map(str::to_string));
        if view.stderr.ends_with('\n') {
            lines.push(String::new());
        }
    }
}

fn shell_words(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if arg
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':'))
            {
                arg.clone()
            } else {
                format!("{arg:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_ps_view(lines: &mut Vec<String>, view: &PsView) {
    push_human_section(lines, "daemon");
    push_human_kv(lines, "graft_home", &view.daemon.graft_home);
    push_human_kv(lines, "socket", &view.daemon.socket);
    push_human_kv(lines, "socket_state", &view.daemon.socket_state);
    push_human_kv(lines, "socket_exists", view.daemon.socket_exists);
    push_human_kv(lines, "pid_file", &view.daemon.pid_file);
    if let Some(pid) = &view.daemon.pid {
        push_human_kv(lines, "pid", pid);
    }

    push_human_section(lines, "registry");
    push_human_kv(lines, "workspaces", view.registry.workspaces);
    if view.registry.workspaces_hidden_missing > 0 {
        push_human_kv(
            lines,
            "workspaces_hidden_missing",
            view.registry.workspaces_hidden_missing,
        );
    }
    push_human_kv(lines, "routes", view.registry.routes);
    if view.registry.routes_hidden_stale > 0 {
        push_human_kv(
            lines,
            "routes_hidden_stale",
            view.registry.routes_hidden_stale,
        );
    }
    push_human_kv(lines, "repo_paths", view.registry.repo_paths);
    if view.registry.repo_paths_hidden_missing > 0 {
        push_human_kv(
            lines,
            "repo_paths_hidden_missing",
            view.registry.repo_paths_hidden_missing,
        );
    }

    if !view.workspaces.is_empty() {
        push_human_section(lines, "workspaces");
        for workspace in &view.workspaces {
            push_human_item(
                lines,
                format!("{} ({}) {}", workspace.id, workspace.kind, workspace.root),
            );
        }
    }
}

fn push_gc_view(lines: &mut Vec<String>, view: &GcView) {
    let apply = !view.dry_run;
    push_human_section(lines, if apply { "applied" } else { "plan" });
    push_human_kv(lines, "dry_run", view.dry_run);
    match &view.workspace {
        GcWorkspaceView::DerivedEvidenceBodies {
            evidence_bodies_before,
            evidence_bodies_selected,
        } => {
            push_human_kv(lines, "scope", "derived evidence bodies");
            push_human_kv(lines, "evidence_bodies_before", evidence_bodies_before);
            push_human_kv(
                lines,
                if apply {
                    "evidence_bodies_deleted"
                } else {
                    "evidence_bodies_to_delete"
                },
                evidence_bodies_selected,
            );
        }
        GcWorkspaceView::RegistryOnly { workspace_objects } => {
            push_human_kv(lines, "workspace_objects", workspace_objects);
        }
        GcWorkspaceView::Workspace {
            orphan_objects_before,
            orphan_evidence_bodies,
            orphan_candidate_evidence_indexes,
            orphan_patch_evidence_indexes,
            orphan_applications,
            orphan_actions,
            orphan_changes,
            orphan_objects_selected,
        } => {
            push_human_kv(lines, "orphan_objects_before", orphan_objects_before);
            push_human_kv(lines, "orphan_evidence_bodies", orphan_evidence_bodies);
            push_human_kv(
                lines,
                "orphan_candidate_evidence_indexes",
                orphan_candidate_evidence_indexes,
            );
            push_human_kv(
                lines,
                "orphan_patch_evidence_indexes",
                orphan_patch_evidence_indexes,
            );
            push_human_kv(lines, "orphan_applications", orphan_applications);
            push_human_kv(lines, "orphan_actions", orphan_actions);
            push_human_kv(lines, "orphan_changes", orphan_changes);
            push_human_kv(
                lines,
                if apply {
                    "orphan_objects_deleted"
                } else {
                    "orphan_objects_to_delete"
                },
                orphan_objects_selected,
            );
        }
    }
    if let Some(registry) = &view.registry {
        push_human_kv(
            lines,
            "stale_registry_records_before",
            registry.stale_registry_records_before,
        );
        push_human_kv(lines, "missing_workspaces", registry.missing_workspaces);
        push_human_kv(lines, "stale_routes", registry.stale_routes);
        push_human_kv(lines, "missing_repo_paths", registry.missing_repo_paths);
        push_human_kv(
            lines,
            if apply {
                "stale_registry_records_deleted"
            } else {
                "stale_registry_records_to_delete"
            },
            registry.stale_registry_records_selected,
        );
    }
    if let Some(hint) = &view.apply_hint {
        push_human_kv(lines, "apply", hint);
    }
}

fn push_hole_report(lines: &mut Vec<String>, actions: &[NextAction]) {
    if actions.is_empty() {
        return;
    }
    lines.push("next:".to_string());
    for action in actions {
        lines.push(format!("  {} {}", action.kind.label(), action.label));
        lines.push(format!("      {}", action.why));
    }
}

fn push_candidate(lines: &mut Vec<String>, candidate: &CandidateSummary) {
    lines.push(format!("candidate: {}", candidate.id));
    lines.push(format!("  base: {}", candidate.base_state));
    lines.push(format!("  target: {}", candidate.target_state));
    lines.push(format!(
        "  constraint: {}",
        joined_or_dash(&candidate.constraint)
    ));
    lines.push(format!(
        "  evidence: {} passed, {} failed, {} unknown, {} skipped",
        candidate.evidence.passed,
        candidate.evidence.failed,
        candidate.evidence.unknown,
        candidate.evidence.skipped
    ));
    if let Some(change) = &candidate.change {
        push_change(lines, change);
    }
}

fn push_patch(lines: &mut Vec<String>, patch: &PatchSummary) {
    lines.push(format!("patch: {}", patch.id));
    lines.push(format!("  base: {}", patch.base_state));
    lines.push(format!("  target: {}", patch.target_state));
    lines.push(format!(
        "  constraint: {}",
        joined_or_dash(&patch.constraint)
    ));
    lines.push(format!("  admitted_at: {}", patch.admitted_at));
    lines.push(format!(
        "  evidence: {} passed, {} failed, {} unknown, {} skipped",
        patch.evidence.passed,
        patch.evidence.failed,
        patch.evidence.unknown,
        patch.evidence.skipped
    ));
    if let Some(change) = &patch.change {
        push_change(lines, change);
    }
}

fn push_promotion(lines: &mut Vec<String>, promotion: &PromotionView) {
    lines.push(format!("promotion: {}", promotion.id));
    lines.push(format!("  patch: {}", promotion.patch_id));
    lines.push(format!("  target: {}", promotion.target));
    lines.push(format!("  status: {}", promotion.status));
    lines.push(format!("  dry_run: {}", promotion.dry_run));
}

fn push_change(lines: &mut Vec<String>, change: &ChangeView) {
    if let Some(id) = &change.id {
        lines.push(format!("change: {id}"));
    }
    if let Some(description) = &change.description {
        lines.push(format!("change: {description}"));
    }
    lines.push(format!(
        "  files: {} added, {} modified, {} deleted, {} captured, {} unchanged",
        change.added, change.modified, change.deleted, change.captured, change.unchanged
    ));
    if !change.sample_paths.is_empty() {
        lines.push(format!("  sample: {}", change.sample_paths.join(", ")));
    }
}

fn joined_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn changed_word(changed: bool) -> &'static str {
    if changed { "changed" } else { "unchanged" }
}
