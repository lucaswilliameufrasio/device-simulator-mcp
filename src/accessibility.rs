use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Options {
    /// Maximum returned elements (1..=200), default 200.
    pub max_elements: Option<usize>,
    /// Maximum traversal depth (1..=16), default 16.
    pub max_depth: Option<usize>,
    /// Case-sensitive substring of label/AXLabel.
    pub label_contains: Option<String>,
    /// Exact identifier/AXIdentifier.
    pub identifier: Option<String>,
    /// Exact role, or type when role is absent.
    pub role: Option<String>,
}

impl Options {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=200).contains(&self.max_elements.unwrap_or(200)),
            "max_elements must be between 1 and 200"
        );
        anyhow::ensure!(
            (1..=16).contains(&self.max_depth.unwrap_or(16)),
            "max_depth must be between 1 and 16"
        );
        for filter in [&self.label_contains, &self.identifier, &self.role]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                !filter.is_empty() && filter.len() <= 256,
                "AX filters must contain between 1 and 256 bytes"
            );
        }
        Ok(())
    }

    fn matches(&self, node: &Map<String, Value>) -> bool {
        self.label_contains.as_ref().is_none_or(|query| {
            ["label", "AXLabel"].iter().any(|key| {
                node.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(query))
            })
        }) && self.identifier.as_ref().is_none_or(|query| {
            ["identifier", "AXIdentifier"]
                .iter()
                .any(|key| node.get(*key).and_then(Value::as_str) == Some(query.as_str()))
        }) && self.role.as_ref().is_none_or(|query| {
            node.get("role")
                .or_else(|| node.get("type"))
                .and_then(Value::as_str)
                == Some(query.as_str())
        })
    }
}

#[derive(Serialize)]
struct Projection {
    elements: Vec<Value>,
    truncated: bool,
    max_elements: usize,
    max_depth: usize,
    visited_values: usize,
}

fn scalar(value: &Value, truncated: &mut bool) -> Option<Value> {
    match value {
        Value::String(text) => {
            let shortened = text.chars().take(512).collect::<String>();
            *truncated |= shortened.len() < text.len();
            Some(Value::String(shortened))
        }
        Value::Number(_) | Value::Bool(_) => Some(value.clone()),
        _ => None,
    }
}

fn bounds(value: &Value, depth: usize, truncated: &mut bool) -> Option<Value> {
    if let Some(value) = scalar(value, truncated) {
        return Some(value);
    }
    let object = value.as_object()?;
    if depth >= 2 {
        *truncated = true;
        return None;
    }
    let mut result = Map::new();
    for key in ["x", "y", "width", "height", "origin", "size"] {
        if let Some(value) = object
            .get(key)
            .and_then(|value| bounds(value, depth + 1, truncated))
        {
            result.insert(key.to_owned(), value);
        }
    }
    (!result.is_empty()).then_some(Value::Object(result))
}

