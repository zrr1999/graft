use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use graft_core::{
    ApplicationEndpoint, ApplicationPlan, ChangeRef, ChangeSet, CheckPlan, EvidenceRecord,
    EvidenceResult, FileChange, FileChangeKind, FileRefPlan, GraftCandidate, HistorySelector,
    OverlayPlan, PatchRecord, PathSetPlan, ProbePlan, ProbePolarity, ProbeResult, PropertyRef,
    PropertyScope, PropertySpec, RunPlan, RunSelectorPlan, ScopedPropertyRef, StateId, TreeEntry,
    TreePlan, TreeSnapshot,
};
use graft_store::GraftStore;
use graft_validate::ValidationSubject;

use crate::config::{GraftConfig, load_graft_config, resolve_property};
use crate::repo::materialized_snapshot_for_state;
use crate::requirements::validation_properties_with_base;

#[derive(Clone)]
struct ValidationTarget {
    subject: ValidationSubject,
    base_snapshot: Option<TreeSnapshot>,
    target_snapshot: Option<TreeSnapshot>,
    integrity: EvidenceResult,
}

pub(crate) fn validate_candidate(
    store: &GraftStore,
    candidate: &GraftCandidate,
    expected: &[String],
) -> Result<Vec<EvidenceRecord>> {
    let config = load_graft_config(store)?;
    let target =
        validation_target_for_change(store, &config, candidate.id.as_str(), &candidate.change)?;
    ensure_integrity_passed(&target.integrity)?;
    let properties = validation_properties_with_base(&config, expected, &candidate.expected)?;
    let mut records = Vec::new();
    let mut memo = BTreeMap::new();
    for property in properties {
        let mut property_records =
            validate_scoped_property(store, &config, &target, &property, &mut memo)?;
        for evidence in &property_records {
            store.write_evidence(evidence)?;
            store.append_candidate_evidence_index(candidate.id.as_str(), evidence.id.as_str())?;
        }
        records.append(&mut property_records);
    }
    Ok(records)
}

pub(crate) fn validate_patch(
    store: &GraftStore,
    patch: &PatchRecord,
    expected: &[String],
) -> Result<Vec<EvidenceRecord>> {
    let config = load_graft_config(store)?;
    let target = validation_target_for_change(store, &config, patch.id.as_str(), &patch.change)?;
    ensure_integrity_passed(&target.integrity)?;
    let patch_properties = patch
        .properties
        .iter()
        .cloned()
        .map(|property| ScopedPropertyRef::new(PropertyScope::Workspace, property))
        .collect::<Vec<_>>();
    let properties = validation_properties_with_base(&config, expected, &patch_properties)?;
    let mut records = Vec::new();
    let mut memo = BTreeMap::new();
    for property in properties {
        let mut property_records =
            validate_scoped_property(store, &config, &target, &property, &mut memo)?;
        for evidence in &property_records {
            store.write_evidence(evidence)?;
            store.append_patch_evidence_index(patch.id.as_str(), evidence.id.as_str())?;
        }
        records.append(&mut property_records);
    }
    Ok(records)
}

fn validate_scoped_property(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &ScopedPropertyRef,
    memo: &mut BTreeMap<String, Vec<EvidenceRecord>>,
) -> Result<Vec<EvidenceRecord>> {
    let memo_key = property.label();
    if let Some(records) = memo.get(&memo_key) {
        return Ok(records.clone());
    }
    let spec = property_spec_for_ref(config, &property.property)?;
    let mut blockers = Vec::new();
    for required in &spec.plan.requires {
        let required_ref = resolve_property(config, required.as_str())?;
        let required_ref = ScopedPropertyRef::new(property.scope.clone(), required_ref);
        let required_records =
            validate_scoped_property(store, config, target, &required_ref, memo)?;
        if !required_records
            .iter()
            .all(|record| record.result.satisfies_requirement())
        {
            blockers.push(format!(
                "`{}` ({}) => {}",
                required_ref.label(),
                required_ref.property.id,
                evidence_results_label(&required_records)
            ));
        }
    }

    let record = if blockers.is_empty() {
        verify_property_spec(store, config, target, property, spec)?
    } else {
        EvidenceRecord::skipped(
            property.evidence_subject(&target.subject.id),
            property.property.id.clone(),
            verifier_id_for_spec(spec)?,
            format!("required properties did not pass: {}", blockers.join("; ")),
        )?
    };
    let records = vec![record];
    memo.insert(memo_key, records.clone());
    Ok(records)
}

#[cfg(test)]
fn validate_property(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &ScopedPropertyRef,
    memo: &mut BTreeMap<String, Vec<EvidenceRecord>>,
) -> Result<Vec<EvidenceRecord>> {
    validate_scoped_property(store, config, target, property, memo)
}

fn property_spec_for_ref<'a>(
    config: &'a GraftConfig,
    property: &PropertyRef,
) -> Result<&'a PropertySpec> {
    let spec = config.properties.get(&property.name).with_context(|| {
        format!(
            "[E_UNKNOWN_PROPERTY] property {} ({}) is not configured in properties.roto",
            property.name, property.id
        )
    })?;
    let current_id = spec.property_id()?;
    if current_id != property.id {
        bail!(
            "[E_PROPERTY_DRIFT] property `{}` drifted: stored ref has {}, current property resolves to {}",
            property.name,
            property.id,
            current_id
        );
    }
    Ok(spec)
}

fn verify_property_spec(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &ScopedPropertyRef,
    spec: &PropertySpec,
) -> Result<EvidenceRecord> {
    let mut scoped_target = target.clone();
    scoped_target.subject.id = property.evidence_subject(&target.subject.id);
    let verifier = verifier_id_for_spec(spec)?;
    let result = evaluate_check_list_as_all(
        store,
        config,
        &scoped_target,
        &property.property,
        &spec.plan.checks,
    );
    Ok(EvidenceRecord::new(
        scoped_target.subject.id.clone(),
        property.property.id.clone(),
        verifier,
        result,
    )?)
}

fn verifier_id_for_spec(spec: &PropertySpec) -> Result<String> {
    Ok(format!(
        "v2-plan:{}@{}",
        spec.name.as_str(),
        spec.property_id()?
    ))
}

fn evaluate_check_list_as_all(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    checks: &[CheckPlan],
) -> EvidenceResult {
    for (index, check) in checks.iter().enumerate() {
        let result = evaluate_check(store, config, target, property, check);
        if !result.satisfies_requirement() {
            return contextualize_non_passing_result(
                result,
                format!("check[{index}] did not pass"),
            );
        }
    }
    EvidenceResult::Passed
}

fn evaluate_check(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    check: &CheckPlan,
) -> EvidenceResult {
    match check {
        CheckPlan::Expect { probe, polarity } => {
            evaluate_expect(store, config, target, property, probe, polarity)
        }
        CheckPlan::AllOf { checks } => evaluate_all_of(store, config, target, property, checks),
        CheckPlan::AnyOf { checks } => evaluate_any_of(store, config, target, property, checks),
        CheckPlan::Unavailable { reason } => EvidenceResult::Unknown {
            reason: reason.clone(),
        },
    }
}

