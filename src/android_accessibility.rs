use quick_xml::{Reader, events::Event};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const MAX_XML_BYTES: usize = 4 * 1024 * 1024;
const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 32;

#[derive(Default)]
struct Node {
    values: Map<String, Value>,
    children: Vec<Value>,
}

impl Node {
    fn into_value(self) -> Value {
        let mut values = self.values;
        if !self.children.is_empty() {
            values.insert("children".to_owned(), Value::Array(self.children));
        }
        Value::Object(values)
    }
}

pub(crate) fn parse(xml: &[u8]) -> anyhow::Result<Value> {
    anyhow::ensure!(!xml.is_empty(), "Android UI hierarchy was empty");
    anyhow::ensure!(
        xml.len() <= MAX_XML_BYTES,
        "Android UI hierarchy exceeded the byte limit"
    );

    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut stack = Vec::<Node>::new();
    let mut roots = Vec::new();
    let mut node_count = 0;
    let mut ignored_depth = 0;
    let mut truncated = false;

    loop {
        match reader.read_event()? {
            Event::Start(element) if element.name().as_ref() == "node" => {
                if ignored_depth > 0 {
                    ignored_depth += 1;
                } else if node_count >= MAX_NODES || stack.len() >= MAX_DEPTH {
                    truncated = true;
                    ignored_depth = 1;
                } else {
                    stack.push(parse_node(&element)?);
                    node_count += 1;
                }
            }
            Event::Empty(element) if element.name().as_ref() == "node" => {
                if ignored_depth > 0 {
                    continue;
                }
                if node_count >= MAX_NODES || stack.len() >= MAX_DEPTH {
                    truncated = true;
                } else {
                    append_node(parse_node(&element)?, &mut stack, &mut roots);
                    node_count += 1;
                }
            }
            Event::End(element) if element.name().as_ref() == "node" => {
                if ignored_depth > 0 {
                    ignored_depth -= 1;
                } else {
                    let node = stack
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid Android UI hierarchy"))?;
                    append_value(node.into_value(), &mut stack, &mut roots);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    anyhow::ensure!(
        stack.is_empty() && ignored_depth == 0,
        "Android UI hierarchy was incomplete"
    );
    anyhow::ensure!(!roots.is_empty(), "Android UI hierarchy contained no nodes");
    Ok(json!({"elements": roots, "truncated": truncated}))
}

fn parse_node(element: &quick_xml::events::BytesStart<'_>) -> anyhow::Result<Node> {
    let mut attributes = BTreeMap::<String, String>::new();
    for attribute in element.attributes().with_checks(true) {
        let attribute = attribute?;
        let key = attribute.key.as_ref();
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::default())?
            .into_owned();
        attributes.insert(key.to_owned(), value);
    }

    let password = attributes
        .get("password")
        .is_some_and(|value| value == "true");
    let text = attributes.get("text").filter(|value| !value.is_empty());
    let description = attributes
        .get("content-desc")
        .filter(|value| !value.is_empty());
    let mut values = Map::new();

    if !password {
        if let Some(label) = text.or(description) {
            values.insert("label".to_owned(), Value::String(label.clone()));
        }
        if let Some(description) = description {
            values.insert(
                "content_description".to_owned(),
                Value::String(description.clone()),
            );
        }
    }
    if !password {
        if let Some(value) = text {
            values.insert("value".to_owned(), Value::String(value.clone()));
        }
    } else {
        values.insert("password".to_owned(), Value::Bool(true));
    }
    if let Some(identifier) = attributes
        .get("resource-id")
        .filter(|value| !value.is_empty())
    {
        values.insert("identifier".to_owned(), Value::String(identifier.clone()));
    }
    if let Some(role) = attributes.get("class") {
        values.insert("role".to_owned(), Value::String(role.clone()));
    }
    if let Some(bounds) = attributes
        .get("bounds")
        .and_then(|bounds| parse_bounds(bounds))
    {
        values.insert("frame".to_owned(), bounds);
    }
    for (source, target) in [
        ("enabled", "enabled"),
        ("checkable", "checkable"),
        ("checked", "checked"),
        ("clickable", "clickable"),
        ("focusable", "focusable"),
        ("focused", "focused"),
        ("scrollable", "scrollable"),
        ("long-clickable", "long_clickable"),
        ("selected", "selected"),
    ] {
        if let Some(value) = attributes
            .get(source)
            .and_then(|value| match value.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            })
        {
            values.insert(target.to_owned(), Value::Bool(value));
        }
    }

    Ok(Node {
        values,
        children: Vec::new(),
    })
}

fn parse_bounds(bounds: &str) -> Option<Value> {
    let bounds = bounds.strip_prefix('[')?;
    let (left, bounds) = bounds.split_once(',')?;
    let (top, bounds) = bounds.split_once(']')?;
    let bounds = bounds.strip_prefix('[')?;
    let (right, bounds) = bounds.split_once(',')?;
    let bottom = bounds.strip_suffix(']')?;
    let (left, top, right, bottom) = (
        left.parse::<i64>().ok()?,
        top.parse::<i64>().ok()?,
        right.parse::<i64>().ok()?,
        bottom.parse::<i64>().ok()?,
    );
    if [left, top, right, bottom]
        .into_iter()
        .any(|coordinate| !(-16_384..=16_384).contains(&coordinate))
        || right < left
        || bottom < top
    {
        return None;
    }
    Some(json!({
        "x": left,
        "y": top,
        "width": right - left,
        "height": bottom - top
    }))
}

fn append_node(node: Node, stack: &mut [Node], roots: &mut Vec<Value>) {
    append_value(node.into_value(), stack, roots);
}

fn append_value(value: Value, stack: &mut [Node], roots: &mut Vec<Value>) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(value);
    } else {
        roots.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_android_nodes_to_the_shared_accessibility_shape_and_redacts_passwords() {
        let tree = parse(
            br#"<?xml version="1.0"?><hierarchy><node class="android.widget.FrameLayout" bounds="[0,0][1080,2400]" enabled="true"><node text="Continue &amp; pay" content-desc="Primary action" resource-id="app:id/continue" class="android.widget.Button" bounds="[10,20][310,90]" clickable="true" enabled="true"/><node text="secret" content-desc="Password" password="true" class="android.widget.EditText" bounds="[10,100][400,170]"/></node></hierarchy>"#,
        )
        .unwrap();
        let output = crate::accessibility::project(&tree, &Default::default());
        let output: Value = serde_json::from_str(&output).unwrap();

        assert_eq!(output["elements"][1]["label"], "Continue & pay");
        assert_eq!(
            output["elements"][1]["content_description"],
            "Primary action"
        );
        assert_eq!(output["elements"][1]["identifier"], "app:id/continue");
        assert_eq!(output["elements"][1]["role"], "android.widget.Button");
        assert_eq!(output["elements"][1]["frame"]["width"], 300);
        assert_eq!(output["elements"][2].get("label"), None);
        assert_eq!(output["elements"][2].get("content_description"), None);
        assert_eq!(output["elements"][2].get("value"), None);
        assert_eq!(output["elements"][2]["password"], true);
        let filtered = crate::accessibility::project(
            &tree,
            &crate::accessibility::Options {
                label_contains: Some("Primary".to_owned()),
                ..Default::default()
            },
        );
        let filtered: Value = serde_json::from_str(&filtered).unwrap();
        assert_eq!(filtered["elements"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn rejects_malformed_or_oversized_hierarchies() {
        assert!(parse(b"not xml").is_err());
        assert!(parse(&vec![b'x'; MAX_XML_BYTES + 1]).is_err());
    }

    #[test]
    fn truncates_hierarchies_at_the_node_limit() {
        let nodes = (0..=MAX_NODES)
            .map(|_| "<node text=\"item\"/>")
            .collect::<String>();
        let xml = format!("<hierarchy>{nodes}</hierarchy>");
        let tree = parse(xml.as_bytes()).unwrap();
        assert_eq!(tree["truncated"], true);
        assert_eq!(tree["elements"].as_array().unwrap().len(), MAX_NODES);
    }
}
