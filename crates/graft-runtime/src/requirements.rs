use std::collections::BTreeSet;

use anyhow::{Result, bail};
use graft_core::{GraftCandidate, PropertyRef};

use crate::config::{GraftConfig, resolve_property};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromotionRequirementPlan {
    pub(crate) properties: Vec<PropertyRef>,
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
    names
        .iter()
        .map(|name| resolve_property(config, name))
        .collect()
}

pub(crate) fn expected_properties(
    config: &GraftConfig,
    names: &[String],
) -> Result<Vec<PropertyRef>> {
    if names.is_empty() {
        parse_properties(config, &["ValidPatch".to_string()])
    } else {
        parse_properties(config, names)
    }
}

/// Returns the explicit `--expect` set, or an empty list when no expectation
/// was passed. Derived candidates without explicit expectations fall back to
/// `[admission].base_properties` at admission/validation time.
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

pub(crate) fn admission_required_properties(
    config: &GraftConfig,
    candidate: &GraftCandidate,
    requested: &[String],
) -> Result<Vec<PropertyRef>> {
    let mut properties = parse_properties(config, &config.admission.base_properties)?;
    properties.extend(candidate.expected.clone());
    if !requested.is_empty() {
        properties.extend(parse_properties(config, requested)?);
    }
    Ok(dedupe_properties(properties))
}

pub(crate) fn validation_properties_with_base(
    config: &GraftConfig,
    requested: &[String],
    subject_properties: &[PropertyRef],
) -> Result<Vec<PropertyRef>> {
    if requested.is_empty() {
        let mut properties = parse_properties(config, &config.admission.base_properties)?;
        properties.extend(subject_properties.to_vec());
        Ok(dedupe_properties(properties))
    } else {
        parse_properties(config, requested)
    }
}

pub(crate) fn promotion_requirement_plan(
    config: &GraftConfig,
    requested: &[String],
) -> Result<PromotionRequirementPlan> {
    if !requested.is_empty() {
        return Ok(PromotionRequirementPlan {
            properties: dedupe_properties(parse_properties(config, requested)?),
            source: PromotionRequirementSource::Cli,
        });
    }

    if !config.promotion.required_properties.is_empty() {
        return Ok(PromotionRequirementPlan {
            properties: dedupe_properties(parse_properties(
                config,
                &config.promotion.required_properties,
            )?),
            source: PromotionRequirementSource::Config,
        });
    }

    bail!(
        "promotion requires explicit properties; set [promotion].required_properties in graft.toml or pass --require"
    )
}

pub(crate) fn property_matches(property: &PropertyRef, requested: &str) -> bool {
    property.name == requested || property.id.as_str() == requested
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
    use graft_core::{ChangeRef, Evaluator, Judge, PropertyDef, Provenance, Query, StateId};
    use std::collections::BTreeMap;

    fn config_with_properties(names: &[&str]) -> GraftConfig {
        let mut config: GraftConfig = toml::from_str(
            r#"
[admission]
base_properties = ["ValidPatch"]

[promotion]
required_properties = ["CargoTestsPass"]
"#,
        )
        .unwrap();
        for name in names {
            config
                .properties
                .insert((*name).to_string(), property_def(name));
        }
        config
    }

    fn property_def(name: &str) -> PropertyDef {
        PropertyDef {
            name: name.to_string(),
            query: Query::Change,
            evaluator: Evaluator::Builtin {
                name: "has_change".to_string(),
                options: BTreeMap::from([("test_name".to_string(), name.to_string())]),
            },
            judge: Judge::ExitCodeZero,
        }
    }

    #[test]
    fn admission_required_properties_include_configured_base_properties() {
        let config = config_with_properties(&["ValidPatch", "TestsPass", "CargoTestsPass"]);
        let candidate = demo_candidate(&config);

        let required = admission_required_properties(&config, &candidate, &[]).unwrap();

        assert_eq!(
            required.iter().map(property_label).collect::<Vec<_>>(),
            vec!["ValidPatch".to_string(), "TestsPass".to_string()]
        );
    }

    #[test]
    fn admission_required_properties_append_requested_requirements() {
        let config = config_with_properties(&[
            "ValidPatch",
            "TestsPass",
            "NoModelWeightChange",
            "CargoTestsPass",
        ]);
        let candidate = demo_candidate(&config);

        let required =
            admission_required_properties(&config, &candidate, &["NoModelWeightChange".into()])
                .unwrap();

        assert_eq!(
            required.iter().map(property_label).collect::<Vec<_>>(),
            vec![
                "ValidPatch".to_string(),
                "TestsPass".to_string(),
                "NoModelWeightChange".to_string()
            ]
        );
    }

    #[test]
    fn promotion_requires_cli_or_config_properties() {
        let config =
            config_with_properties(&["ValidPatch", "CargoTestsPass", "NoModelWeightChange"]);
        let missing_config: GraftConfig = toml::from_str(
            r#"
[admission]
base_properties = ["ValidPatch"]
"#,
        )
        .unwrap();

        let configured = promotion_requirement_plan(&config, &[]).unwrap();
        assert_eq!(configured.source, PromotionRequirementSource::Config);
        assert_eq!(
            configured
                .properties
                .iter()
                .map(property_label)
                .collect::<Vec<_>>(),
            vec!["CargoTestsPass".to_string()]
        );

        let missing = promotion_requirement_plan(&missing_config, &[]).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("[promotion].required_properties")
        );

        let cli = promotion_requirement_plan(&config, &["NoModelWeightChange".into()]).unwrap();
        assert_eq!(cli.source, PromotionRequirementSource::Cli);
        assert_eq!(
            cli.properties
                .iter()
                .map(property_label)
                .collect::<Vec<_>>(),
            vec!["NoModelWeightChange".to_string()]
        );
    }

    fn demo_candidate(config: &GraftConfig) -> GraftCandidate {
        GraftCandidate {
            id: graft_core::CandidateId::new("candidate:demo"),
            base_state: StateId::GitTree("base".to_string()),
            target_state: StateId::GitTree("target".to_string()),
            change: ChangeRef::InlineSummary("demo".to_string()),
            expected: vec![resolve_property(config, "TestsPass").unwrap()],
            provenance: Provenance::now("test", None),
        }
    }
}
