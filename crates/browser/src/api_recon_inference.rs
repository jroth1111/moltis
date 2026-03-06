//! Schema inference engine for API response bodies.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::api_recon_types::{
    ApiBodyClassification, ApiContractNode, ApiFieldContract, ApiInference, ApiInferenceNotes,
    ApiOpaqueReason, ApiScalarKind,
};

pub const MAX_BODY_BYTES: usize = 100_000;
pub const MAX_SCHEMA_DEPTH: usize = 20;
pub const MAX_SCHEMA_KEYS: usize = 500;
pub const MAX_ARRAY_SCHEMA_SAMPLES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AggregateNode {
    Scalar(ApiScalarKind),
    Nullable(Box<AggregateNode>),
    Array {
        items: Box<AggregateNode>,
    },
    Object {
        sample_count: u64,
        fields: BTreeMap<String, AggregateField>,
    },
    OneOf(Vec<AggregateNode>),
    Opaque(ApiOpaqueReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AggregateField {
    pub present_count: u64,
    pub node: AggregateNode,
}

pub fn infer_contract_from_body(body: Option<&str>, content_type: Option<&str>) -> ApiInference {
    let Some(raw_body) = body else {
        return ApiInference::missing();
    };

    let mut notes = ApiInferenceNotes::default();
    let (body_slice, body_truncated) = truncate_body_for_inference(raw_body, MAX_BODY_BYTES);
    notes.body_truncated = body_truncated;

    let trimmed = body_slice.trim_start();
    if trimmed.is_empty() {
        return ApiInference {
            classification: ApiBodyClassification::Missing,
            contract: Some(ApiContractNode::Opaque {
                reason: ApiOpaqueReason::EmptyBody,
            }),
            notes,
        };
    }

    let looks_json = content_type
        .map(|ct| ct.to_ascii_lowercase().contains("json"))
        .unwrap_or(false)
        || trimmed.starts_with('{')
        || trimmed.starts_with('[');

    if !looks_json {
        return ApiInference {
            classification: ApiBodyClassification::NonJson,
            contract: Some(ApiContractNode::Opaque {
                reason: ApiOpaqueReason::NonJson,
            }),
            notes,
        };
    }

    match serde_json::from_str::<Value>(body_slice) {
        Ok(value) => {
            let aggregate = infer_aggregate_node(&value, 0, &mut notes);
            ApiInference {
                classification: ApiBodyClassification::Json,
                contract: Some(aggregate_to_public_node(&aggregate)),
                notes,
            }
        },
        Err(_) => ApiInference {
            classification: ApiBodyClassification::ParseError,
            contract: Some(ApiContractNode::Opaque {
                reason: ApiOpaqueReason::ParseError,
            }),
            notes,
        },
    }
}

fn infer_aggregate_node(
    value: &Value,
    depth: usize,
    notes: &mut ApiInferenceNotes,
) -> AggregateNode {
    if depth >= MAX_SCHEMA_DEPTH {
        notes.depth_truncations = notes.depth_truncations.saturating_add(1);
        return AggregateNode::Opaque(ApiOpaqueReason::DepthLimit);
    }

    match value {
        Value::Null => AggregateNode::Scalar(ApiScalarKind::Null),
        Value::Bool(_) => AggregateNode::Scalar(ApiScalarKind::Boolean),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                AggregateNode::Scalar(ApiScalarKind::Integer)
            } else {
                AggregateNode::Scalar(ApiScalarKind::Number)
            }
        },
        Value::String(_) => AggregateNode::Scalar(ApiScalarKind::String),
        Value::Array(values) => {
            let mut iter = values.iter().take(MAX_ARRAY_SCHEMA_SAMPLES);
            let items = iter.next().map_or(
                AggregateNode::Opaque(ApiOpaqueReason::EmptyArray),
                |first| infer_aggregate_node(first, depth + 1, notes),
            );
            let merged = iter.fold(items, |acc, item| {
                let inferred = infer_aggregate_node(item, depth + 1, notes);
                merge_aggregate_nodes(&acc, &inferred)
            });
            AggregateNode::Array {
                items: Box::new(merged),
            }
        },
        Value::Object(map) => {
            let mut fields = BTreeMap::new();
            for (idx, (key, value)) in map.iter().enumerate() {
                if idx >= MAX_SCHEMA_KEYS {
                    notes.key_truncations = notes.key_truncations.saturating_add(1);
                    break;
                }
                fields.insert(
                    key.clone(),
                    AggregateField {
                        present_count: 1,
                        node: infer_aggregate_node(value, depth + 1, notes),
                    },
                );
            }
            AggregateNode::Object {
                sample_count: 1,
                fields,
            }
        },
    }
}

