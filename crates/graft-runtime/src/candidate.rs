use std::path::Path;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use graft_client::{daemon_socket_path, request_result_or_spawn};
use serde_json::{Map, Value, json};

use crate::daemon_client::{
    add_workspace_route, render_json_result, require_string_array_field, required_string_field,
};
use crate::view::CommandEnvelope;

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum CandidateCommand {
    /// Create a candidate from an existing scratch id
    FromScratch(CandidateFromScratchArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct CandidateFromScratchArgs {
    /// Scratch id to turn into a candidate
    scratch: String,
    #[arg(
        long = "expect",
        help = "Whole-state constraint primitive to validate immediately and add to the candidate constraint, for example tests_pass (repeatable; repeats compose as all_of)"
    )]
    constraint: Vec<String>,
    #[arg(
        long,
        default_value = "graft-cli",
        help = "Provenance producer label recorded on the candidate"
    )]
    producer: String,
    #[arg(
        long,
        help = "Short human description recorded in candidate provenance"
    )]
    message: Option<String>,
}

impl CandidateFromScratchArgs {
    pub(crate) fn validates_on_create(&self) -> bool {
        !self.constraint.is_empty()
    }
}

pub(crate) fn run_candidate_command(
    workspace_root: &Path,
    workspace_id: &str,
    socket: Option<&Path>,
    command: &CandidateCommand,
) -> Result<CommandEnvelope> {
    let socket = match socket {
        Some(socket) => socket.to_path_buf(),
        None => daemon_socket_path()?,
    };
    let (op, mut params) = match command {
        CandidateCommand::FromScratch(args) => {
            ("candidate_from_scratch", from_scratch_params(args)?)
        }
    };
    add_workspace_route(&mut params, workspace_root, workspace_id)?;
    let result = request_result_or_spawn(workspace_root, &socket, op, params)?;
    result_to_envelope(result)
}

fn from_scratch_params(args: &CandidateFromScratchArgs) -> Result<Value> {
    if args.producer.trim().is_empty() {
        bail!("[E_BAD_PARAMS] --producer must not be empty");
    }
    if let Some(message) = &args.message
        && message.trim().is_empty()
    {
        bail!("[E_BAD_PARAMS] --message must not be empty when provided");
    }
    let mut params = Map::new();
    params.insert("scratch".to_string(), json!(args.scratch));
    params.insert("constraint".to_string(), json!(args.constraint));
    params.insert("producer".to_string(), json!(args.producer));
    if let Some(message) = &args.message {
        params.insert("message".to_string(), json!(message));
    }
    Ok(Value::Object(params))
}

fn result_to_envelope(result: Value) -> Result<CommandEnvelope> {
    let candidate_id = required_string_field(&result, "candidate_from_scratch", "candidate")?;
    let _scratch_id = required_string_field(&result, "candidate_from_scratch", "scratch")?;
    require_string_array_field(&result, "candidate_from_scratch", "changed_paths")?;
    Ok(CommandEnvelope {
        message: Some(render_json_result(&result)?),
        result: Some(result.clone()),
        candidate_id: Some(candidate_id),
        cache_changed: true,
        registry_changed: false,
        git_changed: false,
        ..CommandEnvelope::ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn result_to_envelope_extracts_candidate_id() {
        let envelope = result_to_envelope(json!({
            "scratch": "scratch:abc",
            "candidate": "candidate:def",
            "changed_paths": ["a.txt"]
        }))
        .unwrap();

        assert_eq!(
            envelope.result.as_ref().unwrap()["candidate"],
            "candidate:def"
        );
        assert_eq!(envelope.result.as_ref().unwrap()["scratch"], "scratch:abc");
        assert_eq!(envelope.candidate_id.as_deref(), Some("candidate:def"));
        assert!(envelope.cache_changed);
    }

    #[test]
    fn from_scratch_params_omit_absent_optional_message() {
        let args = CandidateFromScratchArgs {
            scratch: "scratch:abc".to_string(),
            constraint: vec![
                "only_touches_docs".to_string(),
                "cargo_tests_pass".to_string(),
            ],
            producer: "test".to_string(),
            message: None,
        };

        let params = from_scratch_params(&args).unwrap();

        assert_eq!(params["scratch"].as_str(), Some("scratch:abc"));
        assert_eq!(params["producer"].as_str(), Some("test"));
        assert_eq!(
            params["constraint"],
            json!(["only_touches_docs", "cargo_tests_pass"])
        );
        assert!(
            params.get("message").is_none(),
            "message must be omitted instead of serialized as null"
        );
    }

    #[test]
    fn from_scratch_params_include_present_message() {
        let args = CandidateFromScratchArgs {
            scratch: "scratch:abc".to_string(),
            constraint: Vec::new(),
            producer: "test".to_string(),
            message: Some("ready".to_string()),
        };

        let params = from_scratch_params(&args).unwrap();

        assert_eq!(params["message"].as_str(), Some("ready"));
    }

    #[test]
    fn from_scratch_params_reject_blank_provenance_metadata() {
        for (producer, message, expected) in [
            (" \t", None, "--producer must not be empty"),
            (
                "test",
                Some(" \t".to_string()),
                "--message must not be empty",
            ),
        ] {
            let args = CandidateFromScratchArgs {
                scratch: "scratch:abc".to_string(),
                constraint: Vec::new(),
                producer: producer.to_string(),
                message,
            };

            let error = from_scratch_params(&args).unwrap_err().to_string();

            assert!(error.contains("[E_BAD_PARAMS]"), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn result_to_envelope_requires_candidate_success_contract() {
        for (result, expected) in [
            (
                json!({"candidate": "candidate:def", "changed_paths": []}),
                "missing string field `scratch`",
            ),
            (
                json!({"scratch": "scratch:abc", "changed_paths": []}),
                "missing string field `candidate`",
            ),
            (
                json!({"scratch": "scratch:abc", "candidate": "candidate:def"}),
                "missing string array field `changed_paths`",
            ),
            (
                json!({
                    "scratch": "scratch:abc",
                    "candidate": "candidate:def",
                    "changed_paths": ["a.txt", 42]
                }),
                "field `changed_paths` item 1 must be string",
            ),
        ] {
            let error = result_to_envelope(result).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
    }
}
