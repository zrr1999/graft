use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use graft_core::{Constraint, GraftCandidate, PropertyRef};

use crate::config::{
    GraftConfig, RequiredPropertiesConfig, required_properties_constraint, resolve_property,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromotionRequirementPlan {
    pub(crate) properties: Vec<PropertyRef>,
    pub(crate) constraint: Constraint,
    pub(crate) source: PromotionRequirementSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromotionRequirementSource {
    Cli,
    Config,
}

impl PromotionRequirementSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Config => "config",
        }
    }
}

pub(crate) fn parse_properties(config: &GraftConfig, names: &[String]) -> Result<Vec<PropertyRef>> {
    parse_property_constraint(config, names).map(|constraint| constraint_primitives(&constraint))
}

pub(crate) fn parse_property_constraint(
    config: &GraftConfig,
    names: &[String],
) -> Result<Constraint> {
    let roots = names
        .iter()
        .map(|name| resolve_property_ref(config, name))
        .map(|property| property.map(|property| Constraint::Primitive { property }))
        .collect::<Result<Vec<_>>>()?;
    expand_constraint_with_requires(config, &Constraint::all_of(roots))
}

pub(crate) fn resolve_property_ref(config: &GraftConfig, value: &str) -> Result<PropertyRef> {
    if value.trim().is_empty() {
        bail!("[E_INVALID_PROPERTY] property requirement must not be empty");
    }
    if value.contains(':') {
        bail!(
            "[E_SCOPED_PROPERTY_UNSUPPORTED] property requirement `{value}` must be a bare property name; properties are whole-workspace by definition"
        );
    }
    resolve_property(config, value)
}

