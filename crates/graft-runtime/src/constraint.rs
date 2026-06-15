use anyhow::{Context, Result};
use clap::Subcommand;
use graft_store::GraftStore;

use crate::config::{
    constraint_lock_drift, load_constraint_catalog, read_constraint_lock,
    require_constraint_lock_current, write_constraint_lock_with_plans,
};
use crate::view::CommandEnvelope;

#[derive(Subcommand, Debug)]
pub(crate) enum ConstraintCommand {
    /// Rebuild graft.lock from constraints.roto
    Lock,
    /// Check that graft.lock matches constraints.roto
    Check,
    /// List configured constraints and locked ids
    List,
    /// Show one configured constraint and locked id
    Show {
        /// Constraint name to show
        name: String,
    },
}

pub(crate) fn run_constraint_command(
    store: &GraftStore,
    command: &ConstraintCommand,
) -> Result<CommandEnvelope> {
    let catalog = load_constraint_catalog(store)?;
    let defs = catalog.defs;
    match command {
        ConstraintCommand::Lock => {
            let previous = read_constraint_lock(store)?;
            let new_lock = write_constraint_lock_with_plans(store, &defs, &catalog.plans)?;
            let message = if let Some(previous) = previous {
                let drift = constraint_lock_drift(&defs, &previous)?;
                if drift.is_clean() {
                    "constraint lock already current".to_string()
                } else {
                    format!("repaired graft.lock ({})", drift.summary())
                }
            } else {
                "created graft.lock".to_string()
            };
            Ok(CommandEnvelope {
                message: Some(format!(
                    "{message}; {} constraints locked",
                    new_lock.constraints.len()
                )),
                ..CommandEnvelope::ok()
            })
        }
        ConstraintCommand::Check => {
            let lock = require_constraint_lock_current(store, &defs)?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "constraint lock current; {} constraints locked",
                    lock.constraints.len()
                )),
                ..CommandEnvelope::ok()
            })
        }
        ConstraintCommand::List => {
            let lock = require_constraint_lock_current(store, &defs)?;
            let mut lines = Vec::new();
            for name in defs.keys() {
                let id = lock
                    .constraints
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
        ConstraintCommand::Show { name } => {
            let lock = require_constraint_lock_current(store, &defs)?;
            let constraint = defs.get(name).with_context(|| {
                format!("[E_UNKNOWN_CONSTRAINT] constraint {name} is not configured")
            })?;
            let id = lock.constraints.get(name).with_context(|| {
                format!(
                    "[E_CONSTRAINT_LOCK_DRIFT] constraint {name} is missing from graft.lock; run `graft constraint lock` to refresh it"
                )
            })?;
            Ok(CommandEnvelope {
                message: Some(format!(
                    "constraint: {name}\nid: {id}\n{}",
                    configured_constraint_body(constraint)?
                )),
                ..CommandEnvelope::ok()
            })
        }
    }
}

fn configured_constraint_body(def: &graft_core::ConstraintDef) -> Result<String> {
    serde_json::to_string_pretty(def).context("serialize constraint def")
}