fn evaluate_all_of(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    checks: &[CheckPlan],
) -> EvidenceResult {
    for (index, check) in checks.iter().enumerate() {
        let result = evaluate_check(store, config, target, property, check);
        if !result.satisfies_requirement() {
            return contextualize_non_passing_result(
                result,
                format!("all_of branch {index} did not pass"),
            );
        }
    }
    EvidenceResult::Passed
}

fn evaluate_any_of(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    checks: &[CheckPlan],
) -> EvidenceResult {
    let mut failures = Vec::new();
    let mut saw_unknown = false;
    for (index, check) in checks.iter().enumerate() {
        let result = evaluate_check(store, config, target, property, check);
        if result.satisfies_requirement() {
            return EvidenceResult::Passed;
        }
        saw_unknown |= matches!(
            result,
            EvidenceResult::Unknown { .. } | EvidenceResult::Skipped { .. }
        );
        failures.push(format!(
            "branch {index}: {}",
            evidence_result_label(&result)
        ));
    }
    let reason = if failures.is_empty() {
        "any_of had no branches".to_string()
    } else {
        format!("no any_of branch passed: {}", failures.join("; "))
    };
    if saw_unknown {
        EvidenceResult::Unknown { reason }
    } else {
        EvidenceResult::Failed { reason }
    }
}

fn evaluate_expect(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    probe: &ProbePlan,
    polarity: &ProbePolarity,
) -> EvidenceResult {
    match evaluate_probe(store, config, target, property, probe) {
        ProbeEvaluation::Result(ProbeResult::Success) => match polarity {
            ProbePolarity::Success => EvidenceResult::Passed,
            ProbePolarity::Failure => EvidenceResult::Failed {
                reason: "probe succeeded but failure was expected".to_string(),
            },
        },
        ProbeEvaluation::Result(ProbeResult::Failure) => match polarity {
            ProbePolarity::Success => EvidenceResult::Failed {
                reason: "probe failed but success was expected".to_string(),
            },
            ProbePolarity::Failure => EvidenceResult::Passed,
        },
        ProbeEvaluation::Result(ProbeResult::Error) => EvidenceResult::Unknown {
            reason: "probe evaluation returned error".to_string(),
        },
        ProbeEvaluation::Error(reason) => EvidenceResult::Unknown { reason },
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProbeEvaluation {
    Result(ProbeResult),
    Error(String),
}

fn evaluate_probe(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    probe: &ProbePlan,
) -> ProbeEvaluation {
    match probe {
        ProbePlan::PathMatch { paths, patterns } => {
            let paths = evaluate_path_set(&target.subject, paths);
            if paths
                .iter()
                .any(|path| patterns.iter().any(|pattern| path_matches(pattern, path)))
            {
                ProbeEvaluation::Result(ProbeResult::Success)
            } else {
                ProbeEvaluation::Result(ProbeResult::Failure)
            }
        }
        ProbePlan::PathAllMatch { paths, patterns } => {
            let paths = evaluate_path_set(&target.subject, paths);
            if paths
                .iter()
                .all(|path| patterns.iter().any(|pattern| path_matches(pattern, path)))
            {
                ProbeEvaluation::Result(ProbeResult::Success)
            } else {
                ProbeEvaluation::Result(ProbeResult::Failure)
            }
        }
        ProbePlan::RunExitCodeIs { run, code } => {
            match execute_run_plan(store, config, target, property, run) {
                Ok(run) if run.output.status.code() == Some(*code) => {
                    ProbeEvaluation::Result(ProbeResult::Success)
                }
                Ok(_run) => ProbeEvaluation::Result(ProbeResult::Failure),
                Err(reason) => ProbeEvaluation::Error(reason),
            }
        }
        ProbePlan::SameOutput {
            left,
            right,
            selectors,
        } => evaluate_same_output(store, config, target, property, left, right, selectors),
    }
}

fn evaluate_path_set<'a>(subject: &'a ValidationSubject, paths: &PathSetPlan) -> &'a [String] {
    match paths {
        PathSetPlan::ChangedPaths => &subject.changed_paths,
    }
}

#[derive(Debug)]
struct RunExecution {
    output: std::process::Output,
    cwd: PathBuf,
}

fn execute_run_plan(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    run: &RunPlan,
) -> std::result::Result<RunExecution, String> {
    let Some(program) = run.argv.first() else {
        return Err("run argv must not be empty".to_string());
    };
    let cwd = materialize_run_tree(store, config, target, property, &run.tree)?;
    let output = ProcessCommand::new(program)
        .args(&run.argv[1..])
        .current_dir(&cwd)
        .output()
        .map_err(|error| {
            format!(
                "failed to execute `{}` in {}: {error}",
                program,
                cwd.display()
            )
        })?;
    Ok(RunExecution { output, cwd })
}

fn evaluate_same_output(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    left: &RunPlan,
    right: &RunPlan,
    selectors: &[RunSelectorPlan],
) -> ProbeEvaluation {
    if selectors.is_empty() {
        return ProbeEvaluation::Error("same_output requires at least one selector".to_string());
    }
    let left = match execute_run_plan(store, config, target, property, left) {
        Ok(run) => run,
        Err(reason) => return ProbeEvaluation::Error(format!("left run failed: {reason}")),
    };
    let left_values = match capture_run_selectors(&left, selectors) {
        Ok(values) => values,
        Err(reason) => return ProbeEvaluation::Error(format!("left selector failed: {reason}")),
    };
    let right = match execute_run_plan(store, config, target, property, right) {
        Ok(run) => run,
        Err(reason) => return ProbeEvaluation::Error(format!("right run failed: {reason}")),
    };
    let right_values = match capture_run_selectors(&right, selectors) {
        Ok(values) => values,
        Err(reason) => return ProbeEvaluation::Error(format!("right selector failed: {reason}")),
    };
    if left_values == right_values {
        ProbeEvaluation::Result(ProbeResult::Success)
    } else {
        ProbeEvaluation::Result(ProbeResult::Failure)
    }
}

fn capture_run_selectors(
    run: &RunExecution,
    selectors: &[RunSelectorPlan],
) -> std::result::Result<Vec<Vec<u8>>, String> {
    selectors
        .iter()
        .map(|selector| capture_run_selector(run, selector))
        .collect()
}

fn capture_run_selector(
    run: &RunExecution,
    selector: &RunSelectorPlan,
) -> std::result::Result<Vec<u8>, String> {
    match selector {
        RunSelectorPlan::Stdout => Ok(run.output.stdout.clone()),
        RunSelectorPlan::Stderr => Ok(run.output.stderr.clone()),
        RunSelectorPlan::PostFile { path } => {
            let path = normalize_plan_path(path)?;
            let full_path = run.cwd.join(&path);
            fs::read(&full_path).map_err(|error| {
                format!("read post_file `{path}` in {}: {error}", run.cwd.display())
            })
        }
    }
}