fn visit(value: &Value, depth: usize, options: &Options, output: &mut Projection) {
    if depth > output.max_depth
        || output.visited_values >= 4096
        || output.elements.len() >= output.max_elements
    {
        output.truncated = true;
        return;
    }
    output.visited_values += 1;
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                visit(value, depth + 1, options, output);
                if output.visited_values >= 4096 || output.elements.len() >= output.max_elements {
                    output.truncated |= index + 1 < values.len();
                    break;
                }
            }
        }
        Value::Object(values) => {
            if options.matches(values) {
                let mut node = Map::new();
                for key in [
                    "AXLabel",
                    "AXValue",
                    "AXIdentifier",
                    "AXFrame",
                    "frame",
                    "label",
                    "value",
                    "identifier",
                    "role",
                    "type",
                    "enabled",
                    "traits",
                ] {
                    if let Some(value) = values.get(key) {
                        let value = if matches!(key, "frame" | "AXFrame") {
                            bounds(value, 0, &mut output.truncated)
                        } else if key == "traits" && value.is_array() {
                            let items = value.as_array().expect("checked array");
                            output.truncated |= items.len() > 32;
                            Some(Value::Array(
                                items
                                    .iter()
                                    .take(32)
                                    .filter_map(|value| scalar(value, &mut output.truncated))
                                    .collect(),
                            ))
                        } else {
                            scalar(value, &mut output.truncated)
                        };
                        if let Some(value) = value {
                            node.insert(key.to_owned(), value);
                        }
                    }
                }
                if !node.is_empty() {
                    output.elements.push(Value::Object(node));
                }
            }
            for key in ["children", "elements", "AXChildren", "tree"] {
                if let Some(value) = values.get(key) {
                    visit(value, depth + 1, options, output);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn project(value: &Value, options: &Options) -> String {
    let mut output = Projection {
        elements: Vec::new(),
        truncated: false,
        max_elements: options.max_elements.unwrap_or(200),
        max_depth: options.max_depth.unwrap_or(16),
        visited_values: 0,
    };
    visit(value, 0, options, &mut output);
    loop {
        let encoded = serde_json::to_string(&output).expect("AX projection is serializable");
        if encoded.len() <= 256 * 1024 {
            return encoded;
        }
        output.elements.pop();
        output.truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_all_filters_to_one_node_before_output_limit() {
        let tree = serde_json::json!({"elements":[
            {"label":"Other","identifier":"sample","role":"button"},
            {"AXLabel":"Add item","AXIdentifier":"sample","type":"button"},
        ]});
        let options = Options {
            label_contains: Some("Add".to_owned()),
            identifier: Some("sample".to_owned()),
            role: Some("button".to_owned()),
            max_elements: Some(1),
            ..Default::default()
        };
        let result: Value = serde_json::from_str(&project(&tree, &options)).unwrap();
        assert_eq!(result["elements"].as_array().unwrap().len(), 1);
        assert_eq!(result["elements"][0]["AXLabel"], "Add item");
    }

    #[test]
    fn limits_unmatched_traversal_and_reports_truncation() {
        let tree = serde_json::json!({"elements":(0..5000).map(|_| serde_json::json!({"label":"Other"})).collect::<Vec<_>>()});
        let result: Value = serde_json::from_str(&project(
            &tree,
            &Options {
                identifier: Some("missing".to_owned()),
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(result["visited_values"], 4096);
        assert_eq!(result["truncated"], true);
        assert!(result["elements"].as_array().unwrap().is_empty());
    }

    #[test]
    fn bounds_strings_and_nested_frame_content() {
        let tree = serde_json::json!({"label":"x".repeat(1000),"frame":{"x":1,"y":2,"arbitrary":"ignored"}});
        let result: Value = serde_json::from_str(&project(&tree, &Default::default())).unwrap();
        assert_eq!(result["elements"][0]["label"].as_str().unwrap().len(), 512);
        assert_eq!(
            result["elements"][0]["frame"],
            serde_json::json!({"x":1,"y":2})
        );
        assert_eq!(result["truncated"], true);
    }

    #[test]
    fn rejects_out_of_range_options() {
        assert!(
            Options {
                max_elements: Some(201),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Options {
                max_depth: Some(0),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Options {
                label_contains: Some(String::new()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn enforces_serialized_output_limit_and_exact_element_boundary() {
        let tree = serde_json::json!({"elements":(0..200).map(|_| serde_json::json!({
            "label":"x".repeat(512),"value":"x".repeat(512),"identifier":"x".repeat(512),
            "AXLabel":"x".repeat(512),"AXValue":"x".repeat(512),"AXIdentifier":"x".repeat(512)
        })).collect::<Vec<_>>()});
        let encoded = project(&tree, &Default::default());
        assert!(encoded.len() <= 256 * 1024);
        let result: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(result["truncated"], true);
        let tree = serde_json::json!({"elements":[{"label":"Only"}]});
        let result: Value = serde_json::from_str(&project(
            &tree,
            &Options {
                max_elements: Some(1),
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(result["truncated"], false);
    }
}
