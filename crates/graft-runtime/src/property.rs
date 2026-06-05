use anyhow::{Context, Result};
use clap::Subcommand;
use graft_store::GraftStore;

use crate::config::{
    load_property_defs, property_lock_drift, read_property_lock, require_property_lock_current,
    write_property_lock,
};
use crate::view::CommandEnvelope;

#[derive(Subcommand, Debug)]
pub(crate) enum PropertyCommand {
    /// Rebuild graft.lock from properties.roto
    Lock,
    /// Check that graft.lock matches properties.roto
    Check,
    /// List configured properties and locked ids
    List,
    /// Show one configured property and locked id
    Show {
        /// Property name to show
        name: String,
    },
}

pub(crate) fn run_property_command(
    store: &GraftStore,
    command: &PropertyCommand,
) -> Result<CommandEnvelope> {
    let defs = load_property_defs(store)?;
    match command {
        PropertyCommand::Lock => {
            let previous = read_property_lock(store)?;
            let new_lock = write_property_lock(store, &defs)?;
            let message = if let Some(previous) = previous {
                let drift = property_lock_drift(&defs, &previous)?;
                if drift.is_clean() {
                    "property lock already current".to_string()
                } else {
                    format!("repaired graft.lock ({})", drift.summary())
                }
            } else {
                "created graft.lock".to_string()
            };
            Ok(CommandEnvelope {
                message: Some(format!(
                    "{message}; {} properties locked",
                    new_lock.properties.len()
                )),
                ..CommandEnvelope::ok()
            })
        }
        PropertyCommand::Check => {
            let lock = require_property_lock_current(store, &defs)?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "property lock current; {} properties locked",
                    lock.properties.len()
                )),
                ..CommandEnvelope::ok()
            })
        }
        PropertyCommand::List => {
            let lock = require_property_lock_current(store, &defs)?;
            let mut lines = Vec::new();
            for name in defs.keys() {
                let id = lock
                    .properties
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("<missing>");
                lines.push(format!("{name}\t{id}"));
            }
            Ok(CommandEnvelope {
                message: Some(lines.join("\n")),
                ..CommandEnvelope::ok()
            })
        }
        PropertyCommand::Show { name } => {
            let lock = require_property_lock_current(store, &defs)?;
            let property = defs.get(name).with_context(|| {
                format!("[E_UNKNOWN_PROPERTY] property {name} is not configured")
            })?;
            let id = lock.properties.get(name).with_context(|| {
                format!(
                    "[E_PROPERTY_LOCK_DRIFT] property {name} is missing from graft.lock; run `graft property lock` to refresh it"
                )
            })?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "property: {name}\nid: {id}\n{}",
                    configured_property_body(property)?
                )),
                ..CommandEnvelope::ok()
            })
        }
    }
}

fn configured_property_body(spec: &graft_core::PropertySpec) -> Result<String> {
    serde_json::to_string_pretty(spec).context("serialize v2 property spec")
}