fn materialize_run_tree(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    tree: &TreePlan,
) -> std::result::Result<PathBuf, String> {
    let snapshot = snapshot_for_tree_plan(store, config, target, property, tree)?;
    let tree_id = snapshot
        .id()
        .map_err(|error| format!("tree id could not be computed: {error}"))?;
    let destination = store.paths().derived_worktrees().join(&tree_id);
    store
        .materialize_tree_snapshot(&snapshot, &destination)
        .map_err(|error| format!("materialize {tree_id} for command run: {error}"))?;
    Ok(destination)
}

fn snapshot_for_tree_plan(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    tree: &TreePlan,
) -> std::result::Result<TreeSnapshot, String> {
    match tree {
        TreePlan::Application {
            application: ApplicationPlan::Current,
            endpoint,
        } => match endpoint {
            ApplicationEndpoint::Base => target
                .base_snapshot
                .clone()
                .ok_or_else(|| "current application base tree is unavailable".to_string()),
            ApplicationEndpoint::Target => target
                .target_snapshot
                .clone()
                .ok_or_else(|| "current application target tree is unavailable".to_string()),
        },
        TreePlan::Application {
            application: ApplicationPlan::PreviousFailure { selector },
            endpoint,
        } => previous_failure_snapshot(store, config, property, selector, endpoint),
        TreePlan::WithOverlay { base, overlays } => {
            let base = snapshot_for_tree_plan(store, config, target, property, base)?;
            apply_overlays(store, config, target, property, base, overlays)
        }
    }
}

#[derive(Clone, Debug)]
struct HistoricalFailure {
    subject: String,
    created_at: String,
    change: ChangeRef,
}

fn previous_failure_snapshot(
    store: &GraftStore,
    config: &GraftConfig,
    property: &PropertyRef,
    selector: &HistorySelector,
    endpoint: &ApplicationEndpoint,
) -> std::result::Result<TreeSnapshot, String> {
    let failure = select_previous_failure(store, property, selector)?;
    let ChangeRef::Stored(change_id) = &failure.change else {
        return Err(format!(
            "previous failed application `{}` has only an inline change summary",
            failure.subject
        ));
    };
    let change = store
        .read_change(change_id.as_str())
        .map_err(|error| format!("read previous failed change `{change_id}`: {error}"))?;
    let state = match endpoint {
        ApplicationEndpoint::Base => &change.base_state,
        ApplicationEndpoint::Target => &change.target_state,
    };
    materialized_snapshot_for_state(store, config, state).map_err(|error| {
        format!(
            "materialize {endpoint:?} tree for previous failed application `{}`: {error}",
            failure.subject
        )
    })
}

fn select_previous_failure(
    store: &GraftStore,
    property: &PropertyRef,
    selector: &HistorySelector,
) -> std::result::Result<HistoricalFailure, String> {
    let mut failures = BTreeMap::<String, HistoricalFailure>::new();
    for candidate in store
        .list_candidates()
        .map_err(|error| format!("list candidates for previous_failure: {error}"))?
    {
        let subject = candidate.id.as_str();
        let evidence = store
            .candidate_evidence_records(subject)
            .map_err(|error| format!("read candidate evidence for `{subject}`: {error}"))?;
        record_failed_application(
            &mut failures,
            subject,
            &candidate.change,
            property,
            evidence,
        );
    }
    for patch in store
        .list_patches()
        .map_err(|error| format!("list patches for previous_failure: {error}"))?
    {
        let subject = patch.id.as_str();
        let evidence = store
            .registry_evidence_for_subject(subject)
            .map_err(|error| format!("read patch evidence for `{subject}`: {error}"))?;
        record_failed_application(&mut failures, subject, &patch.change, property, evidence);
    }
    let mut failures = failures.into_values().collect::<Vec<_>>();
    failures.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.subject.cmp(&right.subject))
    });
    if failures.is_empty() {
        return Err(format!(
            "no previous failed application for property `{}`",
            property.id.as_str()
        ));
    }
    let index = match selector {
        HistorySelector::First => 0,
        HistorySelector::Last => failures.len() - 1,
        HistorySelector::Get { index } => usize::try_from(*index)
            .map_err(|_| format!("previous_failure index {index} is too large"))?,
    };
    failures.get(index).cloned().ok_or_else(|| {
        format!(
            "previous_failure selector {} was out of range for {} failed application(s)",
            history_selector_label(selector),
            failures.len()
        )
    })
}

fn record_failed_application(
    failures: &mut BTreeMap<String, HistoricalFailure>,
    subject: &str,
    change: &ChangeRef,
    property: &PropertyRef,
    evidence: Vec<EvidenceRecord>,
) {
    for record in evidence {
        if record.property == property.id && matches!(record.result, EvidenceResult::Failed { .. })
        {
            let failure = HistoricalFailure {
                subject: subject.to_string(),
                created_at: record.created_at,
                change: change.clone(),
            };
            failures
                .entry(subject.to_string())
                .and_modify(|current| {
                    if (failure.created_at.as_str(), failure.subject.as_str())
                        < (current.created_at.as_str(), current.subject.as_str())
                    {
                        *current = failure.clone();
                    }
                })
                .or_insert(failure);
        }
    }
}

fn history_selector_label(selector: &HistorySelector) -> String {
    match selector {
        HistorySelector::First => "History.First".to_string(),
        HistorySelector::Last => "History.Last".to_string(),
        HistorySelector::Get { index } => format!("History.Get({index})"),
    }
}

fn apply_overlays(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    base: TreeSnapshot,
    overlays: &[OverlayPlan],
) -> std::result::Result<TreeSnapshot, String> {
    let mut entries = base
        .entries
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    for overlay in overlays {
        match overlay {
            OverlayPlan::ReplaceFile { path, file } => {
                let path = normalize_plan_path(path)?;
                let file = resolve_file_ref(store, config, target, property, file)?;
                entries.insert(
                    path.clone(),
                    TreeEntry {
                        path,
                        hash: file.hash,
                        size: file.size,
                    },
                );
            }
        }
    }
    Ok(TreeSnapshot::new(entries.into_values().collect()))
}

fn resolve_file_ref(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    file: &FileRefPlan,
) -> std::result::Result<TreeEntry, String> {
    match file {
        FileRefPlan::TreeFile { tree, path } => {
            let path = normalize_plan_path(path)?;
            let snapshot = snapshot_for_tree_plan(store, config, target, property, tree)?;
            snapshot
                .entries
                .into_iter()
                .find(|entry| entry.path == path)
                .ok_or_else(|| format!("file `{path}` was not found in referenced tree"))
        }
        FileRefPlan::Resolved { file } => Err(format!(
            "resolved file ref `{}` cannot be consumed until file-ref object storage lands",
            file.as_str()
        )),
    }
}

fn normalize_plan_path(path: &str) -> std::result::Result<String, String> {
    let mut value = path;
    while let Some(stripped) = value.strip_prefix("./") {
        value = stripped;
    }
    if value.is_empty() {
        return Err("plan file path must not be empty".to_string());
    }
    if value.starts_with('/') {
        return Err(format!("plan file path `{path}` must be relative"));
    }
    if value.contains('\\') || value.contains('\n') || value.contains('\t') {
        return Err(format!(
            "plan file path `{path}` contains an invalid character"
        ));
    }
    if value.contains("//") {
        return Err(format!(
            "plan file path `{path}` must not contain empty components"
        ));
    }
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "plan file path `{path}` contains invalid component `{component}`"
            ));
        }
    }
    Ok(value.to_string())
}