fn truncate_body_for_inference(body: &str, max_bytes: usize) -> (&str, bool) {
    if body.len() <= max_bytes {
        return (body, false);
    }
    let mut boundary = max_bytes.min(body.len());
    while boundary > 0 && !body.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (&body[..boundary], true)
}

pub(crate) fn merge_aggregate_nodes(left: &AggregateNode, right: &AggregateNode) -> AggregateNode {
    if left == right {
        return left.clone();
    }

    match (left, right) {
        (
            AggregateNode::Object {
                sample_count: left_samples,
                fields: left_fields,
            },
            AggregateNode::Object {
                sample_count: right_samples,
                fields: right_fields,
            },
        ) => {
            let mut fields = BTreeMap::new();
            let mut all_keys = BTreeSet::new();
            all_keys.extend(left_fields.keys().cloned());
            all_keys.extend(right_fields.keys().cloned());
            for key in all_keys {
                match (left_fields.get(&key), right_fields.get(&key)) {
                    (Some(left_field), Some(right_field)) => {
                        fields.insert(
                            key,
                            AggregateField {
                                present_count: left_field.present_count + right_field.present_count,
                                node: merge_aggregate_nodes(&left_field.node, &right_field.node),
                            },
                        );
                    },
                    (Some(left_field), None) => {
                        fields.insert(key, left_field.clone());
                    },
                    (None, Some(right_field)) => {
                        fields.insert(key, right_field.clone());
                    },
                    (None, None) => {},
                }
            }
            AggregateNode::Object {
                sample_count: left_samples + right_samples,
                fields,
            }
        },
        (
            AggregateNode::Array { items: left_items },
            AggregateNode::Array { items: right_items },
        ) => AggregateNode::Array {
            items: Box::new(merge_aggregate_nodes(left_items, right_items)),
        },
        (AggregateNode::Nullable(left), AggregateNode::Nullable(right)) => {
            AggregateNode::Nullable(Box::new(merge_aggregate_nodes(left, right)))
        },
        _ => {
            let mut variants = Vec::new();
            flatten_one_of(left, &mut variants);
            flatten_one_of(right, &mut variants);
            variants.dedup();
            AggregateNode::OneOf(variants)
        },
    }
}

fn flatten_one_of(node: &AggregateNode, output: &mut Vec<AggregateNode>) {
    match node {
        AggregateNode::OneOf(variants) => {
            for variant in variants {
                flatten_one_of(variant, output);
            }
        },
        _ => {
            if !output.contains(node) {
                output.push(node.clone());
            }
        },
    }
}

pub(crate) fn aggregate_to_public_node(node: &AggregateNode) -> ApiContractNode {
    match node {
        AggregateNode::Scalar(kind) => ApiContractNode::Scalar { kind: *kind },
        AggregateNode::Nullable(value) => ApiContractNode::Nullable {
            value: Box::new(aggregate_to_public_node(value)),
        },
        AggregateNode::Array { items } => ApiContractNode::Array {
            items: Box::new(aggregate_to_public_node(items)),
        },
        AggregateNode::Object {
            sample_count,
            fields,
        } => ApiContractNode::Object {
            fields: fields
                .iter()
                .map(|(field, aggregate)| {
                    (
                        field.clone(),
                        ApiFieldContract {
                            schema: aggregate_to_public_node(&aggregate.node),
                            required_rate: if *sample_count == 0 {
                                0.0
                            } else {
                                aggregate.present_count as f32 / *sample_count as f32
                            },
                        },
                    )
                })
                .collect(),
        },
        AggregateNode::OneOf(variants) => ApiContractNode::OneOf {
            variants: variants.iter().map(aggregate_to_public_node).collect(),
        },
        AggregateNode::Opaque(reason) => ApiContractNode::Opaque { reason: *reason },
    }
}

