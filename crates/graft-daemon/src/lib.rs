pub mod cli;

use graft_client::{WireRequest, WireResponse, parse_frame};
use graft_core::{HashlineEdit, PropertyId, PropertyRef, ScratchId};
use graft_scratch::{ReadMode, ScratchEngine};
use graft_store::VirtualBaseRef;
use serde_json::{Value, json};

pub type Result<T> = graft_client::WireResult<T>;
type HandlerResult<T> = std::result::Result<T, Box<WireResponse>>;

pub fn handle_frame(engine: &ScratchEngine, line: &str) -> Result<(WireResponse, bool)> {
    let request = parse_frame(line)?;
    Ok(handle_request(engine, request))
}

pub fn handle_request(engine: &ScratchEngine, request: WireRequest) -> (WireResponse, bool) {
    let id = request.id.clone();
    match request.op.as_str() {
        "status" => (
            WireResponse::ok(id, json!({"status":"ok","daemon":"graftd"})),
            false,
        ),
        "shutdown" => (WireResponse::ok(id, json!({"shutdown":true})), true),
        "cli_exec" => {
            let argv = request
                .params
                .get("argv")
                .cloned()
                .ok_or_else(|| missing_field("argv"))
                .and_then(|value| serde_json::from_value::<Vec<String>>(value).map_err(bad_params));
            match argv {
                Ok(argv) => match graft_runtime::run_daemon_argv_to_value(argv) {
                    Ok(envelope) => (WireResponse::ok(id, envelope), false),
                    Err(error) => (
                        WireResponse::error(id, "E_CLI_EXEC", error.to_string()),
                        false,
                    ),
                },
                Err(response) => (response.with_id(id), false),
            }
        }
        "scratch_open" => match required_str(&request.params, "base").and_then(parse_base_ref) {
            Ok(base) => match engine.open(base) {
                Ok(scratch) => (WireResponse::ok(id, json!({"scratch":scratch})), false),
                Err(error) => scratch_error_response(id, error),
            },
            Err(response) => (response.with_id(id), false),
        },
        "scratch_read" => {
            let scratch = required_str(&request.params, "scratch").map(ScratchId::new);
            let path = required_str(&request.params, "path");
            let mode = optional_str(&request.params, "mode")
                .map(parse_read_mode)
                .unwrap_or(Ok(ReadMode::Hashlines));
            match (scratch, path, mode) {
                (Ok(scratch), Ok(path), Ok(mode)) => match engine.read(&scratch, path, mode) {
                    Ok(read) => (
                        WireResponse::ok(
                            id,
                            json!({
                                "scratch": read.scratch,
                                "path": read.path,
                                "file_view_hash": read.file_view_hash,
                                "content": read.content,
                                "bytes_len": read.bytes.len()
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
        "scratch_write" => {
            let scratch = required_str(&request.params, "scratch").map(ScratchId::new);
            let path = required_str(&request.params, "path");
            let content = required_str(&request.params, "content");
            match (scratch, path, content) {
                (Ok(scratch), Ok(path), Ok(content)) => {
                    match engine.write(&scratch, path, content.as_bytes()) {
                        Ok(write) => (
                            WireResponse::ok(
                                id,
                                json!({
                                    "parent": write.parent,
                                    "scratch": write.scratch,
                                    "path": write.path,
                                    "content_hash": write.content_hash,
                                    "size": write.size
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
        "scratch_edit" => {
            let scratch = required_str(&request.params, "scratch").map(ScratchId::new);
            let path = required_str(&request.params, "path");
            let edits = request
                .params
                .get("edits")
                .cloned()
                .ok_or_else(|| missing_field("edits"))
                .and_then(|value| {
                    serde_json::from_value::<Vec<HashlineEdit>>(value).map_err(bad_params)
                });
            match (scratch, path, edits) {
                (Ok(scratch), Ok(path), Ok(edits)) => match engine.edit(&scratch, path, edits) {
                    Ok(edit) => (
                        WireResponse::ok(
                            id,
                            json!({
                                "parent": edit.parent,
                                "scratch": edit.scratch,
                                "path": edit.path,
                                "updated_anchors": edit.updated_anchors
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
        "scratch_promote" => {
            let scratch = required_str(&request.params, "scratch").map(ScratchId::new);
            let expected = request
                .params
                .get("expected")
                .cloned()
                .map(expected_properties)
                .unwrap_or_else(|| Ok(Vec::new()));
            let producer = optional_str(&request.params, "producer").unwrap_or("graftd");
            let message = optional_str(&request.params, "message").map(ToString::to_string);
            match (scratch, expected) {
                (Ok(scratch), Ok(expected)) => {
                    match engine.promote(&scratch, expected, producer.to_string(), message) {
                        Ok(promotion) => (
                            WireResponse::ok(
                                id,
                                json!({
                                    "scratch": promotion.scratch,
                                    "candidate": promotion.candidate,
                                    "changed_paths": promotion.changed_paths,
                                    "registry_changed": false,
                                    "git_changed": false
                                }),
                            ),
                            false,
                        ),
                        Err(error) => scratch_error_response(id, error),
                    }
                }
                (Err(response), _) | (_, Err(response)) => (response.with_id(id), false),
            }
        }
        "scratch_diff" => {
            let from = required_str(&request.params, "from").map(ScratchId::new);
            let to = required_str(&request.params, "to").map(ScratchId::new);
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
        "scratch_drop" => match required_str(&request.params, "scratch").map(ScratchId::new) {
            Ok(scratch) => match engine.drop_scratch(&scratch) {
                Ok(dropped) => (
                    WireResponse::ok(id, json!({"scratch": scratch, "dropped": dropped})),
                    false,
                ),
                Err(error) => scratch_error_response(id, error),
            },
            Err(response) => (response.with_id(id), false),
        },
        "scratch_pin" => match required_str(&request.params, "scratch").map(ScratchId::new) {
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
        },
        "scratch_unpin" => match required_str(&request.params, "lease") {
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
        },
        _ => (
            WireResponse::error(id, "E_UNKNOWN_OP", format!("unknown op {}", request.op)),
            false,
        ),
    }
}

trait WithId {
    fn with_id(self, id: String) -> WireResponse;
}

impl WithId for WireResponse {
    fn with_id(mut self, id: String) -> WireResponse {
        self.id = id;
        self
    }
}

impl WithId for Box<WireResponse> {
    fn with_id(mut self, id: String) -> WireResponse {
        self.id = id;
        *self
    }
}

fn scratch_error_response(id: String, error: graft_scratch::ScratchError) -> (WireResponse, bool) {
    use graft_scratch::ScratchError;
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
            format!("scratch state was lost; reopen from base: {scratch}"),
        ),
        ScratchError::EmptyChange => WireResponse::error(
            id,
            "E_EMPTY_CHANGE",
            "scratch has no changes to promote".to_string(),
        ),
        ScratchError::UnknownLease(lease) => {
            WireResponse::error(id, "E_UNKNOWN_LEASE", format!("unknown lease: {lease}"))
        }
        ScratchError::Store(error) => WireResponse::error(id, "E_STORE", error.to_string()),
        ScratchError::Core(error) => WireResponse::error(id, "E_INTERNAL", error.to_string()),
    };
    (response, false)
}

fn parse_base_ref(value: &str) -> HandlerResult<VirtualBaseRef> {
    use graft_core::BaseRefSpec;
    let spec = BaseRefSpec::parse(value)
        .map_err(|err| Box::new(WireResponse::error("", "E_INVALID_BASE", err.to_string())))?;
    match spec {
        BaseRefSpec::GraftTree(id) => Ok(VirtualBaseRef::Tree(id)),
        BaseRefSpec::Candidate(id) => Ok(VirtualBaseRef::Candidate(id)),
        BaseRefSpec::Patch(id) => Ok(VirtualBaseRef::Patch(id)),
        BaseRefSpec::GitTreeish(_) | BaseRefSpec::Repo { .. } | BaseRefSpec::Empty => {
            Err(Box::new(WireResponse::error(
                "",
                "E_INVALID_BASE",
                format!(
                    "graftd scratch operations only accept tree:/candidate:/patch: refs; got `{value}`. Resolve git or repo bases through the CLI first."
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

fn expected_properties(value: Value) -> HandlerResult<Vec<PropertyRef>> {
    let names = serde_json::from_value::<Vec<String>>(value).map_err(bad_params)?;
    Ok(names
        .into_iter()
        .map(|name| PropertyRef::new(PropertyId::new(format!("property:{name}")), name))
        .collect())
}

fn required_str<'a>(params: &'a Value, field: &str) -> HandlerResult<&'a str> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| missing_field(field))
}

fn optional_str<'a>(params: &'a Value, field: &str) -> Option<&'a str> {
    params.get(field).and_then(Value::as_str)
}

fn missing_field(field: &str) -> Box<WireResponse> {
    Box::new(WireResponse::error(
        "",
        "E_MISSING_FIELD",
        format!("missing field {field}"),
    ))
}

fn bad_params(error: serde_json::Error) -> Box<WireResponse> {
    Box::new(WireResponse::error("", "E_BAD_PARAMS", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_client::encode_response;
    use graft_core::{TreeEntry, TreeSnapshot};
    use graft_store::GraftStore;

    fn seeded_engine(name: &str) -> (std::path::PathBuf, ScratchEngine, String) {
        let dir = std::env::temp_dir().join(format!("graftd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let hash = store.write_blob(b"hello\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "hello.txt".to_string(),
            hash,
            size: 6,
        }]);
        let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
        (dir, ScratchEngine::new(store), tree_id)
    }

    #[test]
    fn parses_and_encodes_ndjson_frame() {
        let request = parse_frame(r#"{"id":"1","op":"status","params":{}}"#).unwrap();
        assert_eq!(request.id, "1");
        assert_eq!(request.op, "status");
        let encoded = encode_response(&WireResponse::ok("1", json!({"status":"ok"}))).unwrap();
        assert!(encoded.ends_with('\n'));
    }

    #[test]
    fn handles_open_read_write_promote_flow() {
        let (dir, engine, tree_id) = seeded_engine("flow");
        let (open, _) = handle_request(
            &engine,
            WireRequest {
                id: "open".to_string(),
                op: "scratch_open".to_string(),
                params: json!({"base": tree_id}),
            },
        );
        assert!(open.ok);
        let scratch = open.result.unwrap()["scratch"]
            .as_str()
            .unwrap()
            .to_string();

        let (read, _) = handle_request(
            &engine,
            WireRequest {
                id: "read".to_string(),
                op: "scratch_read".to_string(),
                params: json!({"scratch": scratch, "path":"hello.txt", "mode":"hashlines"}),
            },
        );
        assert!(read.ok);
        assert!(
            read.result.as_ref().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("hello")
        );

        let (write, _) = handle_request(
            &engine,
            WireRequest {
                id: "write".to_string(),
                op: "scratch_write".to_string(),
                params: json!({"scratch": scratch, "path":"bye.txt", "content":"bye\n"}),
            },
        );
        assert!(write.ok);
        let written = write.result.unwrap()["scratch"]
            .as_str()
            .unwrap()
            .to_string();

        let (promote, _) = handle_request(
            &engine,
            WireRequest {
                id: "promote".to_string(),
                op: "scratch_promote".to_string(),
                params: json!({"scratch": written, "expected":["ValidPatch"], "producer":"test"}),
            },
        );
        assert!(promote.ok);
        assert!(
            promote.result.unwrap()["candidate"]
                .as_str()
                .unwrap()
                .starts_with("candidate:")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn handles_diff_pin_unpin_and_drop() {
        let (dir, engine, tree_id) = seeded_engine("lifecycle");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let write = engine.write(&root, "bye.txt", b"bye\n").unwrap();

        let (diff, _) = handle_request(
            &engine,
            WireRequest {
                id: "diff".to_string(),
                op: "scratch_diff".to_string(),
                params: json!({"from": root, "to": write.scratch}),
            },
        );
        assert!(diff.ok);
        assert!(
            diff.result.as_ref().unwrap()["changed_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path.as_str() == Some("bye.txt"))
        );

        let scratch = write.scratch.to_string();
        let (pin, _) = handle_request(
            &engine,
            WireRequest {
                id: "pin".to_string(),
                op: "scratch_pin".to_string(),
                params: json!({"scratch": scratch}),
            },
        );
        assert!(pin.ok);
        let lease = pin.result.as_ref().unwrap()["lease"]
            .as_str()
            .unwrap()
            .to_string();

        let (drop_pinned, _) = handle_request(
            &engine,
            WireRequest {
                id: "drop".to_string(),
                op: "scratch_drop".to_string(),
                params: json!({"scratch": scratch}),
            },
        );
        assert!(!drop_pinned.ok);
        assert_eq!(drop_pinned.error.unwrap().code, "E_SCRATCH_PINNED");

        let (unpin, _) = handle_request(
            &engine,
            WireRequest {
                id: "unpin".to_string(),
                op: "scratch_unpin".to_string(),
                params: json!({"lease": lease}),
            },
        );
        assert!(unpin.ok);

        let (drop_free, _) = handle_request(
            &engine,
            WireRequest {
                id: "drop2".to_string(),
                op: "scratch_drop".to_string(),
                params: json!({"scratch": scratch}),
            },
        );
        assert!(drop_free.ok);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn maps_stale_anchor_to_structured_error() {
        let (dir, engine, tree_id) = seeded_engine("stale");
        let scratch = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "edit".to_string(),
                op: "scratch_edit".to_string(),
                params: json!({
                    "scratch": scratch,
                    "path":"hello.txt",
                    "edits":[{"kind":"replace_line","line":1,"hash":"ZZ","old":"hello","new":"hi"}]
                }),
            },
        );
        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_STALE_ANCHOR");
        assert!(
            error.retry.unwrap()["fresh_anchors"]
                .as_str()
                .unwrap()
                .contains(">>>")
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