fn contextualize_non_passing_result(result: EvidenceResult, context: String) -> EvidenceResult {
    match result {
        EvidenceResult::Passed => EvidenceResult::Passed,
        EvidenceResult::Failed { reason } => EvidenceResult::Failed {
            reason: format!("{context}: {reason}"),
        },
        EvidenceResult::Unknown { reason } => EvidenceResult::Unknown {
            reason: format!("{context}: {reason}"),
        },
        EvidenceResult::Skipped { reason } => EvidenceResult::Skipped {
            reason: format!("{context}: {reason}"),
        },
    }
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == path;
    }
    let mut rest = path;
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let parts = pattern
        .split('*')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return true;
    }
    if !starts_with_wildcard
        && let Some(first) = parts.first()
        && !rest.starts_with(first)
    {
        return false;
    }
    for part in &parts {
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    if !ends_with_wildcard
        && let Some(last) = parts.last()
        && !path.ends_with(last)
    {
        return false;
    }
    true
}

fn evidence_results_label(records: &[EvidenceRecord]) -> String {
    if records.is_empty() {
        return "no evidence".to_string();
    }
    records
        .iter()
        .map(|record| evidence_result_label(&record.result))
        .collect::<Vec<_>>()
        .join(", ")
}

fn evidence_result_label(result: &EvidenceResult) -> String {
    match result {
        EvidenceResult::Passed => "passed".to_string(),
        EvidenceResult::Failed { reason } => format!("failed ({reason})"),
        EvidenceResult::Unknown { reason } => format!("unknown ({reason})"),
        EvidenceResult::Skipped { reason } => format!("skipped ({reason})"),
    }
}

pub(crate) fn evidence_for_current_verifiers(
    config: &GraftConfig,
    required: &[ScopedPropertyRef],
    evidence: &[EvidenceRecord],
    subject: &str,
) -> Result<Vec<EvidenceRecord>> {
    let mut current_verifiers = BTreeMap::new();
    for property in required {
        let spec = property_spec_for_ref(config, &property.property)?;
        current_verifiers.insert(
            (property.scope.clone(), property.property.id.clone()),
            verifier_id_for_spec(spec)?,
        );
    }
    Ok(evidence
        .iter()
        .filter(|record| {
            required.iter().any(|property| {
                record.property == property.property.id
                    && record.subject == property.evidence_subject(subject)
                    && current_verifiers
                        .get(&(property.scope.clone(), property.property.id.clone()))
                        .is_some_and(|verifier| verifier == &record.verifier)
            })
        })
        .cloned()
        .collect())
}

fn validation_target_for_change(
    store: &GraftStore,
    config: &GraftConfig,
    id: &str,
    change: &ChangeRef,
) -> Result<ValidationTarget> {
    match change {
        ChangeRef::Stored(change_id) => {
            let change = store.read_change(change_id.as_str())?;
            let changed_paths = change
                .files
                .iter()
                .filter(|file| !matches!(file.kind, FileChangeKind::Unchanged))
                .map(|file| file.path.clone())
                .collect();
            let base_snapshot = materialized_snapshot_for_state(store, config, &change.base_state)?;
            let target_snapshot =
                materialized_snapshot_for_state(store, config, &change.target_state)?;
            let integrity =
                change_integrity_for_snapshots(&change, &base_snapshot, &target_snapshot);
            Ok(ValidationTarget {
                subject: ValidationSubject::with_change(id.to_string(), changed_paths),
                base_snapshot: Some(base_snapshot),
                target_snapshot: Some(target_snapshot),
                integrity,
            })
        }
        ChangeRef::InlineSummary(summary) => Ok(ValidationTarget {
            subject: ValidationSubject::new(id.to_string()),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Unknown {
                reason: graft_explain::diagnostics::c002_inline_change_not_transformable(summary)
                    .format_reason(),
            },
        }),
    }
}

pub(crate) fn ensure_change_integrity(
    store: &GraftStore,
    config: &GraftConfig,
    change: &ChangeRef,
) -> Result<()> {
    let result = change_integrity_for_ref(store, config, change)?;
    ensure_integrity_passed(&result)
}

fn change_integrity_for_ref(
    store: &GraftStore,
    config: &GraftConfig,
    change: &ChangeRef,
) -> Result<EvidenceResult> {
    let ChangeRef::Stored(change_id) = change else {
        return Ok(EvidenceResult::Unknown {
            reason: "patch has only an inline change summary; no stored ChangeSet is available"
                .to_string(),
        });
    };
    let change = store.read_change(change_id.as_str())?;
    let target_snapshot = match materialized_snapshot_for_state(store, config, &change.target_state)
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Ok(EvidenceResult::Unknown {
                reason: graft_explain::diagnostics::m003_target_not_materializable(
                    &format!("{:?}", change.target_state),
                    &error.to_string(),
                )
                .format_reason(),
            });
        }
    };
    Ok(change_integrity_for_change(
        store,
        config,
        &change,
        &target_snapshot,
    ))
}

fn ensure_integrity_passed(result: &EvidenceResult) -> Result<()> {
    match result {
        EvidenceResult::Passed => Ok(()),
        EvidenceResult::Failed { reason }
        | EvidenceResult::Unknown { reason }
        | EvidenceResult::Skipped { reason } => {
            bail!("[E_CHANGE_INTEGRITY] patch core invariant failed: {reason}")
        }
    }
}

fn change_integrity_for_change(
    store: &GraftStore,
    config: &GraftConfig,
    change: &ChangeSet,
    target_snapshot: &TreeSnapshot,
) -> EvidenceResult {
    let base_snapshot = match materialized_snapshot_for_state(store, config, &change.base_state) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return EvidenceResult::Unknown {
                reason: graft_explain::diagnostics::v003_base_unmaterializable(&error.to_string())
                    .format_reason(),
            };
        }
    };
    change_integrity_for_snapshots(change, &base_snapshot, target_snapshot)
}

fn change_integrity_for_snapshots(
    change: &ChangeSet,
    base_snapshot: &TreeSnapshot,
    target_snapshot: &TreeSnapshot,
) -> EvidenceResult {
    match validate_change_replays_to_target(change, base_snapshot, target_snapshot) {
        Ok(()) => EvidenceResult::Passed,
        Err(reason) => EvidenceResult::Failed { reason },
    }
}

