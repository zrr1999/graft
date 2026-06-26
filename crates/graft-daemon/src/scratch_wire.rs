//! Daemon wire-level scratch operation handlers.
//!
//! This module is the boundary between daemon routing and the in-memory scratch
//! state machine. `graft-scratch` owns scratch state transitions; the daemon owns
//! JSON parameter interpretation, success payloads, and wire error mapping.

use graft_client::WireResponse;
use graft_core::{BaseRefSpec, HashlineEdit, ScratchId, StateId};
use graft_scratch::{ReadMode, ScratchBaseMetadata, ScratchEngine, ScratchError, ScratchOpen};
use graft_store::{TreeGrepOptions, TreeListOptions, VirtualBaseRef};
use serde_json::{Value, json};

use crate::{HandlerResult, WithId, bad_field_type, bad_params, bad_params_message, missing_field};

pub(crate) fn scratch_open_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    match scratch_source(params) {
        Ok(ScratchSourceParam::Base(base)) => match engine.open_with_metadata(base) {
            Ok(open) => (WireResponse::ok(id, scratch_open_json(open)), false),
            Err(error) => scratch_error_response(id, error),
        },
        Ok(ScratchSourceParam::Materialized {
            base_state,
            tree_id,
        }) => match engine.open_materialized_with_metadata(base_state, &tree_id) {
            Ok(open) => (WireResponse::ok(id, scratch_open_json(open)), false),
            Err(error) => scratch_error_response(id, error),
        },
        Ok(ScratchSourceParam::From(_)) => (
            WireResponse::error(
                id,
                "E_BAD_PARAMS",
                "scratch_open does not accept from; provide base or resolved base_state/base_tree",
            ),
            false,
        ),
        Err(response) => (response.with_id(id), false),
    }
}

pub(crate) fn scratch_read_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let source = scratch_source(params);
    let path = required_str(params, "path");
    let mode = optional_str(params, "mode")
        .and_then(|mode| mode.map(parse_read_mode).unwrap_or(Ok(ReadMode::Hashlines)));
    match (source, path, mode) {
        (Ok(source), Ok(path), Ok(mode)) => {
            match resolve_scratch_source(engine, source).and_then(|source| {
                engine
                    .read(&source.scratch, path, mode)
                    .map(|read| (source, read))
            }) {
                Ok((source, read)) => (
                    WireResponse::ok(
                        id,
                        json!({
                            "scratch": read.scratch,
                            "base_state": source.base_state,
                            "base_tree": source.base_tree,
                            "path": read.path,
                            "file_view_hash": read.file_view_hash,
                            "content": read.content,
                            "bytes_len": read.bytes.len()
                        }),
                    ),
                    false,
                ),
                Err(error) => scratch_error_response(id, error),
            }
        }
        (Err(response), _, _) | (_, Err(response), _) | (_, _, Err(response)) => {
            (response.with_id(id), false)
        }
    }
}

pub(crate) fn scratch_write_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let source = scratch_source(params);
    let path = required_str(params, "path");
    let content = required_str(params, "content");
    match (source, path, content) {
        (Ok(source), Ok(path), Ok(content)) => match resolve_scratch_source(engine, source)
            .and_then(|source| {
                engine
                    .write(&source.scratch, path, content.as_bytes())
                    .map(|write| (source, write))
            }) {
            Ok((source, write)) => (
                WireResponse::ok(
                    id,
                    json!({
                        "parent": write.parent,
                        "scratch": write.scratch,
                        "base_state": source.base_state,
                        "base_tree": source.base_tree,
                        "path": write.path,
                        "changed_paths": [write.path],
                        "content_hash": write.content_hash,
                        "size": write.size
                    }),
                ),
                false,
            ),
            Err(error) => scratch_error_response(id, error),
        },
        (Err(response), _, _) | (_, Err(response), _) | (_, _, Err(response)) => {
            (response.with_id(id), false)
        }
    }
}

