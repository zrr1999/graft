use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use graft_core::{Constraint, GraftCandidate, PlanId};

use crate::config::{
    GraftConfig, PromoteTargetConfig, RequiredConstraintsConfig, required_constraints_constraint,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromotionRequirementPlan {
    pub(crate) constraints: Vec<PlanId>,
    pub(crate) constraint: Constraint,
    pub(crate) source: PromotionRequirementSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PromotionRequirementSource {
    TargetConfig(String),
    GlobalConfig,
    Cli,
}

impl PromotionRequirementSource {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::TargetConfig(target) => format!("target `{target}`"),
            Self::GlobalConfig => "global promotion.required".to_string(),
            Self::Cli => "CLI --require".to_string(),
        }
    }
}

pub(crate) fn parse_constraints(config: &GraftConfig, names: &[String]) -> Result<Vec<PlanId>> {
    parse_constraint_requirement(config, names).map(|constraint| constraint_primitives(&constraint))
}

pub(crate) fn parse_constraint_requirement(
    config: &GraftConfig,
    names: &[String],
) -> Result<Constraint> {
    let roots = names
        .iter()
        .map(|name| resolve_named_constraint(config, name))
        .collect::<Result<Vec<_>>>()?;
    Ok(Constraint::all_of(roots))
}

pub(crate) fn resolve_named_constraint(config: &GraftConfig, value: &str) -> Result<Constraint> {
    if value.trim().is_empty() {
        bail!("[E_INVALID_CONSTRAINT] constraint requirement must not be empty");
    }
    if value.contains(':') {
        bail!(
            "[E_SCOPED_CONSTRAINT_UNSUPPORTED] constraint requirement uses `{value}`, but constraint requirements must be bare names; constraints are whole-workspace by definition"
        );
    }
    config
        .constraints
        .get(value)
        .map(|def| def.body.clone())
        .with_context(|| format!("[E_UNKNOWN_CONSTRAINT] constraint {value} is not configured"))
}

pub(crate) fn constraint_from_plans(constraints: &[PlanId]) -> Constraint {
    Constraint::all_of(constraints.iter().cloned().map(Constraint::primitive))
}

pub(crate) fn constraint_primitives(constraint: &Constraint) -> Vec<PlanId> {
    let mut primitives = Vec::new();
    collect_constraint_primitives(constraint, &mut primitives);
    dedupe_plans(primitives)
}

fn collect_constraint_primitives(constraint: &Constraint, primitives: &mut Vec<PlanId>) {
    match constraint {
        Constraint::Top | Constraint::Bottom => {}
        Constraint::Primitive { plan } => primitives.push(plan.clone()),
        Constraint::Both { left, right } | Constraint::Either { left, right } => {
            collect_constraint_primitives(left, primitives);
            collect_constraint_primitives(right, primitives);
        }
    }
}

pub(crate) fn validation_constraint_with_base(
    config: &GraftConfig,
    cli_constraints: &[String],
    base: &Constraint,
) -> Result<Constraint> {
    Ok(Constraint::all_of(vec![
        base.clone(),
        parse_constraint_requirement(config, cli_constraints)?,
    ]))
}

pub(crate) fn admission_requirement_constraint(
    config: &GraftConfig,
    cli_constraints: &[String],
) -> Result<Constraint> {
    let configured = required_constraints_constraint(config, &config.admission.required)?;
    let requested = parse_constraint_requirement(config, cli_constraints)?;
    Ok(Constraint::all_of(vec![configured, requested]))
}

pub(crate) fn needs_revalidation_or(config: &GraftConfig, names: &[String]) -> Result<Vec<PlanId>> {
    resolve_candidate_constraint_primitives(config, names)
}

pub(crate) fn admission_required_constraint(
    config: &GraftConfig,
    candidate: &GraftCandidate,
    cli_constraints: &[String],
) -> Result<Constraint> {
    Ok(Constraint::all_of(vec![
        candidate.constraint.clone(),
        admission_requirement_constraint(config, cli_constraints)?,
    ]))
}

pub(crate) fn promotion_requirement_plan(
    config: &GraftConfig,
    cli_constraints: &[String],
) -> Result<PromotionRequirementPlan> {
    promotion_requirement_plan_for_target(config, None, cli_constraints)
}