pub(crate) fn constraint_from_properties(properties: &[PropertyRef]) -> Constraint {
    Constraint::all_of(
        properties
            .iter()
            .cloned()
            .map(|property| Constraint::Primitive { property })
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn constraint_primitives(constraint: &Constraint) -> Vec<PropertyRef> {
    let mut primitives = Vec::new();
    collect_constraint_primitives(constraint, &mut primitives);
    dedupe_properties(primitives)
}

fn collect_constraint_primitives(constraint: &Constraint, primitives: &mut Vec<PropertyRef>) {
    match constraint {
        Constraint::Top | Constraint::Bottom => {}
        Constraint::Primitive { property } => primitives.push(property.clone()),
        Constraint::Both { left, right } | Constraint::Either { left, right } => {
            collect_constraint_primitives(left, primitives);
            collect_constraint_primitives(right, primitives);
        }
    }
}

pub(crate) fn expand_constraint_with_requires(
    config: &GraftConfig,
    constraint: &Constraint,
) -> Result<Constraint> {
    expand_constraint_inner(config, constraint, &mut Vec::new())
}

fn expand_constraint_inner(
    config: &GraftConfig,
    constraint: &Constraint,
    visiting: &mut Vec<String>,
) -> Result<Constraint> {
    match constraint {
        Constraint::Top => Ok(Constraint::Top),
        Constraint::Bottom => Ok(Constraint::Bottom),
        Constraint::Primitive { property } => {
            expand_primitive_with_requires(config, property, visiting)
        }
        Constraint::Both { left, right } => Ok(Constraint::Both {
            left: Box::new(expand_constraint_inner(config, left, visiting)?),
            right: Box::new(expand_constraint_inner(config, right, visiting)?),
        }),
        Constraint::Either { left, right } => Ok(Constraint::Either {
            left: Box::new(expand_constraint_inner(config, left, visiting)?),
            right: Box::new(expand_constraint_inner(config, right, visiting)?),
        }),
    }
}

fn expand_primitive_with_requires(
    config: &GraftConfig,
    property: &PropertyRef,
    visiting: &mut Vec<String>,
) -> Result<Constraint> {
    if let Some(start) = visiting.iter().position(|name| name == &property.name) {
        let mut cycle = visiting[start..].to_vec();
        cycle.push(property.name.clone());
        bail!(
            "[E_PROPERTY_REQUIRES_CYCLE] property requires cycle while expanding requirements: {}",
            cycle.join(" -> ")
        );
    }

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

    visiting.push(property.name.clone());
    let mut items = Vec::new();
    for required in &spec.plan.requires {
        let Some(required_spec) = config.properties.get(required.as_str()) else {
            bail!(
                "[E_PROPERTY_REQUIRES_UNKNOWN] property `{}` requires unknown property `{}`",
                property.name,
                required.as_str()
            );
        };
        let required = required_spec.property_ref()?;
        items.push(expand_primitive_with_requires(config, &required, visiting)?);
    }
    visiting.pop();
    items.push(Constraint::Primitive {
        property: property.clone(),
    });
    Ok(Constraint::all_of(items))
}

/// Returns the explicit `--expect` set, or an empty list when no expectation
/// was passed. Derived candidates without explicit expectations fall back to
/// `[admission.required_properties]` at admission/validation time.
pub(crate) fn needs_revalidation_or(
    config: &GraftConfig,
    names: &[String],
) -> Result<Vec<PropertyRef>> {
    if names.is_empty() {
        Ok(Vec::new())
    } else {
        parse_properties(config, names)
    }
}

pub(crate) fn admission_required_constraint(
    config: &GraftConfig,
    candidate: &GraftCandidate,
    requested: &[String],
) -> Result<Constraint> {
    let mut constraints = vec![
        required_properties_constraint(config, &config.admission.required_properties)?,
        candidate.constraint.clone(),
    ];
    if !requested.is_empty() {
        constraints.push(parse_property_constraint(config, requested)?);
    }
    expand_constraint_with_requires(config, &Constraint::all_of(constraints))
}

pub(crate) fn validation_constraint_with_base(
    config: &GraftConfig,
    requested: &[String],
    subject_constraint: &Constraint,
) -> Result<Constraint> {
    if requested.is_empty() {
        expand_constraint_with_requires(
            config,
            &Constraint::all_of(vec![
                required_properties_constraint(config, &config.admission.required_properties)?,
                subject_constraint.clone(),
            ]),
        )
    } else {
        parse_property_constraint(config, requested)
    }
}

pub(crate) fn promotion_requirement_plan(
    config: &GraftConfig,
    requested: &[String],
) -> Result<PromotionRequirementPlan> {
    if !requested.is_empty() {
        let constraint = parse_property_constraint(config, requested)?;
        return Ok(PromotionRequirementPlan {
            properties: constraint_primitives(&constraint),
            constraint,
            source: PromotionRequirementSource::Cli,
        });
    }

    let constraint = expand_constraint_with_requires(
        config,
        &required_properties_constraint(config, &config.promotion.required_properties)?,
    )?;
    Ok(PromotionRequirementPlan {
        properties: constraint_primitives(&constraint),
        constraint,
        source: PromotionRequirementSource::Config,
    })
}

pub(crate) fn promotion_requirement_plan_with_target(
    config: &GraftConfig,
    requested: &[String],
    target_required: Option<&RequiredPropertiesConfig>,
) -> Result<PromotionRequirementPlan> {
    let mut plan = promotion_requirement_plan(config, requested)?;
    if let Some(required) = target_required {
        let target_constraint = expand_constraint_with_requires(
            config,
            &required_properties_constraint(config, required)?,
        )?;
        plan.constraint = Constraint::all_of(vec![plan.constraint, target_constraint]);
        plan.properties = constraint_primitives(&plan.constraint);
    }
    Ok(plan)
}

pub(crate) fn property_matches(property: &PropertyRef, requested: &str) -> bool {
    property.name == requested || property.id.as_str() == requested
}

pub(crate) fn property_matches_request(
    config: &GraftConfig,
    property: &PropertyRef,
    requested: &str,
) -> Result<bool> {
    if property_matches(property, requested) {
        return Ok(true);
    }
    if let Some(spec) = config.properties.get(requested) {
        return Ok(spec.property_id()? == property.id);
    }
    Ok(false)
}

pub(crate) fn property_label(property: &PropertyRef) -> String {
    property.name.clone()
}

fn dedupe_properties(properties: Vec<PropertyRef>) -> Vec<PropertyRef> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for property in properties {
        if seen.insert(property.id.clone()) {
            deduped.push(property);
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{
        ApplicationId, ApplicationRef, CheckPlan, PropertyName, PropertyPlan, PropertyRef,
        PropertySpec, Provenance, Severity,
    };

    fn config_with_properties(names: &[&str]) -> GraftConfig {
        let mut config: GraftConfig = toml::from_str(
            r#"
		[admission]
		required_properties = ["review_policy"]

		[promotion]
		required_properties = ["cargo_tests_pass"]
	"#,
        )
        .unwrap();
        for name in names {
            config
                .properties
                .insert((*name).to_string(), property_spec(name));
        }
        config
    }

    fn property_spec(name: &str) -> PropertySpec {
        property_spec_with_pattern(name, name)
    }

    fn property_spec_with_requires(name: &str, requires: &[&str]) -> PropertySpec {
        let mut spec = property_spec(name);
        spec.plan.requires = requires
            .iter()
            .map(|name| PropertyName::new(*name))
            .collect();
        spec
    }

    fn property_spec_with_pattern(name: &str, pattern: &str) -> PropertySpec {
        PropertySpec {
            name: PropertyName::new(name),
            plan: PropertyPlan {
                checks: vec![CheckPlan::Unavailable {
                    reason: pattern.to_string(),
                }],
                requires: Vec::new(),
            },
            description: format!("{name} test property"),
            severity: Severity::Blocking,
            source_ref: None,
        }
    }

    #[test]
    fn property_request_matches_current_property_by_property_id() {
        let mut config = config_with_properties(&[]);
        let current = property_spec_with_pattern("current_property", "same-plan");
        let current_id = current.property_id().unwrap();
        config
            .properties
            .insert("current_property".to_string(), current);
        let stored_ref = PropertyRef::new(current_id, "old_display_name");

        assert!(!property_matches(&stored_ref, "current_property"));
        assert!(property_matches_request(&config, &stored_ref, "current_property").unwrap());
    }

    #[test]
    fn admission_required_properties_include_configured_properties() {
        let config = config_with_properties(&["review_policy", "tests_pass", "cargo_tests_pass"]);
        let candidate = demo_candidate(&config);

        let required = constraint_primitives(
            &admission_required_constraint(&config, &candidate, &[]).unwrap(),
        );

        assert_eq!(
            required.iter().map(property_label).collect::<Vec<_>>(),
            vec!["review_policy".to_string(), "tests_pass".to_string()]
        );
    }

    #[test]
    fn admission_required_properties_append_requested_requirements() {
        let config = config_with_properties(&[
            "review_policy",
            "tests_pass",
            "extra_policy",
            "cargo_tests_pass",
        ]);
        let candidate = demo_candidate(&config);

        let required = constraint_primitives(
            &admission_required_constraint(&config, &candidate, &["extra_policy".into()]).unwrap(),
        );

        assert_eq!(
            required.iter().map(property_label).collect::<Vec<_>>(),
            vec![
                "review_policy".to_string(),
                "tests_pass".to_string(),
                "extra_policy".to_string()
            ]
        );
    }

    #[test]
    fn parse_properties_expands_transitive_requires_in_dependency_order() {
        let mut config = config_with_properties(&[]);
        config
            .properties
            .insert("format_clean".to_string(), property_spec("format_clean"));
        config.properties.insert(
            "tests_pass".to_string(),
            property_spec_with_requires("tests_pass", &["format_clean"]),
        );
        config.properties.insert(
            "safe_patch".to_string(),
            property_spec_with_requires("safe_patch", &["tests_pass"]),
        );

        let properties = parse_properties(&config, &["safe_patch".into()]).unwrap();

        assert_eq!(
            properties.iter().map(property_label).collect::<Vec<_>>(),
            vec![
                "format_clean".to_string(),
                "tests_pass".to_string(),
                "safe_patch".to_string()
            ]
        );
    }

    #[test]
    fn admission_required_properties_expands_candidate_expected_requires() {
        let mut config = config_with_properties(&["review_policy", "cargo_tests_pass"]);
        config
            .properties
            .insert("format_clean".to_string(), property_spec("format_clean"));
        config.properties.insert(
            "tests_pass".to_string(),
            property_spec_with_requires("tests_pass", &["format_clean"]),
        );
        let candidate = demo_candidate(&config);

        let required = constraint_primitives(
            &admission_required_constraint(&config, &candidate, &[]).unwrap(),
        );

        assert_eq!(
            required.iter().map(property_label).collect::<Vec<_>>(),
            vec![
                "review_policy".to_string(),
                "format_clean".to_string(),
                "tests_pass".to_string()
            ]
        );
    }

    #[test]
    fn expand_constraint_preserves_requires_as_nested_both() {
        let mut config = config_with_properties(&[]);
        config
            .properties
            .insert("format_clean".to_string(), property_spec("format_clean"));
        config
            .properties
            .insert("lint_clean".to_string(), property_spec("lint_clean"));
        config.properties.insert(
            "safe_patch".to_string(),
            property_spec_with_requires("safe_patch", &["format_clean", "lint_clean"]),
        );

        let safe_patch = Constraint::primitive(resolve_property(&config, "safe_patch").unwrap());
        let expanded = expand_constraint_with_requires(&config, &safe_patch).unwrap();

        assert_eq!(
            expanded,
            Constraint::all_of(vec![
                Constraint::primitive(resolve_property(&config, "format_clean").unwrap(),),
                Constraint::primitive(resolve_property(&config, "lint_clean").unwrap(),),
                Constraint::primitive(resolve_property(&config, "safe_patch").unwrap(),),
            ])
        );
    }

    #[test]
    fn expand_constraint_preserves_either_branches() {
        let mut config = config_with_properties(&[]);
        config
            .properties
            .insert("format_clean".to_string(), property_spec("format_clean"));
        config.properties.insert(
            "quick_policy".to_string(),
            property_spec_with_requires("quick_policy", &["format_clean"]),
        );
        config
            .properties
            .insert("manual_policy".to_string(), property_spec("manual_policy"));

        let quick = Constraint::primitive(resolve_property(&config, "quick_policy").unwrap());
        let manual = Constraint::primitive(resolve_property(&config, "manual_policy").unwrap());
        let expanded =
            expand_constraint_with_requires(&config, &Constraint::any_of(vec![quick, manual]))
                .unwrap();

        assert_eq!(
            expanded,
            Constraint::any_of(vec![
                Constraint::all_of(vec![
                    Constraint::primitive(resolve_property(&config, "format_clean").unwrap(),),
                    Constraint::primitive(resolve_property(&config, "quick_policy").unwrap(),),
                ]),
                Constraint::primitive(resolve_property(&config, "manual_policy").unwrap(),),
            ])
        );
    }

    #[test]
    fn expand_constraint_reports_missing_required_property() {
        let mut config = config_with_properties(&[]);
        config.properties.insert(
            "dependent".to_string(),
            property_spec_with_requires("dependent", &["missing_dependency"]),
        );

        let dependent = Constraint::primitive(resolve_property(&config, "dependent").unwrap());
        let error = expand_constraint_with_requires(&config, &dependent)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_PROPERTY_REQUIRES_UNKNOWN]"), "{error}");
        assert!(error.contains("dependent"), "{error}");
        assert!(error.contains("missing_dependency"), "{error}");
    }

    #[test]
    fn expand_constraint_reports_requires_cycle() {
        let mut config = config_with_properties(&[]);
        config
            .properties
            .insert("a".to_string(), property_spec_with_requires("a", &["b"]));
        config
            .properties
            .insert("b".to_string(), property_spec_with_requires("b", &["a"]));

        let a = Constraint::primitive(resolve_property(&config, "a").unwrap());
        let error = expand_constraint_with_requires(&config, &a)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_PROPERTY_REQUIRES_CYCLE]"), "{error}");
        assert!(error.contains("a -> b -> a"), "{error}");
    }

    #[test]
    fn repeated_cli_properties_accumulate_into_all_of_constraint() {
        let config = config_with_properties(&["review_policy", "tests_pass", "extra_policy"]);

        let constraint = parse_property_constraint(
            &config,
            &["tests_pass".to_string(), "extra_policy".to_string()],
        )
        .unwrap();

        assert_eq!(
            constraint,
            Constraint::all_of(vec![
                Constraint::primitive(resolve_property(&config, "tests_pass").unwrap(),),
                Constraint::primitive(resolve_property(&config, "extra_policy").unwrap(),),
            ])
        );
    }

    #[test]
    fn admission_cli_require_accumulates_with_config_and_candidate_constraints() {
        let config = config_with_properties(&[
            "review_policy",
            "tests_pass",
            "extra_policy",
            "cargo_tests_pass",
        ]);
        let candidate = demo_candidate(&config);

        let constraint =
            admission_required_constraint(&config, &candidate, &["extra_policy".to_string()])
                .unwrap();

        assert_eq!(
            constraint,
            Constraint::all_of(vec![
                Constraint::primitive(resolve_property(&config, "review_policy").unwrap(),),
                Constraint::primitive(resolve_property(&config, "tests_pass").unwrap(),),
                Constraint::primitive(resolve_property(&config, "extra_policy").unwrap(),),
            ])
        );
    }

    #[test]
    fn promotion_cli_require_accumulates_with_target_required_properties() {
        let config = config_with_properties(&[
            "review_policy",
            "cargo_tests_pass",
            "extra_policy",
            "release_policy",
        ]);
        let target_required =
            crate::config::RequiredPropertiesConfig::Names(vec!["release_policy".to_string()]);

        let plan = promotion_requirement_plan_with_target(
            &config,
            &["extra_policy".to_string()],
            Some(&target_required),
        )
        .unwrap();

        assert_eq!(plan.source, PromotionRequirementSource::Cli);
        assert_eq!(
            plan.constraint,
            Constraint::all_of(vec![
                Constraint::primitive(resolve_property(&config, "extra_policy").unwrap(),),
                Constraint::primitive(resolve_property(&config, "release_policy").unwrap(),),
            ])
        );
        assert_eq!(
            plan.properties
                .iter()
                .map(property_label)
                .collect::<Vec<_>>(),
            vec!["extra_policy".to_string(), "release_policy".to_string(),]
        );
    }

    #[test]
    fn promotion_requires_cli_or_config_properties() {
        let config = config_with_properties(&["review_policy", "cargo_tests_pass", "extra_policy"]);
        let missing_config = GraftConfig::default();

        let configured = promotion_requirement_plan(&config, &[]).unwrap();
        assert_eq!(configured.source, PromotionRequirementSource::Config);
        assert_eq!(
            configured
                .properties
                .iter()
                .map(property_label)
                .collect::<Vec<_>>(),
            vec!["cargo_tests_pass".to_string()]
        );

        let missing = promotion_requirement_plan(&missing_config, &[]).unwrap();
        assert_eq!(missing.source, PromotionRequirementSource::Config);
        assert!(missing.properties.is_empty());

        let cli = promotion_requirement_plan(&config, &["extra_policy".into()]).unwrap();
        assert_eq!(cli.source, PromotionRequirementSource::Cli);
        assert_eq!(
            cli.properties
                .iter()
                .map(property_label)
                .collect::<Vec<_>>(),
            vec!["extra_policy".to_string()]
        );
    }

    fn demo_candidate(config: &GraftConfig) -> GraftCandidate {
        GraftCandidate {
            id: graft_core::CandidateId::new("candidate:demo"),
            application: ApplicationRef::Stored(ApplicationId::new("application:demo")),
            constraint: Constraint::primitive(resolve_property(config, "tests_pass").unwrap()),
            provenance: Provenance::now("test", None),
        }
    }
}
