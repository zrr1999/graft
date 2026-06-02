use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use graft_core::{
    Evaluator, EvidenceRecord, EvidenceResult, Judge, PropertyDef, PropertyId, Query,
};

#[derive(Debug, thiserror::Error)]
pub enum ValidateError {
    #[error(transparent)]
    Core(#[from] graft_core::CoreError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, ValidateError>;

#[derive(Clone, Debug)]
pub struct ValidationSubject {
    pub id: String,
    pub changed_paths: Vec<String>,
    pub has_change: bool,
    pub patch_validity: Option<EvidenceResult>,
    pub base_worktree: Option<PathBuf>,
    pub target_worktree: Option<PathBuf>,
}

impl ValidationSubject {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            changed_paths: Vec::new(),
            has_change: false,
            patch_validity: None,
            base_worktree: None,
            target_worktree: None,
        }
    }

    pub fn with_change(id: impl Into<String>, changed_paths: Vec<String>) -> Self {
        Self {
            id: id.into(),
            changed_paths,
            has_change: true,
            patch_validity: None,
            base_worktree: None,
            target_worktree: None,
        }
    }

    pub fn with_patch_validity(mut self, result: EvidenceResult) -> Self {
        self.patch_validity = Some(result);
        self
    }

    pub fn with_base_worktree(mut self, path: impl Into<PathBuf>) -> Self {
        self.base_worktree = Some(path.into());
        self
    }

    pub fn with_target_worktree(mut self, path: impl Into<PathBuf>) -> Self {
        self.target_worktree = Some(path.into());
        self
    }

    pub fn with_validation_worktree(self, path: impl Into<PathBuf>) -> Self {
        self.with_target_worktree(path)
    }
}

#[derive(Clone, Debug)]
pub struct ValidationEngine {
    cwd: PathBuf,
}

impl Default for ValidationEngine {
    fn default() -> Self {
        Self::new(".")
    }
}

impl ValidationEngine {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    pub fn validate(
        &self,
        subject: &ValidationSubject,
        property: &PropertyDef,
    ) -> Result<EvidenceRecord> {
        let property_id = property.property_id()?;
        let verifier = verifier_id(property);

        let context = match QueryContext::prepare(subject, property, &self.cwd) {
            Ok(context) => context,
            Err(reason) => return unknown_record(subject, property_id, verifier, reason),
        };

        let answer = match self.evaluate(subject, property, &context) {
            Ok(answer) => answer,
            Err(reason) => return unknown_record(subject, property_id, verifier, reason),
        };

        let result = self.judge(subject, property, &context, &answer);
        EvidenceRecord::new(subject.id.clone(), property_id, verifier, result)
            .map_err(ValidateError::from)
    }