pub(crate) fn promotion_requirement_plan_for_target(
    config: &GraftConfig,
    target_id: Option<&str>,
    cli_constraints: &[String],
) -> Result<PromotionRequirementPlan> {
    let (constraint, source) = if !cli_constraints.is_empty() {
        (
            parse_constraint_requirement(config, cli_constraints)?,
            PromotionRequirementSource::Cli,
        )
    } else if let Some((target_id, target)) = target_id.and_then(|target_id| {
        config
            .promote_targets
            .get(target_id)
            .map(|target| (target_id, target))
    }) {
        target_requirement_constraint(config, target_id, target)?
    } else {
        global_promotion_requirement_constraint(config)?
    };

    let constraints = constraint_primitives(&constraint);
    if constraints.is_empty() {
        bail!(
            "[E_PROMOTION_REQUIREMENT_MISSING] promotion requires at least one constraint via --require, promotion.required, or promote_targets.<target>.required"
        );
    }
    Ok(PromotionRequirementPlan {
        constraints,
        constraint,
        source,
    })
}

fn target_requirement_constraint(
    config: &GraftConfig,
    target_id: &str,
    target: &PromoteTargetConfig,
) -> Result<(Constraint, PromotionRequirementSource)> {
    match &target.required {
        RequiredConstraintsConfig::Names(names) if names.is_empty() => {
            global_promotion_requirement_constraint(config)
        }
        required => Ok((
            required_constraints_constraint(config, required)?,
            PromotionRequirementSource::TargetConfig(target_id.to_string()),
        )),
    }
}

fn global_promotion_requirement_constraint(
    config: &GraftConfig,
) -> Result<(Constraint, PromotionRequirementSource)> {
    Ok((
        required_constraints_constraint(config, &config.promotion.required)?,
        PromotionRequirementSource::GlobalConfig,
    ))
}

pub(crate) fn promotion_requirement_plan_with_target(
    config: &GraftConfig,
    cli_constraints: &[String],
    target_required: Option<&RequiredConstraintsConfig>,
) -> Result<PromotionRequirementPlan> {
    let (constraint, source) = if !cli_constraints.is_empty() {
        (
            parse_constraint_requirement(config, cli_constraints)?,
            PromotionRequirementSource::Cli,
        )
    } else if let Some(required) = target_required {
        match required {
            RequiredConstraintsConfig::Names(names) if names.is_empty() => {
                global_promotion_requirement_constraint(config)?
            }
            required => (
                required_constraints_constraint(config, required)?,
                PromotionRequirementSource::TargetConfig("configured target".to_string()),
            ),
        }
    } else {
        global_promotion_requirement_constraint(config)?
    };
    let constraints = constraint_primitives(&constraint);
    if constraints.is_empty() {
        bail!(
            "[E_PROMOTION_REQUIREMENT_MISSING] promotion requires at least one constraint via --require, promotion.required, or promote_targets.<target>.required"
        );
    }
    Ok(PromotionRequirementPlan {
        constraints,
        constraint,
        source,
    })
}

pub(crate) fn resolve_candidate_constraint_primitives(
    config: &GraftConfig,
    names: &[String],
) -> Result<Vec<PlanId>> {
    if names.is_empty() {
        Ok(Vec::new())
    } else {
        parse_constraints(config, names)
    }
}

pub(crate) fn resolve_constraint_ref(config: &GraftConfig, name: &str) -> Result<PlanId> {
    let plans = parse_constraints(config, &[name.to_string()])?;
    plans
        .into_iter()
        .next()
        .with_context(|| format!("[E_UNKNOWN_CONSTRAINT] constraint {name} has no primitive plan"))
}

pub(crate) fn plan_matches(constraint: &PlanId, requested: &str) -> bool {
    constraint.as_str() == requested
}

pub(crate) fn constraint_matches_request(
    config: &GraftConfig,
    constraint: &PlanId,
    requested: &str,
) -> Result<bool> {
    if plan_matches(constraint, requested) {
        return Ok(true);
    }
    if let Some(def) = config.constraints.get(requested) {
        return Ok(constraint_primitives(&def.body)
            .iter()
            .any(|plan| plan == constraint));
    }
    Ok(false)
}

pub(crate) fn plan_label(constraint: &PlanId) -> String {
    constraint.to_string()
}

fn dedupe_plans(constraints: Vec<PlanId>) -> Vec<PlanId> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for constraint in constraints {
        if seen.insert(constraint.clone()) {
            deduped.push(constraint);
        }
    }
    deduped
}