pub(crate) fn scratch_delete_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let source = scratch_source(params);
    let path = required_str(params, "path");
    match (source, path) {
        (Ok(source), Ok(path)) => match resolve_scratch_source(engine, source).and_then(|source| {
            engine
                .delete(&source.scratch, path)
                .map(|delete| (source, delete))
        }) {
            Ok((source, delete)) => (
                WireResponse::ok(
                    id,
                    json!({
                        "parent": delete.parent,
                        "scratch": delete.scratch,
                        "base_state": source.base_state,
                        "base_tree": source.base_tree,
                        "path": delete.path,
                        "changed_paths": [delete.path]
                    }),
                ),
                false,
            ),
            Err(error) => scratch_error_response(id, error),
        },
        (Err(response), _) | (_, Err(response)) => (response.with_id(id), false),
    }
}

pub(crate) fn scratch_edit_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let source = scratch_source(params);
    let path = required_str(params, "path");
    let edits = params
        .get("edits")
        .cloned()
        .ok_or_else(|| missing_field("edits"))
        .and_then(|value| serde_json::from_value::<Vec<HashlineEdit>>(value).map_err(bad_params));
    match (source, path, edits) {
        (Ok(source), Ok(path), Ok(edits)) => {
            match resolve_scratch_source(engine, source).and_then(|source| {
                engine
                    .edit(&source.scratch, path, edits)
                    .map(|edit| (source, edit))
            }) {
                Ok((source, edit)) => (
                    WireResponse::ok(
                        id,
                        json!({
                            "parent": edit.parent,
                            "scratch": edit.scratch,
                            "base_state": source.base_state,
                            "base_tree": source.base_tree,
                            "path": edit.path,
                            "changed_paths": [edit.path],
                            "updated_anchors": edit.updated_anchors
                        }),
                    ),
                    false,
                ),
                Err(error) => scratch_error_response(id, error),
            }
        }
        (Err(response), _, _) | (_, Err(response), _) | (_, _, Err(response)) => {
            (response.with_id(id), false)
        }
    }
}

pub(crate) fn scratch_capture_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let base_state = required_state_id(params, "base_state");
    let base_tree = required_str(params, "base_tree");
    let target_tree = required_str(params, "target_tree");
    match (base_state, base_tree, target_tree) {
        (Ok(base_state), Ok(base_tree), Ok(target_tree)) => {
            match engine.capture_tree(base_state, base_tree, target_tree) {
                Ok(capture) => (
                    WireResponse::ok(
                        id,
                        json!({
                            "scratch": capture.scratch,
                            "base_state": capture.base_state,
                            "base_tree": capture.base_tree,
                            "target_tree": capture.target_tree,
                            "changed_paths": capture.changed_paths
                        }),
                    ),
                    false,
                ),
                Err(error) => scratch_error_response(id, error),
            }
        }
        (Err(response), _, _) | (_, Err(response), _) | (_, _, Err(response)) => {
            (response.with_id(id), false)
        }
    }
}

pub(crate) fn scratch_diff_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let from = required_str(params, "from").map(ScratchId::new);
    let to = required_str(params, "to").map(ScratchId::new);
    match (from, to) {
        (Ok(from), Ok(to)) => match engine.diff(&from, &to) {
            Ok(diff) => (
                WireResponse::ok(
                    id,
                    json!({"from": diff.from, "to": diff.to, "changed_paths": diff.changed_paths}),
                ),
                false,
            ),
            Err(error) => scratch_error_response(id, error),
        },
        (Err(response), _) | (_, Err(response)) => (response.with_id(id), false),
    }
}

pub(crate) fn tree_list_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let scratch = required_str(params, "scratch").map(ScratchId::new);
    let path = optional_str(params, "path").map(|value| value.map(ToString::to_string));
    let glob = optional_str(params, "glob").map(|value| value.map(ToString::to_string));
    let limit = optional_usize(params, "limit");
    match (scratch, path, glob, limit) {
        (Ok(scratch), Ok(path), Ok(glob), Ok(limit)) => {
            let result = (|| -> graft_scratch::Result<Value> {
                let snapshot = engine.tree_snapshot(&scratch)?;
                let result = engine
                    .store()
                    .tree_list(&snapshot, &TreeListOptions { path, glob, limit })?;
                Ok(json_with_scratch_source(
                    &scratch,
                    "list",
                    serde_json::to_value(result).map_err(graft_store::StoreError::Json)?,
                ))
            })();
            match result {
                Ok(result) => (WireResponse::ok(id, result), false),
                Err(error) => scratch_error_response(id, error),
            }
        }
        (Err(response), _, _, _)
        | (_, Err(response), _, _)
        | (_, _, Err(response), _)
        | (_, _, _, Err(response)) => (response.with_id(id), false),
    }
}

