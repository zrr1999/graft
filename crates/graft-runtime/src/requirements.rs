use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use graft_core::{GraftCandidate, PropertyRef, PropertyScope, ScopedPropertyRef};

use crate::config::{GraftConfig, resolve_property};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromotionRequirementPlan {
    pub(crate) properties: Vec<ScopedPropertyRef>,
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

pub(crate) fn parse_scoped_properties(
    config: &GraftConfig,
    names: &[String],
) -> Result<Vec<ScopedPropertyRef>> {
    let roots = names
        .iter()
        .map(|name| resolve_scoped_property_ref(config, name))
        .collect::<Result<Vec<_>>>()?;
    expand_scoped_property_refs_with_requires(config, roots)
}

pub(crate) fn resolve_scoped_property_ref(
    config: &GraftConfig,
    value: &str,
) -> Result<ScopedPropertyRef> {
    let (scope, name) = value.split_once(':').with_context(|| {
        format!(
            "[E_UNSCOPED_PROPERTY] property requirement `{value}` must use workspace:<property>, for example workspace:{value}"
        )
    })?;
    if name.is_empty() {
        bail!(
            "[E_INVALID_PROPERTY_SCOPE] property requirement `{value}` is missing a property name"
        );
    }
    let scope = resolve_property_scope(config, scope)?;
    Ok(ScopedPropertyRef::new(
        scope,
        resolve_property(config, name)?,
    ))
}

pub(crate) fn scoped_properties_from_map(
    config: &GraftConfig,
    properties: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<ScopedPropertyRef>> {
    let mut roots = Vec::new();
    for (scope, names) in properties {
        let scope = resolve_property_scope(config, scope)?;
        for name in names {
            roots.push(ScopedPropertyRef::new(
                scope.clone(),
                resolve_property(config, name)?,
            ));
        }
    }
    expand_scoped_property_refs_with_requires(config, roots)
}

fn resolve_property_scope(_config: &GraftConfig, scope: &str) -> Result<PropertyScope> {
    if scope == "workspace" {
        return Ok(PropertyScope::Workspace);
    }
    bail!(
        "[E_UNSUPPORTED_PROPERTY_SCOPE] property scope `{scope}` is not supported; properties run over the complete workspace state, so use workspace:<property> and inspect worktrees/<repo-id> inside the property when cross-repo logic is needed"
    )
}

/// Returns the explicit `--expect` set, or an empty list when no expectation
/// was passed. Derived candidates without explicit expectations fall back to
/// `[admission.required_properties]` at admission/validation time.
pub(crate) fn needs_revalidation_or(
    config: &GraftConfig,
    names: &[String],
) -> Result<Vec<ScopedPropertyRef>> {
    if names.is_empty() {
        Ok(Vec::new())
    } else {
        parse_scoped_properties(config, names)
    }
}

pub(crate) fn admission_required_scoped_properties(
    config: &GraftConfig,
    candidate: &GraftCandidate,
    requested: &[String],
) -> Result<Vec<ScopedPropertyRef>> {
    let mut properties = scoped_properties_from_map(config, &config.admission.required_properties)?;
    properties.extend(candidate.expected.iter().cloned());
    if !requested.is_empty() {
        properties.extend(parse_scoped_properties(config, requested)?);
    }
    expand_scoped_property_refs_with_requires(config, properties)
}

pub(crate) fn validation_properties_with_base(
    config: &GraftConfig,
    requested: &[String],
    subject_properties: &[ScopedPropertyRef],
) -> Result<Vec<ScopedPropertyRef>> {
    if requested.is_empty() {
        let mut properties =
            scoped_properties_from_map(config, &config.admission.required_properties)?;
        properties.extend(subject_properties.iter().cloned());
        expand_scoped_property_refs_with_requires(config, properties)
    } else {
        parse_scoped_properties(config, requested)
    }
}

pub(crate) fn promotion_requirement_plan(
    config: &GraftConfig,
    requested: &[String],
) -> Result<PromotionRequirementPlan> {
    if !requested.is_empty() {
        return Ok(PromotionRequirementPlan {
            properties: dedupe_scoped_properties(parse_scoped_properties(config, requested)?),
            source: PromotionRequirementSource::Cli,
        });
    }

    Ok(PromotionRequirementPlan {
        properties: dedupe_scoped_properties(scoped_properties_from_map(
            config,
            &config.promotion.required_properties,
        )?),
        source: PromotionRequirementSource::Config,
    })
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

pub(crate) fn scoped_property_label(property: &ScopedPropertyRef) -> String {
    property.label()
}

pub(crate) fn property_refs_for_scoped(properties: &[ScopedPropertyRef]) -> Vec<PropertyRef> {
    dedupe_property_refs(properties.iter().map(|item| item.property.clone()))
}

pub(crate) fn expand_scoped_property_refs_with_requires(
    config: &GraftConfig,
    roots: Vec<ScopedPropertyRef>,
) -> Result<Vec<ScopedPropertyRef>> {
    let mut seen = BTreeSet::new();
    let mut visiting = Vec::new();
    let mut out = Vec::new();
    for property in roots {
        visit_scoped_property_requires(config, &property, &mut seen, &mut visiting, &mut out)?;
    }
    Ok(out)
}

fn visit_scoped_property_requires(
    config: &GraftConfig,
    property: &ScopedPropertyRef,
    seen: &mut BTreeSet<(PropertyScope, graft_core::PropertyId)>,
    visiting: &mut Vec<String>,
    out: &mut Vec<ScopedPropertyRef>,
) -> Result<()> {
    if seen.contains(&(property.scope.clone(), property.property.id.clone())) {
        return Ok(());
    }
    if let Some(start) = visiting.iter().position(|name| name == &property.label()) {
        let mut cycle = visiting[start..].to_vec();
        cycle.push(property.label());
        bail!(
            "[E_PROPERTY_REQUIRES_CYCLE] property requires cycle while expanding requirements: {}",
            cycle.join(" -> ")
        );
    }

    let spec = config
        .properties
        .get(&property.property.name)
        .with_context(|| {
            format!(
                "[E_UNKNOWN_PROPERTY] property {} ({}) is not configured in properties.roto",
                property.property.name, property.property.id
            )
        })?;
    let current_id = spec.property_id()?;
    if current_id != property.property.id {
        bail!(
            "[E_PROPERTY_DRIFT] property `{}` drifted: stored ref has {}, current property resolves to {}",
            property.property.name,
            property.property.id,
            current_id
        );
    }

    visiting.push(property.label());
    for required in &spec.plan.requires {
        let required_ref = resolve_property(config, required.as_str())?;
        let required = ScopedPropertyRef::new(property.scope.clone(), required_ref);
        visit_scoped_property_requires(config, &required, seen, visiting, out)?;
    }
    visiting.pop();

    if seen.insert((property.scope.clone(), property.property.id.clone())) {
        out.push(property.clone());
    }
    Ok(())
}

fn dedupe_property_refs(properties: impl IntoIterator<Item = PropertyRef>) -> Vec<PropertyRef> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for property in properties {
        if seen.insert(property.id.clone()) {
            deduped.push(property);
        }
    }
    deduped
}

fn dedupe_scoped_properties(properties: Vec<ScopedPropertyRef>) -> Vec<ScopedPropertyRef> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for property in properties {
        if seen.insert((property.scope.clone(), property.property.id.clone())) {
            deduped.push(property);
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{
        ChangeRef, CheckPlan, PropertyName, PropertyPlan, PropertyRef, PropertySpec, Provenance,
        Severity, StateId,
    };

    fn config_with_properties(names: &[&str]) -> GraftConfig {
        let mut config: GraftConfig = toml::from_str(
            r#"
		[admission.required_properties]
		workspace = ["review_policy"]

		[promotion.required_properties]
		workspace = ["cargo_tests_pass"]
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
    fn admission_required_properties_include_configured_scoped_properties() {
        let config = config_with_properties(&["review_policy", "tests_pass", "cargo_tests_pass"]);
        let candidate = demo_candidate(&config);

        let required = admission_required_scoped_properties(&config, &candidate, &[]).unwrap();

        assert_eq!(
            required
                .iter()
                .map(scoped_property_label)
                .collect::<Vec<_>>(),
            vec![
                "workspace:review_policy".to_string(),
                "workspace:tests_pass".to_string()
            ]
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

        let required = admission_required_scoped_properties(
            &config,
            &candidate,
            &["workspace:extra_policy".into()],
        )
        .unwrap();

        assert_eq!(
            required
                .iter()
                .map(scoped_property_label)
                .collect::<Vec<_>>(),
            vec![
                "workspace:review_policy".to_string(),
                "workspace:tests_pass".to_string(),
                "workspace:extra_policy".to_string()
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

        let properties =
            parse_scoped_properties(&config, &["workspace:safe_patch".into()]).unwrap();

        assert_eq!(
            properties
                .iter()
                .map(scoped_property_label)
                .collect::<Vec<_>>(),
            vec![
                "workspace:format_clean".to_string(),
                "workspace:tests_pass".to_string(),
                "workspace:safe_patch".to_string()
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

        let required = admission_required_scoped_properties(&config, &candidate, &[]).unwrap();

        assert_eq!(
            required
                .iter()
                .map(scoped_property_label)
                .collect::<Vec<_>>(),
            vec![
                "workspace:review_policy".to_string(),
                "workspace:format_clean".to_string(),
                "workspace:tests_pass".to_string()
            ]
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
                .map(scoped_property_label)
                .collect::<Vec<_>>(),
            vec!["workspace:cargo_tests_pass".to_string()]
        );

        let missing = promotion_requirement_plan(&missing_config, &[]).unwrap();
        assert_eq!(missing.source, PromotionRequirementSource::Config);
        assert!(missing.properties.is_empty());

        let cli = promotion_requirement_plan(&config, &["workspace:extra_policy".into()]).unwrap();
        assert_eq!(cli.source, PromotionRequirementSource::Cli);
        assert_eq!(
            cli.properties
                .iter()
                .map(scoped_property_label)
                .collect::<Vec<_>>(),
            vec!["workspace:extra_policy".to_string()]
        );
    }

    fn demo_candidate(config: &GraftConfig) -> GraftCandidate {
        GraftCandidate {
            id: graft_core::CandidateId::new("candidate:demo"),
            base_state: StateId::GitTree("base".to_string()),
            target_state: StateId::GitTree("target".to_string()),
            change: ChangeRef::InlineSummary("demo".to_string()),
            expected: vec![ScopedPropertyRef::new(
                PropertyScope::Workspace,
                resolve_property(config, "tests_pass").unwrap(),
            )],
            provenance: Provenance::now("test", None),
        }
    }
}