pub(crate) fn public_to_aggregate(node: &ApiContractNode) -> AggregateNode {
    match node {
        ApiContractNode::Scalar { kind } => AggregateNode::Scalar(*kind),
        ApiContractNode::Nullable { value } => {
            AggregateNode::Nullable(Box::new(public_to_aggregate(value)))
        },
        ApiContractNode::Array { items } => AggregateNode::Array {
            items: Box::new(public_to_aggregate(items)),
        },
        ApiContractNode::Object { fields } => AggregateNode::Object {
            sample_count: 1,
            fields: fields
                .iter()
                .map(|(field, contract)| {
                    (
                        field.clone(),
                        AggregateField {
                            present_count: 1,
                            node: public_to_aggregate(&contract.schema),
                        },
                    )
                })
                .collect(),
        },
        ApiContractNode::OneOf { variants } => {
            AggregateNode::OneOf(variants.iter().map(public_to_aggregate).collect())
        },
        ApiContractNode::Opaque { reason } => AggregateNode::Opaque(*reason),
    }
}

pub(crate) fn merge_optional_node(target: &mut Option<AggregateNode>, node: &ApiContractNode) {
    let candidate = public_to_aggregate(node);
    match target.take() {
        Some(existing) => *target = Some(merge_aggregate_nodes(&existing, &candidate)),
        None => *target = Some(candidate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_simple_object() {
        let result =
            infer_contract_from_body(Some(r#"{"id":1,"name":"A"}"#), Some("application/json"));
        assert_eq!(result.classification, ApiBodyClassification::Json);
        assert!(result.contract.is_some());
    }

    #[test]
    fn infer_nested_array() {
        let result = infer_contract_from_body(
            Some(r#"[{"id":1},{"id":2,"extra":true}]"#),
            Some("application/json"),
        );
        assert_eq!(result.classification, ApiBodyClassification::Json);
        let contract = result.contract.as_ref().expect("contract");
        assert!(matches!(contract, ApiContractNode::Array { .. }));
    }

    #[test]
    fn infer_empty_body() {
        let result = infer_contract_from_body(Some(""), Some("application/json"));
        assert_eq!(result.classification, ApiBodyClassification::Missing);
    }

    #[test]
    fn infer_non_json() {
        let result = infer_contract_from_body(Some("<html></html>"), Some("text/html"));
        assert_eq!(result.classification, ApiBodyClassification::NonJson);
    }

    #[test]
    fn infer_missing_body() {
        let result = infer_contract_from_body(None, None);
        assert_eq!(result.classification, ApiBodyClassification::Missing);
        assert!(result.contract.is_none());
    }

    #[test]
    fn merge_objects_tracks_required_rates() {
        let left =
            infer_contract_from_body(Some(r#"{"id":1,"name":"A"}"#), Some("application/json"));
        let right = infer_contract_from_body(Some(r#"{"id":2}"#), Some("application/json"));

        let mut aggregate = None;
        merge_optional_node(
            &mut aggregate,
            left.contract.as_ref().expect("left contract"),
        );
        merge_optional_node(
            &mut aggregate,
            right.contract.as_ref().expect("right contract"),
        );

        let public = aggregate_to_public_node(aggregate.as_ref().expect("aggregate"));
        let ApiContractNode::Object { fields } = public else {
            panic!("expected object");
        };

        assert_eq!(fields["id"].required_rate, 1.0);
        assert!(fields["name"].required_rate < 1.0);
    }

    #[test]
    fn truncate_utf8_safe() {
        let body = format!(
            "{{\"emoji\":\"{}\"}}",
            "😀".repeat((MAX_BODY_BYTES / "😀".len()) + 2)
        );
        let result = infer_contract_from_body(Some(&body), Some("application/json"));
        assert!(result.notes.body_truncated);
        assert_eq!(result.classification, ApiBodyClassification::ParseError);
    }
}