pub(crate) fn tree_grep_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let scratch = required_str(params, "scratch").map(ScratchId::new);
    let pattern = required_str(params, "pattern").map(ToString::to_string);
    let path = optional_str(params, "path").map(|value| value.map(ToString::to_string));
    let glob = optional_str(params, "glob").map(|value| value.map(ToString::to_string));
    let limit = optional_usize(params, "limit");
    match (scratch, pattern, path, glob, limit) {
        (Ok(scratch), Ok(pattern), Ok(path), Ok(glob), Ok(limit)) => {
            let result = (|| -> graft_scratch::Result<Value> {
                let snapshot = engine.tree_snapshot(&scratch)?;
                let result = engine.store().tree_grep(
                    &snapshot,
                    &TreeGrepOptions {
                        pattern,
                        path,
                        glob,
                        limit,
                    },
                )?;
                Ok(json_with_scratch_source(
                    &scratch,
                    "grep",
                    serde_json::to_value(result).map_err(graft_store::StoreError::Json)?,
                ))
            })();
            match result {
                Ok(result) => (WireResponse::ok(id, result), false),
                Err(error) => scratch_error_response(id, error),
            }
        }
        (Err(response), _, _, _, _)
        | (_, Err(response), _, _, _)
        | (_, _, Err(response), _, _)
        | (_, _, _, Err(response), _)
        | (_, _, _, _, Err(response)) => (response.with_id(id), false),
    }
}

pub(crate) fn tree_metadata_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let scratch = required_str(params, "scratch").map(ScratchId::new);
    let path = required_str(params, "path");
    match (scratch, path) {
        (Ok(scratch), Ok(path)) => {
            let result = (|| -> graft_scratch::Result<Value> {
                let snapshot = engine.tree_snapshot(&scratch)?;
                let result = engine.store().tree_metadata(&snapshot, path)?;
                Ok(json_with_scratch_source(
                    &scratch,
                    "metadata",
                    serde_json::to_value(result).map_err(graft_store::StoreError::Json)?,
                ))
            })();
            match result {
                Ok(result) => (WireResponse::ok(id, result), false),
                Err(error) => scratch_error_response(id, error),
            }
        }
        (Err(response), _) | (_, Err(response)) => (response.with_id(id), false),
    }
}

pub(crate) fn scratch_drop_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    match required_str(params, "scratch").map(ScratchId::new) {
        Ok(scratch) => match engine.drop_scratch(&scratch) {
            Ok(dropped) => (
                WireResponse::ok(id, json!({"scratch": scratch, "dropped": dropped})),
                false,
            ),
            Err(error) => scratch_error_response(id, error),
        },
        Err(response) => (response.with_id(id), false),
    }
}

pub(crate) fn scratch_pin_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    match required_str(params, "scratch").map(ScratchId::new) {
        Ok(scratch) => match engine.pin(&scratch) {
            Ok(pin) => (
                WireResponse::ok(
                    id,
                    json!({"scratch": pin.scratch, "lease": pin.lease, "pinned": pin.pinned}),
                ),
                false,
            ),
            Err(error) => scratch_error_response(id, error),
        },
        Err(response) => (response.with_id(id), false),
    }
}

pub(crate) fn scratch_unpin_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    match required_str(params, "lease") {
        Ok(lease) => match engine.unpin(lease) {
            Ok(pin) => (
                WireResponse::ok(
                    id,
                    json!({"scratch": pin.scratch, "lease": pin.lease, "pinned": pin.pinned}),
                ),
                false,
            ),
            Err(error) => scratch_error_response(id, error),
        },
        Err(response) => (response.with_id(id), false),
    }
}

