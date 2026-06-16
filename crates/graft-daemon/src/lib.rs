pub mod cli;
mod scratch_wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use graft_client::{WireRequest, WireResponse, parse_frame};
use graft_core::{Constraint, ScratchId};
use graft_scratch::ScratchEngine;
use graft_store::{GraftStore, RegistryStore, normalize_workspace_path};
use serde_json::{Value, json};

pub(crate) type Result<T> = graft_client::WireResult<T>;
type HandlerResult<T> = std::result::Result<T, Box<WireResponse>>;
type OpHandler = fn(&ScratchEngine, String, &Value) -> (WireResponse, bool);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ParamField {
    name: &'static str,
    required: bool,
}

const fn required(name: &'static str) -> ParamField {
    ParamField {
        name,
        required: true,
    }
}

const fn optional(name: &'static str) -> ParamField {
    ParamField {
        name,
        required: false,
    }
}

const ROUTING_FIELDS: &[ParamField] = &[required("workspace_id"), required("workspace_root")];
const NO_FIELDS: &[ParamField] = &[];
const CLI_EXEC_FIELDS: &[ParamField] = &[required("argv")];
const WORKSPACE_ATTACH_FIELDS: &[ParamField] = &[required("cwd"), optional("workspace")];
const WORKSPACE_DETACH_FIELDS: &[ParamField] = &[required("cwd")];
const SCRATCH_OPEN_FIELDS: &[ParamField] = &[required("base")];
const SCRATCH_READ_FIELDS: &[ParamField] = &[
    optional("base"),
    optional("base_state"),
    optional("base_tree"),
    optional("from"),
    required("path"),
    optional("mode"),
];
const SCRATCH_WRITE_FIELDS: &[ParamField] = &[
    optional("base"),
    optional("base_state"),
    optional("base_tree"),
    optional("from"),
    required("path"),
    required("content"),
];
const SCRATCH_DELETE_FIELDS: &[ParamField] = &[
    optional("base"),
    optional("base_state"),
    optional("base_tree"),
    optional("from"),
    required("path"),
];
const SCRATCH_EDIT_FIELDS: &[ParamField] = &[
    optional("base"),
    optional("base_state"),
    optional("base_tree"),
    optional("from"),
    required("path"),
    required("edits"),
];
const SCRATCH_CAPTURE_FIELDS: &[ParamField] = &[
    required("base_state"),
    required("base_tree"),
    required("target_tree"),
];
const CANDIDATE_FROM_SCRATCH_FIELDS: &[ParamField] = &[
    required("scratch"),
    optional("constraint"),
    optional("producer"),
    optional("message"),
];
const SCRATCH_DIFF_FIELDS: &[ParamField] = &[required("from"), required("to")];
const SCRATCH_HANDLE_FIELDS: &[ParamField] = &[required("scratch")];
const SCRATCH_UNPIN_FIELDS: &[ParamField] = &[required("lease")];

#[derive(Debug)]
pub(crate) struct DaemonState {
    default_workspace_root: PathBuf,
    registry: RegistryStore,
    engines: Mutex<HashMap<PathBuf, Arc<ScratchEngine>>>,
}

impl DaemonState {
    pub(crate) fn new(default_store: GraftStore) -> Self {
        Self::new_with_registry(default_store, RegistryStore::from_env())
    }

    pub(crate) fn new_with_registry(default_store: GraftStore, registry: RegistryStore) -> Self {
        let default_workspace_root = normalize_workspace_path(default_store.paths().workspace());
        let default_engine = Arc::new(ScratchEngine::new(GraftStore::open(
            &default_workspace_root,
        )));
        let mut engines = HashMap::new();
        engines.insert(default_workspace_root.clone(), default_engine);
        Self {
            default_workspace_root,
            registry,
            engines: Mutex::new(engines),
        }
    }

    fn engine_for_workspace_root(&self, root: &Path) -> HandlerResult<Arc<ScratchEngine>> {
        let root = normalize_workspace_path(root);
        let mut engines = self
            .engines
            .lock()
            .expect("daemon workspace engine map poisoned");
        if let Some(engine) = engines.get(&root) {
            return Ok(engine.clone());
        }
        let store = GraftStore::open(&root);
        if !store.is_initialized() {
            return Err(Box::new(WireResponse::error(
                "",
                "E_NO_CONFIG",
                format!("workspace {} is not initialized", root.display()),
            )));
        }
        if let Err(error) = store.init_storage() {
            return Err(Box::new(WireResponse::error(
                "",
                "E_STORE",
                format!(
                    "graftd cannot initialize .graft storage at {}: {error}",
                    store.paths().root().display()
                ),
            )));
        }
        let engine = Arc::new(ScratchEngine::new(store));
        engines.insert(root, engine.clone());
        Ok(engine)
    }

    fn engine_for_request(&self, params: &Value) -> HandlerResult<Arc<ScratchEngine>> {
        let workspace_id = required_non_blank_str(params, "workspace_id")?;
        let workspace = self
            .registry
            .get_workspace(workspace_id)
            .map_err(|error| {
                Box::new(WireResponse::error(
                    "",
                    "E_REGISTRY",
                    format!("cannot resolve workspace {workspace_id}: {error}"),
                ))
            })?
            .ok_or_else(|| {
                Box::new(WireResponse::error(
                    "",
                    "E_UNKNOWN_WORKSPACE",
                    format!("workspace {workspace_id} is not registered"),
                ))
            })?;
        let root = required_non_blank_str(params, "workspace_root")?;
        let requested = normalize_workspace_path(Path::new(root));
        let registered = normalize_workspace_path(&workspace.root);
        if requested != registered {
            return Err(Box::new(WireResponse::error(
                "",
                "E_WORKSPACE_ROUTE_MISMATCH",
                format!(
                    "workspace {workspace_id} is registered at {}, but request targeted {}",
                    registered.display(),
                    requested.display()
                ),
            )));
        }
        self.engine_for_workspace_root(&workspace.root)
    }

