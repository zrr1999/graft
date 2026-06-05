use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(crate) fn add_workspace_route(
    params: &mut Value,
    workspace_root: &Path,
    workspace_id: &str,
) -> Result<()> {
    let Some(object) = params.as_object_mut() else {
        bail!("[E_BAD_PARAMS] daemon request params must be a JSON object");
    };
    if workspace_id.trim().is_empty() {
        bail!("[E_BAD_PARAMS] workspace_id must not be empty");
    }
    let workspace_root = workspace_root_wire_string(workspace_root)?;
    object.insert("workspace_root".to_string(), json!(workspace_root));
    object.insert("workspace_id".to_string(), json!(workspace_id));
    Ok(())
}

pub(crate) fn workspace_root_wire_string(workspace_root: &Path) -> Result<&str> {
    let root = workspace_root.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "[E_UNREPRESENTABLE_WORKSPACE_ROOT] workspace root contains non-UTF-8 bytes and cannot be encoded in the current daemon JSON wire protocol: {}",
            workspace_root.display()
        )
    })?;
    if root.trim().is_empty() {
        bail!("[E_BAD_PARAMS] workspace root must not be empty");
    }
    Ok(root)
}

pub(crate) fn render_json_result(result: &Value) -> Result<String> {
    serde_json::to_string_pretty(result)
        .context("[E_BAD_DAEMON_RESPONSE] failed to render daemon result")
}

pub(crate) fn required_string_field(result: &Value, context: &str, field: &str) -> Result<String> {
    result
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[E_BAD_DAEMON_RESPONSE] {context} result missing string field `{field}`"
            )
        })
}

pub(crate) fn require_string_array_field(result: &Value, context: &str, field: &str) -> Result<()> {
    let values = result.get(field).and_then(Value::as_array).ok_or_else(|| {
        anyhow::anyhow!(
            "[E_BAD_DAEMON_RESPONSE] {context} result missing string array field `{field}`"
        )
    })?;
    for (index, value) in values.iter().enumerate() {
        if value.as_str().is_none() {
            return Err(anyhow::anyhow!(
                "[E_BAD_DAEMON_RESPONSE] {context} result field `{field}` item {index} must be string"
            ));
        }
    }
    Ok(())
}

pub(crate) fn require_bool_field(result: &Value, context: &str, field: &str) -> Result<()> {
    if result.get(field).and_then(Value::as_bool).is_some() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "[E_BAD_DAEMON_RESPONSE] {context} result missing bool field `{field}`"
        ))
    }
}

pub(crate) fn require_u64_field(result: &Value, context: &str, field: &str) -> Result<()> {
    if result.get(field).and_then(Value::as_u64).is_some() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "[E_BAD_DAEMON_RESPONSE] {context} result missing integer field `{field}`"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn add_workspace_route_requires_object_params() {
        let mut params = json!(null);

        let error = add_workspace_route(&mut params, Path::new("/workspace"), "ws:test")
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_BAD_PARAMS]"), "{error}");
    }

    #[test]
    fn add_workspace_route_rejects_empty_workspace_id() {
        let mut params = json!({});

        let error = add_workspace_route(&mut params, Path::new("/workspace"), " \t")
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_BAD_PARAMS]"), "{error}");
        assert!(error.contains("workspace_id must not be empty"), "{error}");
        assert!(params.get("workspace_root").is_none(), "{params}");
        assert!(params.get("workspace_id").is_none(), "{params}");
    }

    #[test]
    fn add_workspace_route_rejects_empty_workspace_root() {
        let mut params = json!({});

        let error = add_workspace_route(&mut params, Path::new(""), "ws:test")
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_BAD_PARAMS]"), "{error}");
        assert!(
            error.contains("workspace root must not be empty"),
            "{error}"
        );
        assert!(params.get("workspace_root").is_none(), "{params}");
        assert!(params.get("workspace_id").is_none(), "{params}");
    }

    #[cfg(unix)]
    #[test]
    fn add_workspace_route_rejects_non_utf8_workspace_root() {
        let mut params = json!({});
        let root =
            std::path::PathBuf::from(OsString::from_vec(b"/tmp/graft-workspace-\xFF".to_vec()));

        let error = add_workspace_route(&mut params, &root, "ws:test")
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("[E_UNREPRESENTABLE_WORKSPACE_ROOT]"),
            "{error}"
        );
        assert!(params.get("workspace_root").is_none(), "{params}");
        assert!(params.get("workspace_id").is_none(), "{params}");
    }

    #[test]
    fn render_json_result_pretty_prints_payload() {
        let rendered = render_json_result(&json!({"scratch": "scratch:abc"})).unwrap();

        assert!(rendered.contains("\"scratch\": \"scratch:abc\""));
    }

    #[test]
    fn required_field_helpers_reject_missing_or_wrong_types() {
        let result = json!({
            "candidate": "candidate:abc",
            "changed_paths": ["a.txt"]
        });

        assert_eq!(
            required_string_field(&result, "candidate_from_scratch", "candidate").unwrap(),
            "candidate:abc"
        );
        require_string_array_field(&result, "candidate_from_scratch", "changed_paths").unwrap();

        let missing = required_string_field(&result, "candidate_from_scratch", "scratch")
            .unwrap_err()
            .to_string();
        assert!(
            missing.contains("missing string field `scratch`"),
            "{missing}"
        );

        let wrong_type = require_string_array_field(&result, "candidate_from_scratch", "candidate")
            .unwrap_err()
            .to_string();
        assert!(
            wrong_type.contains("missing string array field `candidate`"),
            "{wrong_type}"
        );

        let wrong_item = require_string_array_field(
            &json!({"changed_paths": ["a.txt", 42]}),
            "candidate_from_scratch",
            "changed_paths",
        )
        .unwrap_err()
        .to_string();
        assert!(
            wrong_item.contains("field `changed_paths` item 1 must be string"),
            "{wrong_item}"
        );

        let wrong_bool = require_bool_field(&result, "scratch_drop", "candidate")
            .unwrap_err()
            .to_string();
        assert!(
            wrong_bool.contains("missing bool field `candidate`"),
            "{wrong_bool}"
        );

        let wrong_integer = require_u64_field(&result, "scratch_read", "candidate")
            .unwrap_err()
            .to_string();
        assert!(
            wrong_integer.contains("missing integer field `candidate`"),
            "{wrong_integer}"
        );
    }
}