pub(crate) fn scratch_error_response(id: String, error: ScratchError) -> (WireResponse, bool) {
    let response = match error {
        ScratchError::UnknownScratch(scratch) => WireResponse::error(
            id,
            "E_UNKNOWN_SCRATCH",
            format!("scratch not found: {scratch}"),
        ),
        ScratchError::BinaryFile { path } => WireResponse::error(
            id,
            "E_BINARY_FILE",
            format!("path is not UTF-8 text: {path}"),
        ),
        ScratchError::StaleAnchor { fresh_anchors, .. } => WireResponse::error_with_retry(
            id,
            "E_STALE_ANCHOR",
            "stale hashline anchor",
            json!({"fresh_anchors": fresh_anchors}),
        ),
        ScratchError::AmbiguousText { matches } => WireResponse::error(
            id,
            "E_AMBIGUOUS_TEXT",
            format!("replace_text matched {matches} occurrences"),
        ),
        ScratchError::InvalidPatch(message) => WireResponse::error(id, "E_INVALID_PATCH", message),
        ScratchError::LineOutOfRange(line) => {
            WireResponse::error(id, "E_STALE_ANCHOR", format!("line out of range: {line}"))
        }
        ScratchError::ScratchPinned(scratch) => WireResponse::error(
            id,
            "E_SCRATCH_PINNED",
            format!("scratch is pinned: {scratch}"),
        ),
        ScratchError::ScratchLost(scratch) => WireResponse::error(
            id,
            "E_SCRATCH_LOST",
            format!("scratch state was lost; retry from --base: {scratch}"),
        ),
        ScratchError::EmptyChange => WireResponse::error(
            id,
            "E_EMPTY_CHANGE",
            "scratch has no changes to turn into a candidate".to_string(),
        ),
        ScratchError::UnknownLease(lease) => {
            WireResponse::error(id, "E_UNKNOWN_LEASE", format!("unknown lease: {lease}"))
        }
        ScratchError::Store(error) => WireResponse::error(id, "E_STORE", error.to_string()),
        ScratchError::Core(error) => WireResponse::error(id, "E_INTERNAL", error.to_string()),
    };
    (response, false)
}

enum ScratchSourceParam {
    Base(VirtualBaseRef),
    Materialized {
        base_state: StateId,
        tree_id: String,
    },
    From(ScratchId),
}

fn scratch_source(params: &Value) -> HandlerResult<ScratchSourceParam> {
    let base = optional_str(params, "base")?;
    let from = optional_str(params, "from")?;
    let materialized = materialized_base(params)?;
    match (base, from, materialized) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            Err(Box::new(WireResponse::error(
                "",
                "E_BAD_PARAMS",
                "provide exactly one of base, from, or resolved base_state/base_tree",
            )))
        }
        (Some(base), None, None) => parse_base_ref(base).map(ScratchSourceParam::Base),
        (None, Some(from), None) => Ok(ScratchSourceParam::From(ScratchId::new(from))),
        (None, None, Some((base_state, tree_id))) => Ok(ScratchSourceParam::Materialized {
            base_state,
            tree_id,
        }),
        (None, None, None) => Err(missing_field("base or from")),
    }
}

fn materialized_base(params: &Value) -> HandlerResult<Option<(StateId, String)>> {
    match (params.get("base_state"), optional_str(params, "base_tree")?) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(Box::new(WireResponse::error(
            "",
            "E_BAD_PARAMS",
            "resolved scratch base requires both base_state and base_tree",
        ))),
        (Some(base_state), Some(tree_id)) => {
            let base_state =
                serde_json::from_value::<StateId>(base_state.clone()).map_err(|err| {
                    Box::new(WireResponse::error(
                        "",
                        "E_BAD_PARAMS",
                        format!("field base_state must be a StateId object: {err}"),
                    ))
                })?;
            Ok(Some((base_state, tree_id.to_string())))
        }
    }
}

struct ResolvedScratchSource {
    scratch: ScratchId,
    base_state: StateId,
    base_tree: String,
}