    fn default_engine(&self) -> Arc<ScratchEngine> {
        self.engine_for_workspace_root(&self.default_workspace_root)
            .expect("default workspace engine must exist")
    }
}

pub(crate) fn handle_frame(state: &DaemonState, line: &str) -> Result<(WireResponse, bool)> {
    let request = parse_frame(line)?;
    Ok(handle_routed_request(state, request))
}

pub(crate) fn handle_routed_request(
    state: &DaemonState,
    request: WireRequest,
) -> (WireResponse, bool) {
    let Some(spec) = op_spec(request.op.as_str()) else {
        let WireRequest { id, op, .. } = request;
        return (unknown_op_response(id, op.as_str()), false);
    };
    match spec.scope {
        OpScope::Global => handle_request_with_spec(&state.default_engine(), request, spec),
        OpScope::Routed => {
            let id = request.id.clone();
            if let Err(response) = ensure_allowed_fields_for_spec(&request.params, spec) {
                return (response.with_id(id), false);
            }
            match state.engine_for_request(&request.params) {
                Ok(engine) => handle_request_with_spec(&engine, request, spec),
                Err(response) => (response.with_id(id), false),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpScope {
    Global,
    Routed,
}

#[derive(Clone, Copy, Debug)]
struct OpSpec {
    op: &'static str,
    scope: OpScope,
    routing_fields: &'static [ParamField],
    fields: &'static [ParamField],
    handler: OpHandler,
}

const OP_SPECS: &[OpSpec] = &[
    OpSpec {
        op: "status",
        scope: OpScope::Global,
        routing_fields: NO_FIELDS,
        fields: NO_FIELDS,
        handler: status_response,
    },
    OpSpec {
        op: "shutdown",
        scope: OpScope::Global,
        routing_fields: NO_FIELDS,
        fields: NO_FIELDS,
        handler: shutdown_response,
    },
    OpSpec {
        op: "cli_exec",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: CLI_EXEC_FIELDS,
        handler: cli_exec_response,
    },
    OpSpec {
        op: "workspace_attach",
        scope: OpScope::Global,
        routing_fields: NO_FIELDS,
        fields: WORKSPACE_ATTACH_FIELDS,
        handler: workspace_attach_response,
    },
    OpSpec {
        op: "workspace_detach",
        scope: OpScope::Global,
        routing_fields: NO_FIELDS,
        fields: WORKSPACE_DETACH_FIELDS,
        handler: workspace_detach_response,
    },
    OpSpec {
        op: "scratch_open",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_OPEN_FIELDS,
        handler: scratch_wire::scratch_open_response,
    },
    OpSpec {
        op: "scratch_read",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_READ_FIELDS,
        handler: scratch_wire::scratch_read_response,
    },
    OpSpec {
        op: "scratch_write",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_WRITE_FIELDS,
        handler: scratch_wire::scratch_write_response,
    },
    OpSpec {
        op: "scratch_delete",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_DELETE_FIELDS,
        handler: scratch_wire::scratch_delete_response,
    },
    OpSpec {
        op: "scratch_edit",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_EDIT_FIELDS,
        handler: scratch_wire::scratch_edit_response,
    },
    OpSpec {
        op: "scratch_capture",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_CAPTURE_FIELDS,
        handler: scratch_wire::scratch_capture_response,
    },
    OpSpec {
        op: "candidate_from_scratch",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: CANDIDATE_FROM_SCRATCH_FIELDS,
        handler: candidate_from_scratch_wire_response,
    },
    OpSpec {
        op: "scratch_diff",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_DIFF_FIELDS,
        handler: scratch_wire::scratch_diff_response,
    },
    OpSpec {
        op: "scratch_drop",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_HANDLE_FIELDS,
        handler: scratch_wire::scratch_drop_response,
    },
    OpSpec {
        op: "scratch_pin",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_HANDLE_FIELDS,
        handler: scratch_wire::scratch_pin_response,
    },
    OpSpec {
        op: "scratch_unpin",
        scope: OpScope::Routed,
        routing_fields: ROUTING_FIELDS,
        fields: SCRATCH_UNPIN_FIELDS,
        handler: scratch_wire::scratch_unpin_response,
    },
];

fn op_spec(op: &str) -> Option<OpSpec> {
    OP_SPECS.iter().copied().find(|spec| spec.op == op)
}

#[cfg(test)]
fn handle_request(engine: &ScratchEngine, request: WireRequest) -> (WireResponse, bool) {
    let Some(spec) = op_spec(request.op.as_str()) else {
        let WireRequest { id, op, .. } = request;
        return (unknown_op_response(id, op.as_str()), false);
    };
    handle_request_with_spec(engine, request, spec)
}

fn handle_request_with_spec(
    engine: &ScratchEngine,
    request: WireRequest,
    spec: OpSpec,
) -> (WireResponse, bool) {
    let id = request.id;
    if let Err(response) = ensure_fields_for_spec(&request.params, spec) {
        return (response.with_id(id), false);
    }
    (spec.handler)(engine, id, &request.params)
}

fn status_response(_engine: &ScratchEngine, id: String, _params: &Value) -> (WireResponse, bool) {
    (
        WireResponse::ok(id, json!({"status":"ok","daemon":"graftd"})),
        false,
    )
}

fn shutdown_response(_engine: &ScratchEngine, id: String, _params: &Value) -> (WireResponse, bool) {
    (WireResponse::ok(id, json!({"shutdown":true})), true)
}

fn cli_exec_response(_engine: &ScratchEngine, id: String, params: &Value) -> (WireResponse, bool) {
    let argv = params
        .get("argv")
        .cloned()
        .ok_or_else(|| missing_field("argv"))
        .and_then(|value| serde_json::from_value::<Vec<String>>(value).map_err(bad_params));
    let workspace_id = required_non_blank_str(params, "workspace_id");
    match (argv, workspace_id) {
        (Ok(argv), Ok(workspace_id)) => {
            match graft_runtime::run_daemon_argv_to_value_for_workspace(argv, workspace_id) {
                Ok(envelope) => (WireResponse::ok(id, envelope), false),
                Err(error) => (
                    WireResponse::error(id, "E_CLI_EXEC", error.to_string()),
                    false,
                ),
            }
        }
        (Err(response), _) | (_, Err(response)) => (response.with_id(id), false),
    }
}

fn workspace_attach_response(
    _engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let cwd = required_non_blank_str(params, "cwd");
    let workspace = optional_non_blank_str(params, "workspace");
    match (cwd, workspace) {
        (Ok(cwd), Ok(workspace)) => {
            match graft_runtime::workspace_attach_to_value(Path::new(cwd), workspace, false) {
                Ok(envelope) => (WireResponse::ok(id, envelope), false),
                Err(error) => (
                    WireResponse::error(id, "E_WORKSPACE_ATTACH", error.to_string()),
                    false,
                ),
            }
        }
        (Err(response), _) | (_, Err(response)) => (response.with_id(id), false),
    }
}

fn workspace_detach_response(
    _engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let cwd = required_non_blank_str(params, "cwd");
    match cwd {
        Ok(cwd) => match graft_runtime::workspace_detach_to_value(Path::new(cwd)) {
            Ok(envelope) => (WireResponse::ok(id, envelope), false),
            Err(error) => (
                WireResponse::error(id, "E_WORKSPACE_DETACH", error.to_string()),
                false,
            ),
        },
        Err(response) => (response.with_id(id), false),
    }
}

fn unknown_op_response(id: String, op: &str) -> WireResponse {
    WireResponse::error(id, "E_UNKNOWN_OP", format!("unknown op {op}"))
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

fn candidate_from_scratch_wire_response(
    engine: &ScratchEngine,
    id: String,
    params: &Value,
) -> (WireResponse, bool) {
    let scratch = required_str(params, "scratch").map(ScratchId::new);
    let constraint = constraint_requirement(engine, params);
    let producer =
        optional_non_blank_str(params, "producer").map(|value| value.unwrap_or("graftd"));
    let message =
        optional_non_blank_str(params, "message").map(|value| value.map(ToString::to_string));
    match (scratch, constraint, producer, message) {
        (Ok(scratch), Ok(constraint), Ok(producer), Ok(message)) => {
            match engine.candidate_from_scratch(&scratch, constraint, producer.to_string(), message)
            {
                Ok(result) => (
                    WireResponse::ok(
                        id,
                        json!({
                            "scratch": result.scratch,
                            "candidate": result.candidate,
                            "changed_paths": result.changed_paths,
                            "registry_changed": false,
                            "git_changed": false
                        }),
                    ),
                    false,
                ),
                Err(error) => scratch_wire::scratch_error_response(id, error),
            }
        }
        (Err(response), _, _, _)
        | (_, Err(response), _, _)
        | (_, _, Err(response), _)
        | (_, _, _, Err(response)) => (response.with_id(id), false),
    }
}

fn ensure_fields_for_spec(params: &Value, spec: OpSpec) -> HandlerResult<()> {
    let object = ensure_allowed_fields_for_spec(params, spec)?;
    for field in spec.fields.iter().filter(|field| field.required) {
        if !object.contains_key(field.name) {
            return Err(missing_field(field.name));
        }
    }
    Ok(())
}

fn ensure_allowed_fields_for_spec(
    params: &Value,
    spec: OpSpec,
) -> HandlerResult<&serde_json::Map<String, Value>> {
    let Some(object) = params.as_object() else {
        return Err(bad_params_message("params must be a JSON object"));
    };
    for field in object.keys() {
        if !op_field_allowed(spec, field) {
            return Err(bad_params_message(format!(
                "unknown field {field} for daemon op"
            )));
        }
    }
    Ok(object)
}

fn op_field_allowed(spec: OpSpec, field: &str) -> bool {
    spec.fields.iter().any(|allowed| allowed.name == field)
        || spec.routing_fields.iter().any(|route| route.name == field)
}

fn constraint_requirement(engine: &ScratchEngine, params: &Value) -> HandlerResult<Constraint> {
    let names = params
        .get("constraint")
        .cloned()
        .map(|value| serde_json::from_value::<Vec<String>>(value).map_err(bad_params))
        .unwrap_or_else(|| Ok(Vec::new()))?;
    graft_runtime::resolve_candidate_constraint_requirement(engine.store(), &names)
        .map_err(|error| Box::new(WireResponse::error("", "E_BAD_PARAMS", error.to_string())))
}

fn required_str<'a>(params: &'a Value, field: &str) -> HandlerResult<&'a str> {
    match params.get(field) {
        Some(value) => value
            .as_str()
            .ok_or_else(|| bad_field_type(field, "string")),
        None => Err(missing_field(field)),
    }
}

fn required_non_blank_str<'a>(params: &'a Value, field: &str) -> HandlerResult<&'a str> {
    let value = required_str(params, field)?;
    if value.trim().is_empty() {
        return Err(bad_params_message(format!(
            "field {field} must not be empty"
        )));
    }
    Ok(value)
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

fn optional_non_blank_str<'a>(params: &'a Value, field: &str) -> HandlerResult<Option<&'a str>> {
    let value = optional_str(params, field)?;
    if let Some(value) = value
        && value.trim().is_empty()
    {
        return Err(bad_params_message(format!(
            "field {field} must not be empty"
        )));
    }
    Ok(value)
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

fn bad_params_message(message: impl Into<String>) -> Box<WireResponse> {
    Box::new(WireResponse::error("", "E_BAD_PARAMS", message))
}

fn bad_field_type(field: &str, expected: &str) -> Box<WireResponse> {
    Box::new(WireResponse::error(
        "",
        "E_BAD_PARAMS",
        format!("field {field} must be {expected}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_client::encode_response;
    use graft_core::{Constraint, TreeEntry, TreeSnapshot};
    use graft_store::{
        DEFAULT_WORKSPACE_ID, GraftStore, RegistryStore, VirtualBaseRef, WorkspaceKind,
    };
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let old = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(old) => unsafe { std::env::set_var(self.key, old) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    fn seeded_store(name: &str) -> (std::path::PathBuf, GraftStore, String) {
        let dir = std::env::temp_dir().join(format!("graftd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        std::fs::write(
            dir.join("graft.lock"),
            "# @generated by graft constraint lock; do not edit by hand\nversion = 1\n",
        )
        .unwrap();
        let hash = store.write_blob(b"hello\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "hello.txt".to_string(),
            hash,
            size: 6,
        }]);
        let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
        (dir, store, tree_id)
    }

    fn seeded_engine(name: &str) -> (std::path::PathBuf, ScratchEngine, String) {
        let (dir, store, tree_id) = seeded_store(name);
        (dir, ScratchEngine::new(store), tree_id)
    }

    fn schema_field_names(spec: OpSpec) -> Vec<&'static str> {
        spec.fields.iter().map(|field| field.name).collect()
    }

    fn minimal_schema_params(spec: OpSpec, tree_id: &str) -> Value {
        let mut params = serde_json::Map::new();
        for field in spec.fields.iter().filter(|field| field.required) {
            params.insert(
                field.name.to_string(),
                schema_value_for_field(field.name, tree_id),
            );
        }
        Value::Object(params)
    }

    fn schema_value_for_field(field: &str, tree_id: &str) -> Value {
        match field {
            "argv" => json!(["graft", "status"]),
            "base" => json!(tree_id),
            "base_state" => {
                serde_json::to_value(graft_core::StateId::GraftTree(tree_id.to_string())).unwrap()
            }
            "base_tree" => json!(tree_id),
            "content" => json!("hello\n"),
            "cwd" => json!("/tmp/graft-cwd"),
            "edits" => json!([]),
            "from" => json!("scratch:from"),
            "lease" => json!("lease:test"),
            "path" => json!("hello.txt"),
            "scratch" => json!("scratch:test"),
            "target_tree" => json!(tree_id),
            "to" => json!("scratch:to"),
            other => panic!("test helper has no schema value for field {other}"),
        }
    }

    fn assert_schema_error(response: WireResponse, code: &str, message: &str) {
        let error = response.error.expect("schema response must be an error");
        assert_eq!(error.code, code);
        assert_eq!(error.message, message);
    }

    fn assert_response_error(response: WireResponse, code: &str, message: &str) {
        assert!(!response.ok, "{response:?}");
        let error = response.error.expect("wire response must be an error");
        assert_eq!(error.code, code);
        assert_eq!(error.message, message);
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
    fn op_spec_table_is_the_wire_schema_source() {
        let mut seen = std::collections::BTreeSet::new();
        for spec in OP_SPECS {
            assert!(
                seen.insert(spec.op),
                "duplicate daemon op spec: {}",
                spec.op
            );
            assert_eq!(op_spec(spec.op).map(|found| found.op), Some(spec.op));
            match spec.scope {
                OpScope::Global => assert!(
                    spec.routing_fields.is_empty(),
                    "{} global op must not accept routing fields",
                    spec.op
                ),
                OpScope::Routed => assert_eq!(
                    spec.routing_fields, ROUTING_FIELDS,
                    "{} routed op must declare the workspace route schema",
                    spec.op
                ),
            }
            assert!(
                spec.fields.iter().all(|field| !spec
                    .routing_fields
                    .iter()
                    .any(|route| route.name == field.name)),
                "{} must not duplicate routing fields in op-local fields: {:?}",
                spec.op,
                schema_field_names(*spec)
            );
            let mut field_names = std::collections::BTreeSet::new();
            for field in spec.fields {
                assert!(
                    field_names.insert(field.name),
                    "{} declares field {} more than once",
                    spec.op,
                    field.name
                );
            }
        }
        assert_eq!(seen.len(), OP_SPECS.len());
        assert!(op_spec("not_a_real_op").is_none());
    }

    #[test]
    fn every_op_rejects_unknown_wire_fields_from_schema() {
        let (_dir, _engine, tree_id) = seeded_engine("schema_unknown_field");

        for spec in OP_SPECS {
            let mut params = minimal_schema_params(*spec, &tree_id);
            params
                .as_object_mut()
                .unwrap()
                .insert("unexpected".to_string(), json!(true));

            let error = ensure_fields_for_spec(&params, *spec).unwrap_err();
            assert_schema_error(
                *error,
                "E_BAD_PARAMS",
                "unknown field unexpected for daemon op",
            );
        }
    }

    #[test]
    fn every_required_op_field_is_enforced_by_schema() {
        let (_dir, _engine, tree_id) = seeded_engine("schema_required_fields");

        for spec in OP_SPECS {
            for field in spec.fields.iter().filter(|field| field.required) {
                let mut params = minimal_schema_params(*spec, &tree_id);
                params.as_object_mut().unwrap().remove(field.name);

                let error = ensure_fields_for_spec(&params, *spec).unwrap_err();
                assert_schema_error(
                    *error,
                    "E_MISSING_FIELD",
                    &format!("missing field {}", field.name),
                );
            }
        }
    }

    #[test]
    fn every_global_op_rejects_workspace_routing_fields_from_schema() {
        for spec in OP_SPECS
            .iter()
            .copied()
            .filter(|spec| spec.scope == OpScope::Global)
        {
            let error = ensure_fields_for_spec(&json!({"workspace_id": "ws:registered"}), spec)
                .unwrap_err();
            assert_schema_error(
                *error,
                "E_BAD_PARAMS",
                "unknown field workspace_id for daemon op",
            );
        }
    }

    #[test]
    fn every_routed_op_validates_workspace_route_before_handler_fields() {
        let (dir, _store, tree_id) = seeded_store("schema_routed_fields");
        let home = std::env::temp_dir().join(format!(
            "graftd-schema-routed-fields-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:registered", WorkspaceKind::Local, &dir)
            .unwrap();
        let state = DaemonState::new_with_registry(GraftStore::open(&dir), registry);

        for spec in OP_SPECS
            .iter()
            .copied()
            .filter(|spec| spec.scope == OpScope::Routed)
        {
            ensure_allowed_fields_for_spec(
                &json!({"workspace_id": "ws:registered", "workspace_root": dir}),
                spec,
            )
            .unwrap();

            let missing_id = minimal_schema_params(spec, &tree_id);
            let (response, _) = handle_routed_request(
                &state,
                WireRequest {
                    id: format!("{}-missing-id", spec.op),
                    op: spec.op.to_string(),
                    params: missing_id,
                },
            );
            assert_response_error(response, "E_MISSING_FIELD", "missing field workspace_id");

            let mut missing_root = minimal_schema_params(spec, &tree_id);
            missing_root
                .as_object_mut()
                .unwrap()
                .insert("workspace_id".to_string(), json!("ws:registered"));
            let (response, _) = handle_routed_request(
                &state,
                WireRequest {
                    id: format!("{}-missing-root", spec.op),
                    op: spec.op.to_string(),
                    params: missing_root,
                },
            );
            assert_response_error(response, "E_MISSING_FIELD", "missing field workspace_root");

            let (response, _) = handle_routed_request(
                &state,
                WireRequest {
                    id: format!("{}-unknown", spec.op),
                    op: spec.op.to_string(),
                    params: json!({"unexpected": true}),
                },
            );
            assert_response_error(
                response,
                "E_BAD_PARAMS",
                "unknown field unexpected for daemon op",
            );
        }

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn global_status_rejects_workspace_routing_fields() {
        let (dir, engine, _tree_id) = seeded_engine("status_rejects_routing");

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "status".to_string(),
                op: "status".to_string(),
                params: json!({
                    "workspace_id": "ws:registered"
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "unknown field workspace_id for daemon op");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workspace_attach_typed_op_writes_registry_route_only() {
        let _lock = env_lock();
        let home = std::env::temp_dir().join(format!(
            "graftd-workspace-attach-home-{}",
            std::process::id()
        ));
        let cwd = std::env::temp_dir().join(format!(
            "graftd-workspace-attach-cwd-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let _guard = EnvGuard::set("GRAFT_HOME", &home);
        let (engine_root, engine, _tree_id) = seeded_engine("workspace_attach_typed_op");

        let (attach, _) = handle_request(
            &engine,
            WireRequest {
                id: "attach".to_string(),
                op: "workspace_attach".to_string(),
                params: json!({"cwd": cwd}),
            },
        );

        assert!(attach.ok, "{attach:?}");
        let result = attach.result.unwrap();
        assert_eq!(result["registry_changed"].as_bool(), Some(true));
        let registry = RegistryStore::new(&home);
        assert_eq!(
            registry.lookup_workspace_for_cwd(&cwd).unwrap(),
            Some(DEFAULT_WORKSPACE_ID.to_string())
        );
        assert!(
            !cwd.join(".graft").exists(),
            "workspace_attach must not initialize the attached cwd"
        );

        let (detach, _) = handle_request(
            &engine,
            WireRequest {
                id: "detach".to_string(),
                op: "workspace_detach".to_string(),
                params: json!({"cwd": cwd}),
            },
        );

        assert!(detach.ok, "{detach:?}");
        assert_eq!(
            detach.result.unwrap()["registry_changed"].as_bool(),
            Some(true)
        );
        assert_eq!(registry.lookup_workspace_for_cwd(&cwd).unwrap(), None);

        let _ = std::fs::remove_dir_all(engine_root);
        let _ = std::fs::remove_dir_all(cwd);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn global_request_rejects_null_params() {
        let (dir, engine, _tree_id) = seeded_engine("global_null_params");

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "status".to_string(),
                op: "status".to_string(),
                params: Value::Null,
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "params must be a JSON object");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_routed_op_does_not_require_workspace_route() {
        let (dir, store, _tree_id) = seeded_store("unknown_routed_op");
        let state = DaemonState::new(store);

        for params in [
            json!({}),
            json!({
                "workspace_id": "ws:does-not-exist",
                "workspace_root": "/not/a/workspace"
            }),
        ] {
            let (response, _) = handle_routed_request(
                &state,
                WireRequest {
                    id: "unknown".to_string(),
                    op: "not_a_real_op".to_string(),
                    params,
                },
            );

            assert!(!response.ok);
            let error = response.error.unwrap();
            assert_eq!(error.code, "E_UNKNOWN_OP");
            assert_eq!(error.message, "unknown op not_a_real_op");
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn routed_request_rejects_non_object_params_before_workspace_route() {
        let (dir, store, _tree_id) = seeded_store("routed_non_object_params");
        let state = DaemonState::new(store);

        for params in [Value::Null, json!(["not", "an", "object"])] {
            let (response, _) = handle_routed_request(
                &state,
                WireRequest {
                    id: "write".to_string(),
                    op: "scratch_write".to_string(),
                    params,
                },
            );

            assert!(!response.ok);
            let error = response.error.unwrap();
            assert_eq!(error.code, "E_BAD_PARAMS");
            assert_eq!(error.message, "params must be a JSON object");
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn routed_request_rejects_unknown_fields_before_workspace_route() {
        let (dir, store, _tree_id) = seeded_store("routed_unknown_field");
        let state = DaemonState::new(store);

        let (response, _) = handle_routed_request(
            &state,
            WireRequest {
                id: "write".to_string(),
                op: "scratch_write".to_string(),
                params: json!({
                    "extra": true
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "unknown field extra for daemon op");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn routed_typed_ops_resolve_workspace_id_through_registry() {
        let home =
            std::env::temp_dir().join(format!("graftd-route-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (workspace, store, _tree_id) = seeded_store("routed_registry");
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:registered", WorkspaceKind::Local, &workspace)
            .unwrap();
        let state = DaemonState::new_with_registry(GraftStore::open(&workspace), registry);

        let (response, _) = handle_routed_request(
            &state,
            WireRequest {
                id: "write".to_string(),
                op: "scratch_write".to_string(),
                params: json!({
                    "workspace_id": "ws:registered",
                    "workspace_root": workspace.clone(),
                    "base": "graft:empty",
                    "path": "hello.txt",
                    "content": "hello\n"
                }),
            },
        );

        assert!(response.ok, "{response:?}");
        assert!(
            response.result.as_ref().unwrap()["scratch"]
                .as_str()
                .is_some_and(|id| id.starts_with("scratch:"))
        );

        drop(store);
        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn default_workspace_engine_uses_normalized_route_key() {
        use std::os::unix::fs::symlink;

        let home = std::env::temp_dir().join(format!(
            "graftd-default-normalized-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (workspace, _store, _tree_id) = seeded_store("default_normalized_workspace");
        let link = home.join("workspace-link");
        symlink(&workspace, &link).unwrap();

        let state =
            DaemonState::new_with_registry(GraftStore::open(&link), RegistryStore::new(&home));
        let expected_key = normalize_workspace_path(&workspace);
        assert_eq!(state.default_workspace_root, expected_key);
        {
            let engines = state.engines.lock().unwrap();
            assert_eq!(engines.len(), 1);
            assert!(engines.contains_key(&expected_key));
            assert!(!engines.contains_key(&link));
        }

        let _ = state.default_engine();
        assert_eq!(state.engines.lock().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn routed_typed_ops_reject_workspace_id_root_mismatch() {
        let home =
            std::env::temp_dir().join(format!("graftd-route-mismatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (workspace, _store, _tree_id) = seeded_store("route_mismatch_workspace");
        let other_root = home.join("other-root");
        std::fs::create_dir_all(&other_root).unwrap();
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:registered", WorkspaceKind::Local, &workspace)
            .unwrap();
        let state = DaemonState::new_with_registry(GraftStore::open(&workspace), registry);

        let (response, _) = handle_routed_request(
            &state,
            WireRequest {
                id: "write".to_string(),
                op: "scratch_write".to_string(),
                params: json!({
                    "workspace_id": "ws:registered",
                    "workspace_root": other_root,
                    "base": "graft:empty",
                    "path": "hello.txt",
                    "content": "hello\n"
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_WORKSPACE_ROUTE_MISMATCH");

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn routed_typed_ops_require_workspace_root() {
        let home =
            std::env::temp_dir().join(format!("graftd-route-missing-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (workspace, _store, _tree_id) = seeded_store("route_missing_root_workspace");
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:registered", WorkspaceKind::Local, &workspace)
            .unwrap();
        let state = DaemonState::new_with_registry(GraftStore::open(&workspace), registry);

        let (response, _) = handle_routed_request(
            &state,
            WireRequest {
                id: "write".to_string(),
                op: "scratch_write".to_string(),
                params: json!({
                    "workspace_id": "ws:registered",
                    "base": "graft:empty",
                    "path": "hello.txt",
                    "content": "hello\n"
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_MISSING_FIELD");
        assert_eq!(error.message, "missing field workspace_root");

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn routed_typed_ops_reject_blank_workspace_route_fields() {
        let home =
            std::env::temp_dir().join(format!("graftd-route-blank-fields-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (workspace, _store, _tree_id) = seeded_store("route_blank_fields_workspace");
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:registered", WorkspaceKind::Local, &workspace)
            .unwrap();
        let state = DaemonState::new_with_registry(GraftStore::open(&workspace), registry);

        for (field, params) in [
            (
                "workspace_id",
                json!({
                    "workspace_id": " \t",
                    "workspace_root": workspace.clone(),
                    "base": "graft:empty",
                    "path": "hello.txt",
                    "content": "hello\n"
                }),
            ),
            (
                "workspace_root",
                json!({
                    "workspace_id": "ws:registered",
                    "workspace_root": " \t",
                    "base": "graft:empty",
                    "path": "hello.txt",
                    "content": "hello\n"
                }),
            ),
        ] {
            let (response, _) = handle_routed_request(
                &state,
                WireRequest {
                    id: format!("blank-{field}"),
                    op: "scratch_write".to_string(),
                    params,
                },
            );

            assert!(!response.ok, "{field}: {response:?}");
            let error = response.error.unwrap();
            assert_eq!(error.code, "E_BAD_PARAMS");
            assert_eq!(error.message, format!("field {field} must not be empty"));
        }

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn cli_exec_is_also_workspace_routed() {
        let home = std::env::temp_dir().join(format!(
            "graftd-cli-exec-route-mismatch-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (workspace, _store, _tree_id) = seeded_store("cli_exec_route_mismatch_workspace");
        let other_root = home.join("other-root");
        std::fs::create_dir_all(&other_root).unwrap();
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:registered", WorkspaceKind::Local, &workspace)
            .unwrap();
        let state = DaemonState::new_with_registry(GraftStore::open(&workspace), registry);

        let (response, _) = handle_routed_request(
            &state,
            WireRequest {
                id: "cli".to_string(),
                op: "cli_exec".to_string(),
                params: json!({
                    "workspace_id": "ws:registered",
                    "workspace_root": other_root,
                    "argv": ["graft", "status"]
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_WORKSPACE_ROUTE_MISMATCH");

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn cli_exec_rejects_unknown_wire_fields() {
        let (dir, engine, _tree_id) = seeded_engine("cli_exec_unknown_field");

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "cli".to_string(),
                op: "cli_exec".to_string(),
                params: json!({
                    "argv": ["graft", "status"],
                    "extra": true
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "unknown field extra for daemon op");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cli_exec_requires_workspace_id() {
        let (dir, engine, _tree_id) = seeded_engine("cli_exec_missing_workspace_id");

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "cli".to_string(),
                op: "cli_exec".to_string(),
                params: json!({
                    "argv": ["graft", "gc", "--apply"],
                    "workspace_root": dir
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_MISSING_FIELD");
        assert_eq!(error.message, "missing field workspace_id");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn routed_requests_reject_non_string_workspace_root() {
        let home =
            std::env::temp_dir().join(format!("graftd-route-bad-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let (workspace, _store, _tree_id) = seeded_store("route_bad_root_workspace");
        let registry = RegistryStore::new(&home);
        registry
            .ensure_workspace("ws:registered", WorkspaceKind::Local, &workspace)
            .unwrap();
        let state = DaemonState::new_with_registry(GraftStore::open(&workspace), registry);

        let (response, _) = handle_routed_request(
            &state,
            WireRequest {
                id: "write".to_string(),
                op: "scratch_write".to_string(),
                params: json!({
                    "workspace_id": "ws:registered",
                    "workspace_root": true,
                    "base": "graft:empty",
                    "path": "hello.txt",
                    "content": "hello\n"
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "field workspace_root must be string");

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn handles_base_and_from_read_write_edit_delete_flow() {
        let (dir, engine, tree_id) = seeded_engine("base_from_flow");

        let (read_base, _) = handle_request(
            &engine,
            WireRequest {
                id: "read-base".to_string(),
                op: "scratch_read".to_string(),
                params: json!({"base": tree_id, "path":"hello.txt", "mode":"text"}),
            },
        );
        assert!(read_base.ok);
        assert_eq!(
            read_base.result.as_ref().unwrap()["content"].as_str(),
            Some("hello\n")
        );
        let root = read_base.result.as_ref().unwrap()["scratch"]
            .as_str()
            .unwrap()
            .to_string();

        let (write_base, _) = handle_request(
            &engine,
            WireRequest {
                id: "write-base".to_string(),
                op: "scratch_write".to_string(),
                params: json!({"base": tree_id, "path":"bye.txt", "content":"bye\n"}),
            },
        );
        assert!(write_base.ok);
        let write_result = write_base.result.as_ref().unwrap();
        assert_eq!(write_result["parent"].as_str(), Some(root.as_str()));
        assert!(
            write_result["changed_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path.as_str() == Some("bye.txt"))
        );
        let written = write_result["scratch"].as_str().unwrap().to_string();

        let (read_from, _) = handle_request(
            &engine,
            WireRequest {
                id: "read-from".to_string(),
                op: "scratch_read".to_string(),
                params: json!({"from": written, "path":"bye.txt", "mode":"text"}),
            },
        );
        assert!(read_from.ok);
        assert_eq!(
            read_from.result.as_ref().unwrap()["content"].as_str(),
            Some("bye\n")
        );

        let (edit_from, _) = handle_request(
            &engine,
            WireRequest {
                id: "edit-from".to_string(),
                op: "scratch_edit".to_string(),
                params: json!({
                    "from": written,
                    "path":"bye.txt",
                    "edits":[{"kind":"replace_text","old_text":"bye","new_text":"ciao"}]
                }),
            },
        );
        assert!(edit_from.ok);
        assert!(
            edit_from.result.as_ref().unwrap()["changed_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path.as_str() == Some("bye.txt"))
        );
        let edited = edit_from.result.as_ref().unwrap()["scratch"]
            .as_str()
            .unwrap()
            .to_string();

        let (delete_from, _) = handle_request(
            &engine,
            WireRequest {
                id: "delete-from".to_string(),
                op: "scratch_delete".to_string(),
                params: json!({"from": edited, "path":"bye.txt"}),
            },
        );
        assert!(delete_from.ok);
        let deleted = delete_from.result.as_ref().unwrap()["scratch"]
            .as_str()
            .unwrap()
            .to_string();

        let (diff, _) = handle_request(
            &engine,
            WireRequest {
                id: "diff".to_string(),
                op: "scratch_diff".to_string(),
                params: json!({"from": written, "to": deleted}),
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

        let (edit_base, _) = handle_request(
            &engine,
            WireRequest {
                id: "edit-base".to_string(),
                op: "scratch_edit".to_string(),
                params: json!({
                    "base": tree_id,
                    "path":"hello.txt",
                    "edits":[{"kind":"replace_text","old_text":"hello","new_text":"hi"}]
                }),
            },
        );
        assert!(edit_base.ok);
        assert!(
            edit_base.result.as_ref().unwrap()["updated_anchors"]
                .as_str()
                .unwrap()
                .contains("hi")
        );

        let (delete_base, _) = handle_request(
            &engine,
            WireRequest {
                id: "delete-base".to_string(),
                op: "scratch_delete".to_string(),
                params: json!({"base": tree_id, "path":"hello.txt"}),
            },
        );
        assert!(delete_base.ok);
        assert_eq!(delete_base.result.as_ref().unwrap()["path"], "hello.txt");

        let (missing, _) = handle_request(
            &engine,
            WireRequest {
                id: "delete-missing".to_string(),
                op: "scratch_delete".to_string(),
                params: json!({"base": tree_id, "path":"missing.txt"}),
            },
        );
        assert!(!missing.ok);
        let error = missing.error.unwrap();
        assert_eq!(error.code, "E_STORE");
        assert!(
            error
                .message
                .contains("virtual path not found: missing.txt")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scratch_write_rejects_unknown_wire_fields() {
        let (dir, engine, tree_id) = seeded_engine("scratch_write_unknown_field");

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "write".to_string(),
                op: "scratch_write".to_string(),
                params: json!({
                    "base": tree_id,
                    "path": "hello.txt",
                    "content": "hello\n",
                    "typo": true
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "unknown field typo for daemon op");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scratch_read_rejects_non_string_mode_instead_of_defaulting() {
        let (dir, engine, tree_id) = seeded_engine("bad_read_mode_type");

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "read".to_string(),
                op: "scratch_read".to_string(),
                params: json!({"base": tree_id, "path":"hello.txt", "mode": true}),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "field mode must be string");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn handles_candidate_from_scratch_wire_op() {
        let (dir, engine, tree_id) = seeded_engine("candidate_from_scratch");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let write = engine.write(&root, "bye.txt", b"bye\n").unwrap();

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "candidate".to_string(),
                op: "candidate_from_scratch".to_string(),
                params: json!({
                    "scratch": write.scratch,
                    "producer": "test-daemon",
                    "message": "demo candidate"
                }),
            },
        );
        assert!(response.ok);
        let result = response.result.as_ref().unwrap();
        let candidate_id = result["candidate"].as_str().unwrap();
        assert!(candidate_id.starts_with("candidate:"));
        assert!(
            result["changed_paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path.as_str() == Some("bye.txt"))
        );

        let candidate = engine.store().read_candidate(candidate_id).unwrap();
        assert_eq!(candidate.provenance.producer, "test-daemon");
        assert_eq!(
            candidate.provenance.message.as_deref(),
            Some("demo candidate")
        );
        assert_eq!(candidate.constraint, Constraint::Top);
        assert_eq!(
            engine
                .store()
                .candidate_evidence_index(candidate_id)
                .unwrap(),
            Vec::<String>::new()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn candidate_from_scratch_rejects_unknown_wire_fields() {
        let (dir, engine, tree_id) = seeded_engine("candidate_unknown_field");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let write = engine.write(&root, "bye.txt", b"bye\n").unwrap();

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "candidate".to_string(),
                op: "candidate_from_scratch".to_string(),
                params: json!({
                    "scratch": write.scratch,
                    "unexpected": "value"
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "unknown field unexpected for daemon op");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn candidate_from_scratch_rejects_non_string_optional_metadata() {
        let (dir, engine, tree_id) = seeded_engine("candidate_bad_metadata_type");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let write = engine.write(&root, "bye.txt", b"bye\n").unwrap();

        let (response, _) = handle_request(
            &engine,
            WireRequest {
                id: "candidate".to_string(),
                op: "candidate_from_scratch".to_string(),
                params: json!({
                    "scratch": write.scratch,
                    "producer": 42
                }),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_BAD_PARAMS");
        assert_eq!(error.message, "field producer must be string");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn candidate_from_scratch_rejects_blank_optional_metadata() {
        let (dir, engine, tree_id) = seeded_engine("candidate_blank_metadata");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let write = engine.write(&root, "bye.txt", b"bye\n").unwrap();

        for (field, params) in [
            (
                "producer",
                json!({
                    "scratch": write.scratch,
                    "producer": " \t"
                }),
            ),
            (
                "message",
                json!({
                    "scratch": write.scratch,
                    "message": " \t"
                }),
            ),
        ] {
            let (response, _) = handle_request(
                &engine,
                WireRequest {
                    id: format!("candidate-{field}"),
                    op: "candidate_from_scratch".to_string(),
                    params,
                },
            );

            assert!(!response.ok, "{field}: {response:?}");
            let error = response.error.unwrap();
            assert_eq!(error.code, "E_BAD_PARAMS");
            assert_eq!(error.message, format!("field {field} must not be empty"));
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_missing_or_ambiguous_scratch_source_params() {
        let (dir, engine, tree_id) = seeded_engine("source_params");

        let (missing, _) = handle_request(
            &engine,
            WireRequest {
                id: "missing-source".to_string(),
                op: "scratch_read".to_string(),
                params: json!({"path":"hello.txt"}),
            },
        );
        assert!(!missing.ok);
        assert_eq!(missing.error.unwrap().code, "E_MISSING_FIELD");

        let (ambiguous, _) = handle_request(
            &engine,
            WireRequest {
                id: "ambiguous-source".to_string(),
                op: "scratch_read".to_string(),
                params: json!({"base": tree_id, "from":"scratch:deadbeef", "path":"hello.txt"}),
            },
        );
        assert!(!ambiguous.ok);
        assert_eq!(ambiguous.error.unwrap().code, "E_BAD_PARAMS");

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
    fn maps_lost_scratch_to_retry_from_base_error() {
        let (dir, engine, tree_id) = seeded_engine("lost_scratch");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let restarted = ScratchEngine::new(GraftStore::open(&dir));

        let (response, _) = handle_request(
            &restarted,
            WireRequest {
                id: "lost".to_string(),
                op: "scratch_read".to_string(),
                params: json!({"from": root, "path":"hello.txt", "mode":"text"}),
            },
        );

        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_SCRATCH_LOST");
        assert!(error.message.contains("retry from --base"));

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
                    "from": scratch,
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