    fn evaluate(
        &self,
        subject: &ValidationSubject,
        property: &PropertyDef,
        context: &QueryContext,
    ) -> std::result::Result<EvaluationAnswer, String> {
        match &property.evaluator {
            Evaluator::Builtin { name, options } => evaluate_builtin(subject, name, options),
            Evaluator::Command {
                command,
                args,
                env,
                setup,
                pre,
                teardown,
                timeout_secs,
            } => {
                let cwd = context.target_or_default();
                let env = runtime_env(subject, property, context, env);
                run_phase_list("setup", setup, cwd, &env, *timeout_secs)?;
                run_phase_list("pre", pre, cwd, &env, *timeout_secs)?;
                let answer =
                    run_command(command, args, cwd, &env, *timeout_secs).map_err(|error| {
                        format!("command failed before producing an answer: {error}")
                    })?;
                if answer.status_code.is_none() {
                    run_teardown(teardown, cwd, &env, *timeout_secs);
                    return Err("command terminated without an exit code".to_string());
                }
                run_teardown(teardown, cwd, &env, *timeout_secs);
                Ok(EvaluationAnswer::Command(answer))
            }
            Evaluator::Pair {
                command,
                args,
                env,
                setup,
                pre,
                teardown,
                timeout_secs,
            } => {
                let base = context
                    .base
                    .as_deref()
                    .ok_or_else(|| "base worktree was not prepared".to_string())?;
                let target = context
                    .target
                    .as_deref()
                    .ok_or_else(|| "target worktree was not prepared".to_string())?;
                let env = runtime_env(subject, property, context, env);

                for cwd in [base, target] {
                    run_phase_list("setup", setup, cwd, &env, *timeout_secs)?;
                    run_phase_list("pre", pre, cwd, &env, *timeout_secs)?;
                }

                let base_answer =
                    run_command(command, args, base, &env, *timeout_secs).map_err(|error| {
                        format!("base command failed before producing an answer: {error}")
                    })?;
                let target_answer = run_command(command, args, target, &env, *timeout_secs)
                    .map_err(|error| {
                        format!("target command failed before producing an answer: {error}")
                    })?;

                let missing_exit =
                    base_answer.status_code.is_none() || target_answer.status_code.is_none();
                for cwd in [base, target] {
                    run_teardown(teardown, cwd, &env, *timeout_secs);
                }
                if missing_exit {
                    return Err("pair command terminated without an exit code".to_string());
                }

                Ok(EvaluationAnswer::Pair {
                    base: base_answer,
                    target: target_answer,
                })
            }
        }
    }

    fn judge(
        &self,
        subject: &ValidationSubject,
        property: &PropertyDef,
        context: &QueryContext,
        answer: &EvaluationAnswer,
    ) -> EvidenceResult {
        match &property.judge {
            Judge::BoolTrue => judge_bool_true(answer),
            Judge::ExitOk | Judge::ExitCodeZero => judge_exit_ok(answer),
            Judge::Pairwise => judge_pairwise(answer),
            Judge::Command {
                command,
                args,
                env,
                timeout_secs,
            } => {
                let cwd = context.target_or_default();
                let env = runtime_env(subject, property, context, env);
                match run_command(command, args, cwd, &env, *timeout_secs) {
                    Ok(output) if output.status_code.is_none() => EvidenceResult::Unknown {
                        reason: "judge command terminated without an exit code".to_string(),
                    },
                    Ok(output) if output.success => EvidenceResult::Passed,
                    Ok(output) => EvidenceResult::Failed {
                        reason: non_empty_reason(&output, "judge command exited unsuccessfully"),
                    },
                    Err(error) => EvidenceResult::Unknown {
                        reason: format!(
                            "judge command failed before producing a decision: {error}"
                        ),
                    },
                }
            }
            Judge::StdoutContains { text } => match answer {
                EvaluationAnswer::Command(output) if output.stdout.contains(text) => {
                    EvidenceResult::Passed
                }
                EvaluationAnswer::Command(output) => EvidenceResult::Failed {
                    reason: format!("stdout did not contain `{text}`: {}", output.stdout.trim()),
                },
                _ => EvidenceResult::Failed {
                    reason: "stdout_contains judge requires a command answer".to_string(),
                },
            },
            Judge::JsonEquals { .. } => EvidenceResult::Unknown {
                reason: "json_equals judge is not implemented by graft-validate yet".to_string(),
            },
        }
    }
}

pub fn validate_property(
    subject: &ValidationSubject,
    property: &PropertyDef,
) -> Result<EvidenceRecord> {
    ValidationEngine::default().validate(subject, property)
}

#[derive(Clone, Debug)]
struct QueryContext {
    kind: &'static str,
    base: Option<PathBuf>,
    target: Option<PathBuf>,
    default_cwd: PathBuf,
}