fn scratch_open_json(open: ScratchOpen) -> Value {
    json!({
        "scratch": open.scratch,
        "base_state": open.base_state,
        "base_tree": open.base_tree
    })
}

fn resolve_scratch_source(
    engine: &ScratchEngine,
    source: ScratchSourceParam,
) -> graft_scratch::Result<ResolvedScratchSource> {
    match source {
        ScratchSourceParam::Base(base) => {
            let open = engine.open_with_metadata(base)?;
            Ok(ResolvedScratchSource {
                scratch: open.scratch,
                base_state: open.base_state,
                base_tree: open.base_tree,
            })
        }
        ScratchSourceParam::Materialized {
            base_state,
            tree_id,
        } => {
            let open = engine.open_materialized_with_metadata(base_state, &tree_id)?;
            Ok(ResolvedScratchSource {
                scratch: open.scratch,
                base_state: open.base_state,
                base_tree: open.base_tree,
            })
        }
        ScratchSourceParam::From(scratch) => {
            let ScratchBaseMetadata {
                base_state,
                base_tree,
            } = engine.base_metadata(&scratch)?;
            Ok(ResolvedScratchSource {
                scratch,
                base_state,
                base_tree,
            })
        }
    }
}

fn parse_base_ref(value: &str) -> HandlerResult<VirtualBaseRef> {
    let spec = BaseRefSpec::parse(value)
        .map_err(|err| Box::new(WireResponse::error("", "E_INVALID_BASE", err.to_string())))?;
    match spec {
        BaseRefSpec::Empty => Ok(VirtualBaseRef::Empty),
        BaseRefSpec::GraftTree(id) => Ok(VirtualBaseRef::Tree(id)),
        BaseRefSpec::Candidate(id) => Ok(VirtualBaseRef::Candidate(id)),
        BaseRefSpec::Patch(id) => Ok(VirtualBaseRef::Patch(id)),
        BaseRefSpec::GitTreeish(_) | BaseRefSpec::Repo { .. } => {
            Err(Box::new(WireResponse::error(
                "",
                "E_INVALID_BASE",
                format!(
                    "graftd scratch operations only accept graft:empty, tree:/candidate:/patch: refs; got `{value}`. Resolve git or repo bases through the CLI first."
                ),
            )))
        }
    }
}

fn parse_read_mode(value: &str) -> HandlerResult<ReadMode> {
    match value {
        "bytes" => Ok(ReadMode::Bytes),
        "text" => Ok(ReadMode::Text),
        "hashlines" => Ok(ReadMode::Hashlines),
        _ => Err(Box::new(WireResponse::error(
            "",
            "E_BAD_PARAMS",
            format!("unsupported read mode: {value}"),
        ))),
    }
}

fn required_str<'a>(params: &'a Value, field: &str) -> HandlerResult<&'a str> {
    match params.get(field) {
        Some(value) => value
            .as_str()
            .ok_or_else(|| bad_field_type(field, "string")),
        None => Err(missing_field(field)),
    }
}

fn optional_str<'a>(params: &'a Value, field: &str) -> HandlerResult<Option<&'a str>> {
    params
        .get(field)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| bad_field_type(field, "string"))
        })
        .transpose()
}

fn optional_usize(params: &Value, field: &str) -> HandlerResult<Option<usize>> {
    let Some(value) = params.get(field) else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| bad_field_type(field, "non-negative integer"))?;
    usize::try_from(value).map(Some).map_err(|_| {
        bad_params_message(format!(
            "field `{field}` value {value} is too large for this platform"
        ))
    })
}

fn required_state_id(params: &Value, field: &str) -> HandlerResult<StateId> {
    let value = params.get(field).ok_or_else(|| missing_field(field))?;
    serde_json::from_value::<StateId>(value.clone()).map_err(bad_params)
}

fn json_with_scratch_source(scratch: &ScratchId, operation: &str, mut payload: Value) -> Value {
    let source = json!({"kind": "scratch", "scratch": scratch});
    let Some(object) = payload.as_object_mut() else {
        return json!({"source": source, "operation": operation, "data": payload});
    };
    object.insert("source".to_string(), source);
    object.insert("operation".to_string(), json!(operation));
    payload
}