fn validate_change_replays_to_target(
    change: &ChangeSet,
    base_snapshot: &TreeSnapshot,
    target_snapshot: &TreeSnapshot,
) -> std::result::Result<(), String> {
    ensure_snapshot_matches_state("base", &change.base_state, base_snapshot)?;
    ensure_snapshot_matches_state("target", &change.target_state, target_snapshot)?;

    let mut applied = snapshot_entries(base_snapshot);
    let mut seen = BTreeSet::new();
    for file in &change.files {
        if !seen.insert(file.path.clone()) {
            return Err(format!("duplicate change entry for path {}", file.path));
        }
        match file.kind {
            FileChangeKind::Added | FileChangeKind::Captured => {
                if applied.contains_key(&file.path) {
                    return Err(format!("path {} already exists in base", file.path));
                }
                let entry = target_entry_from_change(file)?;
                applied.insert(file.path.clone(), entry);
            }
            FileChangeKind::Modified => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("path {} is missing from base", file.path));
                };
                ensure_change_matches_base(file, base_entry)?;
                let entry = target_entry_from_change(file)?;
                applied.insert(file.path.clone(), entry);
            }
            FileChangeKind::Deleted => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("path {} is missing from base", file.path));
                };
                ensure_change_matches_base(file, base_entry)?;
                if file.target_hash.is_some() || file.target_size.is_some() {
                    return Err(format!("deleted path {} has target content", file.path));
                }
                applied.remove(&file.path);
            }
            FileChangeKind::Unchanged => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("unchanged path {} is missing from base", file.path));
                };
                ensure_change_matches_base(file, base_entry)?;
                let entry = target_entry_from_change(file)?;
                if entry != *base_entry {
                    return Err(format!(
                        "unchanged path {} changes content in target",
                        file.path
                    ));
                }
            }
        }
    }

    let expected = snapshot_entries(target_snapshot);
    if applied == expected {
        Ok(())
    } else {
        Err(snapshot_mismatch_reason(&applied, &expected))
    }
}

fn ensure_snapshot_matches_state(
    label: &str,
    state: &StateId,
    snapshot: &TreeSnapshot,
) -> std::result::Result<(), String> {
    if let StateId::GraftTree(expected) = state {
        let actual = snapshot
            .id()
            .map_err(|error| format!("{label} snapshot id could not be computed: {error}"))?;
        if actual != *expected {
            return Err(format!(
                "{label} snapshot id mismatch: state declares {expected}, snapshot is {actual}"
            ));
        }
    }
    Ok(())
}

fn snapshot_entries(snapshot: &TreeSnapshot) -> BTreeMap<String, TreeEntry> {
    snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}

fn target_entry_from_change(file: &FileChange) -> std::result::Result<TreeEntry, String> {
    let Some(hash) = &file.target_hash else {
        return Err(format!("path {} has no target hash", file.path));
    };
    let Some(size) = file.target_size else {
        return Err(format!("path {} has no target size", file.path));
    };
    Ok(TreeEntry {
        path: file.path.clone(),
        hash: hash.clone(),
        size,
    })
}

fn ensure_change_matches_base(
    file: &FileChange,
    base_entry: &TreeEntry,
) -> std::result::Result<(), String> {
    if file.base_hash.as_deref() != Some(base_entry.hash.as_str())
        || file.base_size != Some(base_entry.size)
    {
        return Err(format!(
            "path {} does not match declared base content",
            file.path
        ));
    }
    Ok(())
}