impl QueryContext {
    fn prepare(
        subject: &ValidationSubject,
        property: &PropertyDef,
        default_cwd: &Path,
    ) -> std::result::Result<Self, String> {
        match &property.query {
            Query::ChangeMeta | Query::Change => Ok(Self {
                kind: "change_meta",
                base: subject.base_worktree.clone(),
                target: subject.target_worktree.clone(),
                default_cwd: default_cwd.to_path_buf(),
            }),
            Query::TargetSnapshot | Query::Files { .. } | Query::Command { .. } => {
                let target = require_dir(subject.target_worktree.as_deref(), "target")?;
                Ok(Self {
                    kind: "target_snapshot",
                    base: subject.base_worktree.clone(),
                    target: Some(target),
                    default_cwd: default_cwd.to_path_buf(),
                })
            }
            Query::BaseAndTarget => {
                let base = require_dir(subject.base_worktree.as_deref(), "base")?;
                let target = require_dir(subject.target_worktree.as_deref(), "target")?;
                Ok(Self {
                    kind: "base_and_target",
                    base: Some(base),
                    target: Some(target),
                    default_cwd: default_cwd.to_path_buf(),
                })
            }
        }
    }

    fn target_or_default(&self) -> &Path {
        self.target.as_deref().unwrap_or(&self.default_cwd)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EvaluationAnswer {
    Bool(bool),
    Command(CommandAnswer),
    Pair {
        base: CommandAnswer,
        target: CommandAnswer,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandAnswer {
    status_code: Option<i32>,
    success: bool,
    stdout: String,
    stderr: String,
}

impl CommandAnswer {
    fn reason(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.trim().to_string()
        } else {
            self.stderr.trim().to_string()
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum RunError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

fn evaluate_builtin(
    subject: &ValidationSubject,
    name: &str,
    options: &BTreeMap<String, String>,
) -> std::result::Result<EvaluationAnswer, String> {
    match normalize_builtin_name(name).as_str() {
        "has_change" => Ok(EvaluationAnswer::Bool(subject.has_change)),
        "valid_patch" => match &subject.patch_validity {
            Some(EvidenceResult::Passed) => Ok(EvaluationAnswer::Bool(true)),
            Some(EvidenceResult::Failed { .. }) => Ok(EvaluationAnswer::Bool(false)),
            Some(EvidenceResult::Unknown { reason } | EvidenceResult::Skipped { reason }) => {
                Err(reason.clone())
            }
            None => Err("no patch validity result was available".to_string()),
        },
        "paths_none_match" => {
            let patterns = option_patterns(options);
            let matched = subject
                .changed_paths
                .iter()
                .any(|path| patterns.iter().any(|pattern| path_matches(pattern, path)));
            Ok(EvaluationAnswer::Bool(!matched))
        }
        "paths_all_match" => {
            if subject.changed_paths.is_empty() {
                return Err("no changed paths were available".to_string());
            }
            let patterns = option_patterns(options);
            let all_match = subject
                .changed_paths
                .iter()
                .all(|path| patterns.iter().any(|pattern| path_matches(pattern, path)));
            Ok(EvaluationAnswer::Bool(all_match))
        }
        other => Err(format!("unknown builtin evaluator `{other}`")),
    }
}

fn judge_bool_true(answer: &EvaluationAnswer) -> EvidenceResult {
    match answer {
        EvaluationAnswer::Bool(true) => EvidenceResult::Passed,
        EvaluationAnswer::Bool(false) => EvidenceResult::Failed {
            reason: "boolean answer was false".to_string(),
        },
        _ => EvidenceResult::Failed {
            reason: "bool_true judge requires a boolean answer".to_string(),
        },
    }
}

fn judge_exit_ok(answer: &EvaluationAnswer) -> EvidenceResult {
    match answer {
        EvaluationAnswer::Command(output) if output.success => EvidenceResult::Passed,
        EvaluationAnswer::Command(output) => EvidenceResult::Failed {
            reason: non_empty_reason(output, "command exited unsuccessfully"),
        },
        EvaluationAnswer::Pair { base, target } if base.success && target.success => {
            EvidenceResult::Passed
        }
        EvaluationAnswer::Pair { base, target } => EvidenceResult::Failed {
            reason: format!(
                "base status {:?}, target status {:?}",
                base.status_code, target.status_code
            ),
        },
        _ => EvidenceResult::Failed {
            reason: "exit_ok judge requires command output".to_string(),
        },
    }
}

fn judge_pairwise(answer: &EvaluationAnswer) -> EvidenceResult {
    match answer {
        EvaluationAnswer::Pair { base, target }
            if base.success && target.success && base.stdout == target.stdout =>
        {
            EvidenceResult::Passed
        }
        EvaluationAnswer::Pair { base, target } if !base.success || !target.success => {
            EvidenceResult::Failed {
                reason: format!(
                    "pair command did not exit successfully: base={}, target={}",
                    non_empty_reason(base, "base command exited unsuccessfully"),
                    non_empty_reason(target, "target command exited unsuccessfully")
                ),
            }
        }
        EvaluationAnswer::Pair { base, target } => EvidenceResult::Failed {
            reason: format!(
                "pairwise stdout differed: base=`{}`, target=`{}`",
                base.stdout.trim(),
                target.stdout.trim()
            ),
        },
        _ => EvidenceResult::Failed {
            reason: "pairwise judge requires a pair answer".to_string(),
        },
    }
}

fn run_phase_list(
    phase: &str,
    commands: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout_secs: Option<u64>,
) -> std::result::Result<(), String> {
    for command in commands {
        let output = run_shell_command(command, cwd, env, timeout_secs)
            .map_err(|error| format!("{phase} failed before producing an answer: {error}"))?;
        if output.status_code.is_none() {
            return Err(format!("{phase} terminated without an exit code"));
        }
        if !output.success {
            return Err(non_empty_reason(
                &output,
                &format!("{phase} command exited unsuccessfully"),
            ));
        }
    }
    Ok(())
}

fn run_teardown(
    commands: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout_secs: Option<u64>,
) {
    for command in commands {
        match run_shell_command(command, cwd, env, timeout_secs) {
            Ok(output) if output.success => {}
            Ok(output) => eprintln!(
                "warning: verifier teardown failed: {}",
                non_empty_reason(&output, "teardown command exited unsuccessfully")
            ),
            Err(error) => eprintln!("warning: verifier teardown failed: {error}"),
        }
    }
}

fn run_shell_command(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout_secs: Option<u64>,
) -> std::result::Result<CommandAnswer, RunError> {
    run_command(command, &[], cwd, env, timeout_secs)
}

fn run_command(
    command: &str,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout_secs: Option<u64>,
) -> std::result::Result<CommandAnswer, RunError> {
    let mut process = if args.is_empty() {
        shell_command(command)
    } else {
        let mut process = Command::new(command);
        process.args(args);
        process
    };
    process
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_timeout(process, timeout_secs.map(Duration::from_secs))
}

fn run_with_timeout(
    mut command: Command,
    timeout: Option<Duration>,
) -> std::result::Result<CommandAnswer, RunError> {
    let mut child = command.spawn()?;
    if let Some(timeout) = timeout {
        let start = Instant::now();
        loop {
            if child.try_wait()?.is_some() {
                let output = child.wait_with_output()?;
                return Ok(command_answer(output));
            }
            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait_with_output();
                return Err(RunError::Timeout(timeout));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let output = child.wait_with_output()?;
    Ok(command_answer(output))
}

fn command_answer(output: std::process::Output) -> CommandAnswer {
    CommandAnswer {
        status_code: output.status.code(),
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn runtime_env(
    subject: &ValidationSubject,
    property: &PropertyDef,
    context: &QueryContext,
    custom: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("GRAFT_SUBJECT".to_string(), subject.id.clone());
    env.insert("GRAFT_PROPERTY_NAME".to_string(), property.name.clone());
    env.insert(
        "GRAFT_PROPERTY_ID".to_string(),
        property
            .property_id()
            .unwrap_or_else(|_| PropertyId::new("property:unknown"))
            .as_str()
            .to_string(),
    );
    env.insert("GRAFT_QUERY_KIND".to_string(), context.kind.to_string());
    env.insert(
        "GRAFT_CHANGED_PATHS".to_string(),
        subject.changed_paths.join("\n"),
    );
    if let Some(base) = &context.base {
        env.insert(
            "GRAFT_BASE_WORKTREE".to_string(),
            base.to_string_lossy().to_string(),
        );
    }
    if let Some(target) = &context.target {
        let target = target.to_string_lossy().to_string();
        env.insert("GRAFT_TARGET_WORKTREE".to_string(), target.clone());
        env.insert("GRAFT_VALIDATION_WORKTREE".to_string(), target);
    }
    env.extend(custom.clone());
    env
}

fn require_dir(path: Option<&Path>, label: &str) -> std::result::Result<PathBuf, String> {
    let path = path.ok_or_else(|| format!("{label} worktree was not provided"))?;
    if path.is_dir() {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "{label} worktree is not a directory: {}",
            path.display()
        ))
    }
}

fn unknown_record(
    subject: &ValidationSubject,
    property: PropertyId,
    verifier: String,
    reason: impl Into<String>,
) -> Result<EvidenceRecord> {
    EvidenceRecord::unknown(subject.id.clone(), property, verifier, reason.into())
        .map_err(ValidateError::from)
}

fn verifier_id(property: &PropertyDef) -> String {
    match &property.evaluator {
        Evaluator::Builtin { name, .. } => format!("builtin:{name}"),
        Evaluator::Command { command, .. } => format!("command:{command}"),
        Evaluator::Pair { command, .. } => format!("pair:{command}"),
    }
}

fn non_empty_reason(output: &CommandAnswer, fallback: &str) -> String {
    let reason = output.reason();
    if reason.is_empty() {
        fallback.to_string()
    } else {
        reason
    }
}

fn option_patterns(options: &BTreeMap<String, String>) -> Vec<String> {
    options
        .get("patterns")
        .or_else(|| options.get("pattern"))
        .map(|value| {
            value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|patterns| !patterns.is_empty())
        .unwrap_or_else(|| vec!["*".to_string()])
}

fn normalize_builtin_name(name: &str) -> String {
    let mut normalized = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                normalized.push('_');
            }
            normalized.push(ch.to_ascii_lowercase());
        } else if ch == '-' {
            normalized.push('_');
        } else {
            normalized.push(ch);
        }
    }
    normalized
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

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    shell
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "graft-validate-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn property(query: Query, evaluator: Evaluator, judge: Judge) -> PropertyDef {
        PropertyDef {
            name: "TestProperty".to_string(),
            query,
            evaluator,
            judge,
        }
    }

    fn command_eval(command: &str) -> Evaluator {
        Evaluator::Command {
            command: command.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            setup: Vec::new(),
            pre: Vec::new(),
            teardown: Vec::new(),
            timeout_secs: Some(5),
        }
    }

    fn pair_eval(command: &str) -> Evaluator {
        Evaluator::Pair {
            command: command.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            setup: Vec::new(),
            pre: Vec::new(),
            teardown: Vec::new(),
            timeout_secs: Some(5),
        }
    }

    #[test]
    fn change_meta_builtin_bool_true_passes() {
        let subject =
            ValidationSubject::with_change("candidate:demo", vec!["src/lib.rs".to_string()]);
        let property = property(
            Query::ChangeMeta,
            Evaluator::Builtin {
                name: "has_change".to_string(),
                options: BTreeMap::new(),
            },
            Judge::BoolTrue,
        );

        let evidence = validate_property(&subject, &property).unwrap();

        assert_eq!(evidence.result, EvidenceResult::Passed);
    }

    #[test]
    fn target_snapshot_command_injects_graft_env_vars() {
        let target = temp_dir("target-env");
        fs::write(target.join("ok.txt"), "ok").unwrap();
        let subject = ValidationSubject::with_change("candidate:demo", vec!["ok.txt".to_string()])
            .with_target_worktree(&target);
        let command = if cfg!(windows) {
            "if not \"%GRAFT_TARGET_WORKTREE%\"==\"\" (exit /b 0) else (exit /b 1)"
        } else {
            "test -n \"$GRAFT_TARGET_WORKTREE\" && test \"$GRAFT_VALIDATION_WORKTREE\" = \"$GRAFT_TARGET_WORKTREE\""
        };
        let property = property(Query::TargetSnapshot, command_eval(command), Judge::ExitOk);

        let evidence = validate_property(&subject, &property).unwrap();

        assert_eq!(evidence.result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn base_and_target_pairwise_runs_setup_in_both_worktrees() {
        let base = temp_dir("pair-base");
        let target = temp_dir("pair-target");
        let subject =
            ValidationSubject::with_change("candidate:demo", vec!["value.txt".to_string()])
                .with_base_worktree(&base)
                .with_target_worktree(&target);
        let mut evaluator = pair_eval(if cfg!(windows) {
            "type marker.txt"
        } else {
            "cat marker.txt"
        });
        if let Evaluator::Pair { setup, .. } = &mut evaluator {
            setup.push(if cfg!(windows) {
                "echo same>marker.txt".to_string()
            } else {
                "printf same > marker.txt".to_string()
            });
        }
        let property = property(Query::BaseAndTarget, evaluator, Judge::Pairwise);

        let evidence = validate_property(&subject, &property).unwrap();

        assert_eq!(evidence.result, EvidenceResult::Passed);
        assert!(base.join("marker.txt").is_file());
        assert!(target.join("marker.txt").is_file());
        let _ = fs::remove_dir_all(base);
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn exit_ok_judge_turns_nonzero_command_into_failed() {
        let target = temp_dir("command-fail");
        let subject = ValidationSubject::with_change("candidate:demo", vec!["x".to_string()])
            .with_target_worktree(&target);
        let command = if cfg!(windows) { "exit /b 7" } else { "exit 7" };
        let property = property(Query::TargetSnapshot, command_eval(command), Judge::ExitOk);

        let evidence = validate_property(&subject, &property).unwrap();

        assert!(matches!(evidence.result, EvidenceResult::Failed { .. }));
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn setup_failure_is_unknown() {
        let target = temp_dir("setup-fail");
        let subject = ValidationSubject::with_change("candidate:demo", vec!["x".to_string()])
            .with_target_worktree(&target);
        let mut evaluator = command_eval(if cfg!(windows) { "exit /b 0" } else { "true" });
        if let Evaluator::Command { setup, .. } = &mut evaluator {
            setup.push(if cfg!(windows) { "exit /b 2" } else { "exit 2" }.to_string());
        }
        let property = property(Query::TargetSnapshot, evaluator, Judge::ExitOk);

        let evidence = validate_property(&subject, &property).unwrap();

        assert!(matches!(evidence.result, EvidenceResult::Unknown { .. }));
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn pre_failure_is_unknown() {
        let target = temp_dir("pre-fail");
        let subject = ValidationSubject::with_change("candidate:demo", vec!["x".to_string()])
            .with_target_worktree(&target);
        let mut evaluator = command_eval(if cfg!(windows) { "exit /b 0" } else { "true" });
        if let Evaluator::Command { pre, .. } = &mut evaluator {
            pre.push(if cfg!(windows) { "exit /b 3" } else { "exit 3" }.to_string());
        }
        let property = property(Query::TargetSnapshot, evaluator, Judge::ExitOk);

        let evidence = validate_property(&subject, &property).unwrap();

        assert!(matches!(evidence.result, EvidenceResult::Unknown { .. }));
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn timeout_is_unknown() {
        let target = temp_dir("timeout");
        let subject = ValidationSubject::with_change("candidate:demo", vec!["x".to_string()])
            .with_target_worktree(&target);
        let mut evaluator = command_eval(if cfg!(windows) {
            "ping -n 3 127.0.0.1 >NUL"
        } else {
            "sleep 2"
        });
        if let Evaluator::Command { timeout_secs, .. } = &mut evaluator {
            *timeout_secs = Some(1);
        }
        let property = property(Query::TargetSnapshot, evaluator, Judge::ExitOk);

        let evidence = validate_property(&subject, &property).unwrap();

        assert!(matches!(evidence.result, EvidenceResult::Unknown { .. }));
        let _ = fs::remove_dir_all(target);
    }

    #[cfg(unix)]
    #[test]
    fn signal_without_exit_code_is_unknown() {
        let target = temp_dir("signal");
        let subject = ValidationSubject::with_change("candidate:demo", vec!["x".to_string()])
            .with_target_worktree(&target);
        let property = property(
            Query::TargetSnapshot,
            command_eval("kill -TERM $$"),
            Judge::ExitOk,
        );

        let evidence = validate_property(&subject, &property).unwrap();

        assert!(matches!(evidence.result, EvidenceResult::Unknown { .. }));
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn teardown_failure_is_warn_only() {
        let target = temp_dir("teardown-fail");
        let subject = ValidationSubject::with_change("candidate:demo", vec!["x".to_string()])
            .with_target_worktree(&target);
        let mut evaluator = command_eval(if cfg!(windows) { "exit /b 0" } else { "true" });
        if let Evaluator::Command { teardown, .. } = &mut evaluator {
            teardown.push(if cfg!(windows) { "exit /b 9" } else { "exit 9" }.to_string());
        }
        let property = property(Query::TargetSnapshot, evaluator, Judge::ExitOk);

        let evidence = validate_property(&subject, &property).unwrap();

        assert_eq!(evidence.result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn command_judge_can_read_base_and_target_worktree_env() {
        let base = temp_dir("judge-base");
        let target = temp_dir("judge-target");
        fs::write(base.join("value.txt"), "same").unwrap();
        fs::write(target.join("value.txt"), "same").unwrap();
        let subject =
            ValidationSubject::with_change("candidate:demo", vec!["value.txt".to_string()])
                .with_base_worktree(&base)
                .with_target_worktree(&target);
        let judge_command = if cfg!(windows) {
            "fc \"%GRAFT_BASE_WORKTREE%\\value.txt\" \"%GRAFT_TARGET_WORKTREE%\\value.txt\" >NUL"
        } else {
            "cmp \"$GRAFT_BASE_WORKTREE/value.txt\" \"$GRAFT_TARGET_WORKTREE/value.txt\""
        };
        let property = property(
            Query::BaseAndTarget,
            pair_eval(if cfg!(windows) {
                "echo ignored"
            } else {
                "printf ignored"
            }),
            Judge::Command {
                command: judge_command.to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
                timeout_secs: Some(5),
            },
        );

        let evidence = validate_property(&subject, &property).unwrap();

        assert_eq!(evidence.result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(base);
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn real_command_verifier_smoke_prints_stdout() {
        let target = temp_dir("real-command-smoke");
        let subject =
            ValidationSubject::with_change("candidate:demo", vec!["smoke.txt".to_string()])
                .with_target_worktree(&target);
        let smoke = if cfg!(windows) {
            "echo smoke:%GRAFT_TARGET_WORKTREE%"
        } else {
            "printf 'smoke:%s' \"$GRAFT_TARGET_WORKTREE\""
        };
        let property = property(
            Query::TargetSnapshot,
            command_eval(smoke),
            Judge::StdoutContains {
                text: "smoke:".to_string(),
            },
        );

        let env = runtime_env(
            &subject,
            &property,
            &QueryContext::prepare(&subject, &property, Path::new(".")).unwrap(),
            &BTreeMap::new(),
        );
        let output = run_shell_command(smoke, &target, &env, Some(5)).unwrap();
        println!("real command verifier smoke stdout: {}", output.stdout);

        let evidence = validate_property(&subject, &property).unwrap();
        assert_eq!(evidence.result, EvidenceResult::Passed);
        let _ = fs::remove_dir_all(target);
    }
}