fn snapshot_mismatch_reason(
    applied: &BTreeMap<String, TreeEntry>,
    expected: &BTreeMap<String, TreeEntry>,
) -> String {
    let paths = applied
        .keys()
        .chain(expected.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        match (applied.get(&path), expected.get(&path)) {
            (None, Some(_)) => return format!("target contains path {path} absent after replay"),
            (Some(_), None) => return format!("replay produced path {path} absent from target"),
            (Some(left), Some(right)) if left != right => {
                return format!("replay produced different content for path {path}");
            }
            _ => {}
        }
    }
    "replayed patch did not reconstruct target".to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use graft_core::{
        ApplicationEndpoint, ApplicationPlan, ChangeRef, ChangeSet, CheckPlan, EvidenceRecord,
        EvidenceResult, HistorySelector, PatchId, PatchRecord, PathSetPlan, ProbePlan,
        ProbePolarity, PropertyName, PropertyPlan, PropertyRef, PropertyScope, PropertySpec,
        Provenance, RunPlan, ScopedPropertyRef, Severity, StateId, TreeEntry, TreePlan,
        TreeSnapshot,
    };
    use graft_store::GraftStore;
    use graft_validate::ValidationSubject;

    use super::{
        ValidationTarget, evidence_for_current_verifiers, validate_change_replays_to_target,
        validate_property, verifier_id_for_spec,
    };

    fn property_spec_with_checks(
        name: &str,
        checks: Vec<CheckPlan>,
        requires: &[&str],
    ) -> PropertySpec {
        PropertySpec {
            name: PropertyName::new(name),
            plan: PropertyPlan {
                checks,
                requires: requires
                    .iter()
                    .map(|name| PropertyName::new(*name))
                    .collect(),
            },
            description: format!("{name} property"),
            severity: Severity::Blocking,
            source_ref: None,
        }
    }

    fn workspace_property(property: PropertyRef) -> ScopedPropertyRef {
        ScopedPropertyRef::new(PropertyScope::Workspace, property)
    }

    fn write_historical_failed_patch(
        store: &GraftStore,
        patch_id: &str,
        property: &PropertyRef,
        path: &str,
        content: &[u8],
    ) {
        let base_snapshot = TreeSnapshot::new(Vec::new());
        let target_hash = store.write_blob(content).unwrap();
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: path.to_string(),
            hash: target_hash,
            size: u64::try_from(content.len()).unwrap(),
        }]);
        store.write_tree_snapshot(&base_snapshot).unwrap();
        store.write_tree_snapshot(&target_snapshot).unwrap();
        let change = ChangeSet::from_snapshots(
            StateId::GraftTree(base_snapshot.id().unwrap()),
            Some(&base_snapshot),
            StateId::GraftTree(target_snapshot.id().unwrap()),
            &target_snapshot,
        );
        let (change_id, _) = store.write_change(&change).unwrap();
        let patch = PatchRecord {
            id: PatchId::new(patch_id),
            base_state: change.base_state.clone(),
            target_state: change.target_state.clone(),
            change: ChangeRef::Stored(change_id),
            properties: vec![property.clone()],
            provenance: Provenance::now("test", None),
            admitted_at: "test-time".to_string(),
        };
        store.write_patch(&patch).unwrap();
        let evidence = EvidenceRecord::failed(
            patch.id.as_str(),
            property.id.clone(),
            "test-verifier",
            "historical failure",
        )
        .unwrap();
        let evidence_id = evidence.id.to_string();
        store.write_evidence(&evidence).unwrap();
        store
            .append_patch_evidence_index(patch.id.as_str(), &evidence_id)
            .unwrap();
    }

    #[test]
    fn change_integrity_replays_change_to_target() {
        let base = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "old".to_string(),
            size: 3,
        }]);
        let target = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "new".to_string(),
            size: 3,
        }]);
        let change = ChangeSet::from_snapshots(
            StateId::GraftTree(base.id().unwrap()),
            Some(&base),
            StateId::GraftTree(target.id().unwrap()),
            &target,
        );

        assert!(validate_change_replays_to_target(&change, &base, &target).is_ok());
    }

    #[test]
    fn change_integrity_fails_when_base_content_does_not_match() {
        let base = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "old".to_string(),
            size: 3,
        }]);
        let target = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "new".to_string(),
            size: 3,
        }]);
        let mut change = ChangeSet::from_snapshots(
            StateId::GraftTree(base.id().unwrap()),
            Some(&base),
            StateId::GraftTree(target.id().unwrap()),
            &target,
        );
        change.files[0].base_hash = Some("other".to_string());

        let reason = validate_change_replays_to_target(&change, &base, &target).unwrap_err();

        assert!(reason.contains("does not match declared base content"));
    }

    #[test]
    fn v2_structural_path_match_success_and_failure_polarity() {
        let dir = test_workspace("graft-cli-v2-structural-path-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "source_changed".to_string(),
            property_spec_with_checks(
                "source_changed",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::PathMatch {
                        paths: PathSetPlan::ChangedPaths,
                        patterns: vec!["src/**".to_string()],
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        config.properties.insert(
            "no_generated".to_string(),
            property_spec_with_checks(
                "no_generated",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::PathMatch {
                        paths: PathSetPlan::ChangedPaths,
                        patterns: vec!["target/**".to_string()],
                    },
                    polarity: ProbePolarity::Failure,
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::with_change(
                "candidate:demo",
                vec!["src/lib.rs".to_string()],
            ),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };
        let source_changed =
            workspace_property(config.properties["source_changed"].property_ref().unwrap());
        let no_generated =
            workspace_property(config.properties["no_generated"].property_ref().unwrap());

        let source_records = validate_property(
            &store,
            &config,
            &target,
            &source_changed,
            &mut BTreeMap::new(),
        )
        .unwrap();
        let generated_records = validate_property(
            &store,
            &config,
            &target,
            &no_generated,
            &mut BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(source_records[0].result, EvidenceResult::Passed);
        assert_eq!(generated_records[0].result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_structural_path_all_match_checks_every_changed_path() {
        let dir = test_workspace("graft-cli-v2-structural-all-path-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "docs_only".to_string(),
            property_spec_with_checks(
                "docs_only",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::PathAllMatch {
                        paths: PathSetPlan::ChangedPaths,
                        patterns: vec!["docs/**".to_string(), "README.md".to_string()],
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        let docs_only = workspace_property(config.properties["docs_only"].property_ref().unwrap());
        let passing_target = ValidationTarget {
            subject: ValidationSubject::with_change(
                "candidate:docs",
                vec!["docs/guide.md".to_string(), "README.md".to_string()],
            ),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };
        let failing_target = ValidationTarget {
            subject: ValidationSubject::with_change(
                "candidate:mixed",
                vec!["docs/guide.md".to_string(), "src/lib.rs".to_string()],
            ),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };

        let passing = validate_property(
            &store,
            &config,
            &passing_target,
            &docs_only,
            &mut BTreeMap::new(),
        )
        .unwrap();
        let failing = validate_property(
            &store,
            &config,
            &failing_target,
            &docs_only,
            &mut BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(passing[0].result, EvidenceResult::Passed);
        assert!(matches!(failing[0].result, EvidenceResult::Failed { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_combinators_are_lazy() {
        let dir = test_workspace("graft-cli-v2-lazy-combinators-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "any_short_circuit".to_string(),
            property_spec_with_checks(
                "any_short_circuit",
                vec![CheckPlan::AnyOf {
                    checks: vec![
                        CheckPlan::Expect {
                            probe: ProbePlan::PathMatch {
                                paths: PathSetPlan::ChangedPaths,
                                patterns: vec!["src/**".to_string()],
                            },
                            polarity: ProbePolarity::Success,
                        },
                        CheckPlan::Unavailable {
                            reason: "any_of should not evaluate this branch".to_string(),
                        },
                    ],
                }],
                &[],
            ),
        );
        config.properties.insert(
            "all_short_circuit".to_string(),
            property_spec_with_checks(
                "all_short_circuit",
                vec![CheckPlan::AllOf {
                    checks: vec![
                        CheckPlan::Expect {
                            probe: ProbePlan::PathMatch {
                                paths: PathSetPlan::ChangedPaths,
                                patterns: vec!["target/**".to_string()],
                            },
                            polarity: ProbePolarity::Success,
                        },
                        CheckPlan::Unavailable {
                            reason: "all_of should not evaluate this branch".to_string(),
                        },
                    ],
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::with_change(
                "candidate:demo",
                vec!["src/lib.rs".to_string()],
            ),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };

        let any_ref = workspace_property(
            config.properties["any_short_circuit"]
                .property_ref()
                .unwrap(),
        );
        let all_ref = workspace_property(
            config.properties["all_short_circuit"]
                .property_ref()
                .unwrap(),
        );
        let any_records =
            validate_property(&store, &config, &target, &any_ref, &mut BTreeMap::new()).unwrap();
        let all_records =
            validate_property(&store, &config, &target, &all_ref, &mut BTreeMap::new()).unwrap();

        assert_eq!(any_records[0].result, EvidenceResult::Passed);
        assert!(matches!(
            all_records[0].result,
            EvidenceResult::Failed { ref reason }
                if !reason.contains("all_of should not evaluate this branch")
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_run_exit_code_executes_in_materialized_target_tree() {
        let dir = test_workspace("graft-cli-v2-run-probe-exec-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let target_hash = store.write_blob(b"ok\n").unwrap();
        let base_hash = store.write_blob(b"base\n").unwrap();
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "marker.txt".to_string(),
            hash: target_hash,
            size: 3,
        }]);
        let base_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "base.txt".to_string(),
            hash: base_hash,
            size: 5,
        }]);
        let target_tree_id = target_snapshot.id().unwrap();
        let base_tree_id = base_snapshot.id().unwrap();
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "command_check".to_string(),
            property_spec_with_checks(
                "command_check",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::RunExitCodeIs {
                        run: graft_core::RunPlan {
                            argv: vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "test -f marker.txt".to_string(),
                            ],
                            tree: graft_core::TreePlan::Application {
                                application: graft_core::ApplicationPlan::Current,
                                endpoint: graft_core::ApplicationEndpoint::Target,
                            },
                        },
                        code: 0,
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        config.properties.insert(
            "base_command_check".to_string(),
            property_spec_with_checks(
                "base_command_check",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::RunExitCodeIs {
                        run: graft_core::RunPlan {
                            argv: vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "test -f base.txt".to_string(),
                            ],
                            tree: graft_core::TreePlan::Application {
                                application: graft_core::ApplicationPlan::Current,
                                endpoint: graft_core::ApplicationEndpoint::Base,
                            },
                        },
                        code: 0,
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: Some(base_snapshot),
            target_snapshot: Some(target_snapshot),
            integrity: EvidenceResult::Passed,
        };
        let property =
            workspace_property(config.properties["command_check"].property_ref().unwrap());
        let base_property = workspace_property(
            config.properties["base_command_check"]
                .property_ref()
                .unwrap(),
        );

        let records =
            validate_property(&store, &config, &target, &property, &mut BTreeMap::new()).unwrap();

        let base_records = validate_property(
            &store,
            &config,
            &target,
            &base_property,
            &mut BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(records[0].result, EvidenceResult::Passed);
        assert_eq!(base_records[0].result, EvidenceResult::Passed);
        assert!(
            store
                .paths()
                .derived_worktrees()
                .join(target_tree_id)
                .join("marker.txt")
                .exists(),
            "target run tree must be materialized under derived/worktrees/<tree-id>"
        );
        assert!(
            store
                .paths()
                .derived_worktrees()
                .join(base_tree_id)
                .join("base.txt")
                .exists(),
            "base run tree must be materialized under derived/worktrees/<tree-id>"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_overlay_replaces_file_from_referenced_tree_for_command_runs() {
        let dir = test_workspace("graft-cli-v2-overlay-run-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let base_hash = store.write_blob(b"base\n").unwrap();
        let replacement_hash = store.write_blob(b"ok\n").unwrap();
        let base_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "check.txt".to_string(),
            hash: base_hash,
            size: 5,
        }]);
        let target_snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "replacement.txt".to_string(),
            hash: replacement_hash,
            size: 3,
        }]);
        let mut config = crate::config::GraftConfig::default();
        let target_tree = graft_core::TreePlan::Application {
            application: graft_core::ApplicationPlan::Current,
            endpoint: graft_core::ApplicationEndpoint::Target,
        };
        config.properties.insert(
            "overlay_check".to_string(),
            property_spec_with_checks(
                "overlay_check",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::RunExitCodeIs {
                        run: graft_core::RunPlan {
                            argv: vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "grep -q ok check.txt".to_string(),
                            ],
                            tree: graft_core::TreePlan::WithOverlay {
                                base: Box::new(graft_core::TreePlan::Application {
                                    application: graft_core::ApplicationPlan::Current,
                                    endpoint: graft_core::ApplicationEndpoint::Base,
                                }),
                                overlays: vec![graft_core::OverlayPlan::ReplaceFile {
                                    path: "./check.txt".to_string(),
                                    file: graft_core::FileRefPlan::TreeFile {
                                        tree: Box::new(target_tree),
                                        path: "./replacement.txt".to_string(),
                                    },
                                }],
                            },
                        },
                        code: 0,
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: Some(base_snapshot),
            target_snapshot: Some(target_snapshot),
            integrity: EvidenceResult::Passed,
        };
        let property =
            workspace_property(config.properties["overlay_check"].property_ref().unwrap());

        let records =
            validate_property(&store, &config, &target, &property, &mut BTreeMap::new()).unwrap();

        assert_eq!(records[0].result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_missing_tree_file_overlay_is_unknown_when_consumed() {
        let dir = test_workspace("graft-cli-v2-overlay-missing-file-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let base_snapshot = TreeSnapshot::new(Vec::new());
        let target_snapshot = TreeSnapshot::new(Vec::new());
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "overlay_check".to_string(),
            property_spec_with_checks(
                "overlay_check",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::RunExitCodeIs {
                        run: graft_core::RunPlan {
                            argv: vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()],
                            tree: graft_core::TreePlan::WithOverlay {
                                base: Box::new(graft_core::TreePlan::Application {
                                    application: graft_core::ApplicationPlan::Current,
                                    endpoint: graft_core::ApplicationEndpoint::Base,
                                }),
                                overlays: vec![graft_core::OverlayPlan::ReplaceFile {
                                    path: "check.txt".to_string(),
                                    file: graft_core::FileRefPlan::TreeFile {
                                        tree: Box::new(graft_core::TreePlan::Application {
                                            application: graft_core::ApplicationPlan::Current,
                                            endpoint: graft_core::ApplicationEndpoint::Target,
                                        }),
                                        path: "missing.txt".to_string(),
                                    },
                                }],
                            },
                        },
                        code: 0,
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: Some(base_snapshot),
            target_snapshot: Some(target_snapshot),
            integrity: EvidenceResult::Passed,
        };
        let property =
            workspace_property(config.properties["overlay_check"].property_ref().unwrap());

        let records =
            validate_property(&store, &config, &target, &property, &mut BTreeMap::new()).unwrap();

        assert!(matches!(
            records[0].result,
            EvidenceResult::Unknown { ref reason }
                if reason.contains("file `missing.txt` was not found")
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_same_output_compares_stdout_and_post_file_selectors() {
        let dir = test_workspace("graft-cli-v2-same-output-pass-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        let target_tree = graft_core::TreePlan::Application {
            application: graft_core::ApplicationPlan::Current,
            endpoint: graft_core::ApplicationEndpoint::Target,
        };
        config.properties.insert(
            "same_output_check".to_string(),
            property_spec_with_checks(
                "same_output_check",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::SameOutput {
                        left: graft_core::RunPlan {
                            argv: vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "printf same; printf file > out.txt".to_string(),
                            ],
                            tree: target_tree.clone(),
                        },
                        right: graft_core::RunPlan {
                            argv: vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "printf same; printf file > out.txt".to_string(),
                            ],
                            tree: target_tree,
                        },
                        selectors: vec![
                            graft_core::RunSelectorPlan::Stdout,
                            graft_core::RunSelectorPlan::PostFile {
                                path: "out.txt".to_string(),
                            },
                        ],
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: None,
            target_snapshot: Some(TreeSnapshot::new(Vec::new())),
            integrity: EvidenceResult::Passed,
        };
        let property = workspace_property(
            config.properties["same_output_check"]
                .property_ref()
                .unwrap(),
        );

        let records =
            validate_property(&store, &config, &target, &property, &mut BTreeMap::new()).unwrap();

        assert_eq!(records[0].result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_same_output_fails_on_selector_mismatch_and_unknown_on_missing_post_file() {
        let dir = test_workspace("graft-cli-v2-same-output-fail-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        let target_tree = graft_core::TreePlan::Application {
            application: graft_core::ApplicationPlan::Current,
            endpoint: graft_core::ApplicationEndpoint::Target,
        };
        config.properties.insert(
            "mismatch".to_string(),
            property_spec_with_checks(
                "mismatch",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::SameOutput {
                        left: graft_core::RunPlan {
                            argv: vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "printf left".to_string(),
                            ],
                            tree: target_tree.clone(),
                        },
                        right: graft_core::RunPlan {
                            argv: vec![
                                "/bin/sh".to_string(),
                                "-c".to_string(),
                                "printf right".to_string(),
                            ],
                            tree: target_tree.clone(),
                        },
                        selectors: vec![graft_core::RunSelectorPlan::Stdout],
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        config.properties.insert(
            "missing_post_file".to_string(),
            property_spec_with_checks(
                "missing_post_file",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::SameOutput {
                        left: graft_core::RunPlan {
                            argv: vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()],
                            tree: target_tree.clone(),
                        },
                        right: graft_core::RunPlan {
                            argv: vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()],
                            tree: target_tree,
                        },
                        selectors: vec![graft_core::RunSelectorPlan::PostFile {
                            path: "missing.txt".to_string(),
                        }],
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: None,
            target_snapshot: Some(TreeSnapshot::new(Vec::new())),
            integrity: EvidenceResult::Passed,
        };
        let mismatch = workspace_property(config.properties["mismatch"].property_ref().unwrap());
        let missing = workspace_property(
            config.properties["missing_post_file"]
                .property_ref()
                .unwrap(),
        );

        let mismatch_records =
            validate_property(&store, &config, &target, &mismatch, &mut BTreeMap::new()).unwrap();
        let missing_records =
            validate_property(&store, &config, &target, &missing, &mut BTreeMap::new()).unwrap();

        assert!(matches!(
            mismatch_records[0].result,
            EvidenceResult::Failed { .. }
        ));
        assert!(matches!(
            missing_records[0].result,
            EvidenceResult::Unknown { ref reason }
                if reason.contains("post_file `missing.txt`")
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_previous_failure_last_resolves_target_tree_for_current_property() {
        let dir = test_workspace("graft-cli-v2-previous-failure-last-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        let history_tree = TreePlan::Application {
            application: ApplicationPlan::PreviousFailure {
                selector: HistorySelector::Last,
            },
            endpoint: ApplicationEndpoint::Target,
        };
        let spec = property_spec_with_checks(
            "history_check",
            vec![CheckPlan::Expect {
                probe: ProbePlan::RunExitCodeIs {
                    run: RunPlan {
                        argv: vec![
                            "/bin/sh".to_string(),
                            "-c".to_string(),
                            "test -f selected.txt".to_string(),
                        ],
                        tree: history_tree,
                    },
                    code: 0,
                },
                polarity: ProbePolarity::Success,
            }],
            &[],
        );
        let property_ref = spec.property_ref().unwrap();
        write_historical_failed_patch(&store, "patch:001", &property_ref, "old.txt", b"old\n");
        write_historical_failed_patch(&store, "patch:999", &property_ref, "selected.txt", b"new\n");
        config.properties.insert("history_check".to_string(), spec);
        let property = workspace_property(property_ref);
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:current"),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };

        let records =
            validate_property(&store, &config, &target, &property, &mut BTreeMap::new()).unwrap();

        assert_eq!(records[0].result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_previous_failure_missing_history_is_unknown_when_consumed() {
        let dir = test_workspace("graft-cli-v2-previous-failure-missing-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        let spec = property_spec_with_checks(
            "history_check",
            vec![CheckPlan::Expect {
                probe: ProbePlan::RunExitCodeIs {
                    run: RunPlan {
                        argv: vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()],
                        tree: TreePlan::Application {
                            application: ApplicationPlan::PreviousFailure {
                                selector: HistorySelector::First,
                            },
                            endpoint: ApplicationEndpoint::Target,
                        },
                    },
                    code: 0,
                },
                polarity: ProbePolarity::Success,
            }],
            &[],
        );
        let property = workspace_property(spec.property_ref().unwrap());
        config.properties.insert("history_check".to_string(), spec);
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:current"),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };

        let records =
            validate_property(&store, &config, &target, &property, &mut BTreeMap::new()).unwrap();

        assert!(matches!(
            records[0].result,
            EvidenceResult::Unknown { ref reason }
                if reason.contains("no previous failed application for property")
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_run_exit_code_empty_argv_is_unknown_not_panic() {
        let dir = test_workspace("graft-cli-v2-run-probe-empty-argv-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "command_check".to_string(),
            property_spec_with_checks(
                "command_check",
                vec![CheckPlan::Expect {
                    probe: ProbePlan::RunExitCodeIs {
                        run: graft_core::RunPlan {
                            argv: Vec::new(),
                            tree: graft_core::TreePlan::Application {
                                application: graft_core::ApplicationPlan::Current,
                                endpoint: graft_core::ApplicationEndpoint::Target,
                            },
                        },
                        code: 0,
                    },
                    polarity: ProbePolarity::Success,
                }],
                &[],
            ),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: None,
            target_snapshot: Some(TreeSnapshot::new(Vec::new())),
            integrity: EvidenceResult::Passed,
        };
        let property =
            workspace_property(config.properties["command_check"].property_ref().unwrap());

        let records =
            validate_property(&store, &config, &target, &property, &mut BTreeMap::new()).unwrap();

        assert!(matches!(
            records[0].result,
            EvidenceResult::Unknown { ref reason } if reason.contains("run argv must not be empty")
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_current_verifier_filter_rejects_stale_same_property_id_evidence() {
        let mut config = crate::config::GraftConfig::default();
        let spec = property_spec_with_checks("current", Vec::new(), &[]);
        let property = workspace_property(spec.property_ref().unwrap());
        let current_verifier = verifier_id_for_spec(&spec).unwrap();
        config.properties.insert("current".to_string(), spec);
        let subject = property.evidence_subject("candidate:demo");
        let stale = EvidenceRecord::passed(
            &subject,
            property.property.id.clone(),
            "legacy-verifier-with-same-property-id",
        )
        .unwrap();
        let current =
            EvidenceRecord::passed(&subject, property.property.id.clone(), current_verifier)
                .unwrap();

        let filtered = evidence_for_current_verifiers(
            &config,
            std::slice::from_ref(&property),
            &[stale, current.clone()],
            "candidate:demo",
        )
        .unwrap();

        assert_eq!(filtered, vec![current]);
    }

    #[test]
    fn v2_requires_skip_dependent_when_dependency_does_not_pass() {
        let dir = test_workspace("graft-cli-v2-requires-skip-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "dependency".to_string(),
            property_spec_with_checks(
                "dependency",
                vec![CheckPlan::Unavailable {
                    reason: "dependency not available".to_string(),
                }],
                &[],
            ),
        );
        config.properties.insert(
            "dependent".to_string(),
            property_spec_with_checks("dependent", Vec::new(), &["dependency"]),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };
        let dependent = workspace_property(config.properties["dependent"].property_ref().unwrap());
        let mut memo = BTreeMap::new();

        let records = validate_property(&store, &config, &target, &dependent, &mut memo).unwrap();

        assert!(matches!(
            records[0].result,
            EvidenceResult::Skipped { ref reason }
                if reason.contains("required properties did not pass")
                    && reason.contains("dependency not available")
        ));
        let dependency =
            workspace_property(config.properties["dependency"].property_ref().unwrap());
        assert!(matches!(
            memo.get(&dependency.label()).unwrap()[0].result,
            EvidenceResult::Unknown { ref reason } if reason.contains("dependency not available")
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_requires_allow_dependent_after_dependencies_pass() {
        let dir = test_workspace("graft-cli-v2-requires-pass-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut config = crate::config::GraftConfig::default();
        config.properties.insert(
            "dependency".to_string(),
            property_spec_with_checks("dependency", Vec::new(), &[]),
        );
        config.properties.insert(
            "dependent".to_string(),
            property_spec_with_checks("dependent", Vec::new(), &["dependency"]),
        );
        let target = ValidationTarget {
            subject: ValidationSubject::new("candidate:demo"),
            base_snapshot: None,
            target_snapshot: None,
            integrity: EvidenceResult::Passed,
        };
        let dependent = workspace_property(config.properties["dependent"].property_ref().unwrap());
        let mut memo = BTreeMap::new();

        let records = validate_property(&store, &config, &target, &dependent, &mut memo).unwrap();

        assert_eq!(records[0].result, EvidenceResult::Passed);
        let dependency =
            workspace_property(config.properties["dependency"].property_ref().unwrap());
        assert_eq!(
            memo.get(&dependency.label()).unwrap()[0].result,
            EvidenceResult::Passed
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn test_workspace(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{nanos}", std::process::id()))
    }
}
