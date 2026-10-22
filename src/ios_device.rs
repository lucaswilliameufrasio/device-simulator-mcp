//! Physical iOS device operations. Simulator-only operations stay in `ios`.
use std::{io::Cursor, time::Duration};

use crate::process::CommandRunner;
use futures_util::StreamExt;

const MAX_ACCESSIBILITY_XML_BYTES: usize = 4 * 1024 * 1024;
const MAX_ACCESSIBILITY_NODES: usize = 4096;
const MAX_ACCESSIBILITY_DEPTH: usize = 32;
const MAX_WDA_RESPONSE_BYTES: usize = 6 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    identifier: String,
    wda: Option<reqwest::Url>,
}

impl Target {
    pub(crate) fn for_identifier(
        identifier: String,
        wda: Option<reqwest::Url>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !identifier.trim().is_empty() && identifier.len() <= 128,
            "physical iOS device identifier must contain between 1 and 128 characters"
        );
        Ok(Self { identifier, wda })
    }

    async fn details(&self, runner: &dyn CommandRunner) -> anyhow::Result<serde_json::Value> {
        let output = run_devicectl(
            runner,
            &[
                "device".to_owned(),
                "info".to_owned(),
                "details".to_owned(),
                "--device".to_owned(),
                self.identifier.clone(),
                "--json-output".to_owned(),
                "-".to_owned(),
            ],
        )
        .await?;
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    pub(crate) async fn automation_udid(
        &self,
        runner: &dyn CommandRunner,
    ) -> anyhow::Result<String> {
        let details = self.details(runner).await?;
        physical_hardware_udid(&details)
    }

    pub(crate) async fn status(&self, runner: &dyn CommandRunner) -> anyhow::Result<String> {
        let value = self.details(runner).await?;
        let name = value
            .get("result")
            .and_then(|result| result.get("deviceProperties"))
            .and_then(|properties| properties.get("name"))
            .or_else(|| value.pointer("/result/properties/name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("iPhone");
        if self.wda.is_some() {
            let response = self.get_wda("status").await?;
            anyhow::ensure!(
                response
                    .pointer("/value/ready")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                "WebDriverAgent is not ready"
            );
        }
        Ok(format!(
            "devicectl returned details for physical iOS device: {name}"
        ))
    }

    pub(crate) async fn capture(
        &self,
        name: &str,
        runner: &dyn CommandRunner,
    ) -> anyhow::Result<Vec<u8>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(format!("{name}.png"));
        run_devicectl(
            runner,
            &[
                "device".to_owned(),
                "capture".to_owned(),
                "screenshot".to_owned(),
                "--device".to_owned(),
                self.identifier.clone(),
                "--destination".to_owned(),
                path.display().to_string(),
            ],
        )
        .await?;
        Ok(tokio::fs::read(path).await?)
    }

    pub(crate) async fn rotate(
        &self,
        orientation: crate::platform::Orientation,
        runner: &dyn CommandRunner,
    ) -> anyhow::Result<String> {
        let value = match orientation {
            crate::platform::Orientation::Portrait => "portrait",
            crate::platform::Orientation::PortraitUpsideDown => "portraitUpsideDown",
            crate::platform::Orientation::LandscapeLeft => "landscapeLeft",
            crate::platform::Orientation::LandscapeRight => "landscapeRight",
        };
        run_devicectl(
            runner,
            &[
                "device".to_owned(),
                "orientation".to_owned(),
                "set".to_owned(),
                "--device".to_owned(),
                self.identifier.clone(),
                value.to_owned(),
            ],
        )
        .await?;
        Ok(format!("Physical iOS device orientation set to {value}"))
    }

    pub(crate) async fn tap(
        &self,
        x: f64,
        y: f64,
        runner: &dyn CommandRunner,
    ) -> anyhow::Result<String> {
        self.wda_url()?;
        let dimensions = self.screen_dimensions(runner).await?;
        self.post_wda(
            "wda/tap/0",
            serde_json::json!({
                "x": x * f64::from(dimensions.0 - 1),
                "y": y * f64::from(dimensions.1 - 1),
            }),
        )
        .await?;
        Ok("Tap submitted to WebDriverAgent".to_owned())
    }

    pub(crate) async fn swipe(
        &self,
        (x1, y1, x2, y2): (f64, f64, f64, f64),
        runner: &dyn CommandRunner,
    ) -> anyhow::Result<String> {
        self.wda_url()?;
        let dimensions = self.screen_dimensions(runner).await?;
        self.post_wda(
            "wda/dragfromtoforduration",
            serde_json::json!({
                "fromX": x1 * f64::from(dimensions.0 - 1),
                "fromY": y1 * f64::from(dimensions.1 - 1),
                "toX": x2 * f64::from(dimensions.0 - 1),
                "toY": y2 * f64::from(dimensions.1 - 1),
                "duration": 0.3,
            }),
        )
        .await?;
        Ok("Swipe submitted to WebDriverAgent".to_owned())
    }

    pub(crate) async fn type_text(&self, text: &str) -> anyhow::Result<String> {
        self.post_wda("wda/keys", serde_json::json!({"value": [text]}))
            .await?;
        Ok("Text submitted to WebDriverAgent".to_owned())
    }

    pub(crate) async fn accessibility(&self) -> anyhow::Result<serde_json::Value> {
        let response = self.get_wda("wda/accessibleSource").await?;
        let xml = response
            .get("value")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("WebDriverAgent returned no accessibility source"))?;
        parse_accessibility_xml(xml.as_bytes())
    }

    async fn screen_dimensions(&self, runner: &dyn CommandRunner) -> anyhow::Result<(u32, u32)> {
        let bytes = self.capture("device", runner).await?;
        let image = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()?
            .decode()?;
        Ok((image.width(), image.height()))
    }

    async fn post_wda(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let base = self.wda_url()?;
        let url = base.join(path)?;
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?
            .post(url)
            .json(&body)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "WebDriverAgent request failed"
        );
        let value = parse_wda_response(response).await?;
        anyhow::ensure!(
            value
                .get("value")
                .is_none_or(|result| result.get("error").is_none()),
            "WebDriverAgent rejected the request"
        );
        Ok(value)
    }

    async fn get_wda(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let base = self.wda_url()?;
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?
            .get(base.join(path)?)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "WebDriverAgent request failed"
        );
        parse_wda_response(response).await
    }

    fn wda_url(&self) -> anyhow::Result<&reqwest::Url> {
        self.wda
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this operation requires IOS_WDA_URL"))
    }
}

fn physical_hardware_udid(details: &serde_json::Value) -> anyhow::Result<String> {
    let udid = details
        .pointer("/result/hardwareProperties/udid")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "devicectl returned no hardware UDID; cannot safely target XCTest or iproxy"
            )
        })?;
    anyhow::ensure!(
        !udid.is_empty()
            && udid.len() <= 128
            && udid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "devicectl returned an invalid hardware UDID"
    );
    Ok(udid.to_owned())
}

pub(crate) async fn discover_devices(
    runner: &dyn CommandRunner,
) -> anyhow::Result<Vec<crate::device::DeviceTarget>> {
    let wda_url = std::env::var("IOS_WDA_URL")
        .ok()
        .map(|value| validate_wda_url(&value))
        .transpose()?;
    discover_devices_with_wda(runner, wda_url).await
}

pub(crate) async fn discover_devices_with_wda(
    runner: &dyn CommandRunner,
    wda_url: Option<reqwest::Url>,
) -> anyhow::Result<Vec<crate::device::DeviceTarget>> {
    let output = run_devicectl(
        runner,
        &[
            "list".to_owned(),
            "devices".to_owned(),
            "--json-output".to_owned(),
            "-".to_owned(),
        ],
    )
    .await?;
    let inventory: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let devices = inventory
        .pointer("/result/devices")
        .or_else(|| inventory.get("devices"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("invalid devicectl device inventory"))?;
    let mut targets = Vec::new();
    for device in devices {
        let udid = device
            .get("identifier")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                device
                    .pointer("/hardwareProperties/udid")
                    .and_then(serde_json::Value::as_str)
            });
        let Some(udid) = udid else {
            continue;
        };
        let name = device
            .pointer("/deviceProperties/name")
            .or_else(|| device.pointer("/hardwareProperties/name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("iPhone")
            .to_owned();
        let tunnel_state = device
            .pointer("/connectionProperties/tunnelState")
            .and_then(serde_json::Value::as_str);
        let pairing_state = device
            .pointer("/connectionProperties/pairingState")
            .and_then(serde_json::Value::as_str);
        let availability = match (tunnel_state, pairing_state) {
            (Some("connected"), Some("paired")) => "available",
            (Some("disconnected"), _) | (_, Some("unpaired")) => "unavailable",
            (Some(state), _) => state,
            (_, Some(state)) => state,
            _ => "unknown",
        };
        targets.push(crate::device::physical_target(
            udid.to_owned(),
            name,
            availability.to_owned(),
            wda_url.clone(),
        ));
    }
    Ok(targets)
}

async fn parse_wda_response(response: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_WDA_RESPONSE_BYTES,
            "WebDriverAgent response exceeded the byte limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn validate_wda_url(value: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    anyhow::ensure!(
        url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "IOS_WDA_URL must be a loopback HTTP URL without credentials, query or fragment"
    );
    Ok(url)
}

async fn run_devicectl(
    runner: &dyn CommandRunner,
    arguments: &[String],
) -> anyhow::Result<crate::process::CommandOutput> {
    let mut command_arguments = vec!["devicectl".to_owned()];
    command_arguments.extend_from_slice(arguments);
    let output = runner.run("xcrun", &command_arguments).await?;
    anyhow::ensure!(
        output.success,
        "devicectl failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

fn parse_accessibility_xml(xml: &[u8]) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(
        !xml.is_empty(),
        "WebDriverAgent accessibility source was empty"
    );
    anyhow::ensure!(
        xml.len() <= MAX_ACCESSIBILITY_XML_BYTES,
        "WebDriverAgent accessibility source exceeded the byte limit"
    );
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut nodes = Vec::<serde_json::Value>::new();
    let mut roots = Vec::new();
    let mut node_count = 0;
    let mut ignored_depth = 0;
    let mut truncated = false;
    loop {
        match reader.read_event()? {
            quick_xml::events::Event::Start(element) if element.name().as_ref() != "AppiumAUT" => {
                if ignored_depth > 0 {
                    ignored_depth += 1;
                } else if node_count >= MAX_ACCESSIBILITY_NODES
                    || nodes.len() >= MAX_ACCESSIBILITY_DEPTH
                {
                    truncated = true;
                    ignored_depth = 1;
                } else {
                    nodes.push(parse_accessibility_node(&element)?);
                    node_count += 1;
                }
            }
            quick_xml::events::Event::Empty(element) if element.name().as_ref() != "AppiumAUT" => {
                if ignored_depth == 0 {
                    if node_count >= MAX_ACCESSIBILITY_NODES
                        || nodes.len() >= MAX_ACCESSIBILITY_DEPTH
                    {
                        truncated = true;
                    } else {
                        append_accessibility_node(
                            parse_accessibility_node(&element)?,
                            &mut nodes,
                            &mut roots,
                        );
                        node_count += 1;
                    }
                }
            }
            quick_xml::events::Event::End(element) if element.name().as_ref() != "AppiumAUT" => {
                if ignored_depth > 0 {
                    ignored_depth -= 1;
                } else {
                    let node = nodes.pop().ok_or_else(|| {
                        anyhow::anyhow!("invalid WebDriverAgent accessibility source")
                    })?;
                    append_accessibility_node(node, &mut nodes, &mut roots);
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    anyhow::ensure!(
        nodes.is_empty() && ignored_depth == 0,
        "WebDriverAgent accessibility source was incomplete"
    );
    anyhow::ensure!(
        !roots.is_empty(),
        "WebDriverAgent accessibility source contained no nodes"
    );
    Ok(serde_json::json!({"elements": roots, "truncated": truncated}))
}

fn parse_accessibility_node(
    element: &quick_xml::events::BytesStart<'_>,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::{Map, Value};

    let mut node = Map::new();
    let mut attributes = Vec::new();
    for attribute in element.attributes().with_checks(true) {
        let attribute = attribute?;
        let key = attribute.key.as_ref();
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::default())?
            .into_owned();
        attributes.push((key.to_owned(), value));
    }
    let secure_text_field = element.name().as_ref().contains("SecureTextField")
        || attributes
            .iter()
            .any(|(key, value)| key == "secure" && value == "true");
    if secure_text_field {
        node.insert("password".to_owned(), Value::Bool(true));
    }
    for (key, value) in attributes {
        match key.as_str() {
            "label" => {
                node.insert("label".to_owned(), Value::String(value));
            }
            "name" => {
                node.entry("label".to_owned())
                    .or_insert_with(|| Value::String(value.clone()));
                node.insert("identifier".to_owned(), Value::String(value));
            }
            "identifier" => {
                node.insert("identifier".to_owned(), Value::String(value));
            }
            "type" => {
                node.insert("role".to_owned(), Value::String(value));
            }
            "value" if !secure_text_field && value.len() <= 512 => {
                node.insert("value".to_owned(), Value::String(value));
            }
            "enabled" | "visible" => {
                if let Ok(value) = value.parse::<bool>() {
                    node.insert(key.to_owned(), Value::Bool(value));
                }
            }
            "x" | "y" | "width" | "height" => {
                if let Ok(value) = value.parse::<f64>()
                    && value.is_finite()
                    && value.abs() <= 16_384.0
                {
                    node.entry("frame")
                        .or_insert_with(|| Value::Object(Map::new()))
                        .as_object_mut()
                        .expect("frame is an object")
                        .insert(key.to_owned(), serde_json::json!(value));
                }
            }
            _ => {}
        }
    }
    Ok(Value::Object(node))
}

fn append_accessibility_node(
    mut node: serde_json::Value,
    stack: &mut [serde_json::Value],
    roots: &mut Vec<serde_json::Value>,
) {
    if let Some(parent) = stack.last_mut() {
        let children = parent
            .as_object_mut()
            .expect("accessibility node is an object")
            .entry("children")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .expect("accessibility children is an array");
        children.push(node);
    } else {
        roots.push(std::mem::take(&mut node));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DetailsRunner;

    #[async_trait::async_trait]
    impl CommandRunner for DetailsRunner {
        async fn run_with_timeout(
            &self,
            command: &str,
            arguments: &[String],
            _command_timeout: Duration,
        ) -> anyhow::Result<crate::process::CommandOutput> {
            assert_eq!(command, "xcrun");
            assert_eq!(arguments[0], "devicectl");
            assert_eq!(arguments[1..5], ["device", "info", "details", "--device"]);
            assert_eq!(arguments[5], "device-123");
            Ok(crate::process::CommandOutput {
                success: true,
                status: "success".to_owned(),
                stdout: br#"{"result":{"hardwareProperties":{"udid":"00008110-001234567890001E"},"deviceProperties":{"name":"Test iPhone"}}}"#.to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn accepts_only_loopback_webdriveragent_urls() {
        assert!(validate_wda_url("http://127.0.0.1:8100").is_ok());
        assert!(validate_wda_url("http://localhost:8100/wda").is_ok());
        assert!(validate_wda_url("http://example.com:8100").is_err());
        assert!(validate_wda_url("https://127.0.0.1:8100").is_err());
        assert!(validate_wda_url("http://user@127.0.0.1:8100").is_err());
    }

    #[tokio::test]
    async fn queries_details_for_only_the_explicit_physical_device() {
        let target = Target {
            identifier: "device-123".to_owned(),
            wda: None,
        };
        assert_eq!(
            target.status(&DetailsRunner).await.unwrap(),
            "devicectl returned details for physical iOS device: Test iPhone"
        );
    }

    #[tokio::test]
    async fn resolves_the_hardware_udid_without_using_the_coredevice_identifier_for_automation() {
        let target = Target::for_identifier("device-123".to_owned(), None).unwrap();
        assert_eq!(
            target.automation_udid(&DetailsRunner).await.unwrap(),
            "00008110-001234567890001E"
        );
    }

    #[test]
    fn refuses_missing_or_invalid_hardware_udids_instead_of_guessing() {
        assert!(
            physical_hardware_udid(&serde_json::json!({"result":{"identifier":"coredevice-id"}}))
                .is_err()
        );
        assert!(
            physical_hardware_udid(
                &serde_json::json!({"result":{"hardwareProperties":{"udid":"invalid,id=other"}}})
            )
            .is_err()
        );
    }

    #[test]
    fn parses_webdriveragent_accessibility_xml_into_shared_shape() {
        let tree = parse_accessibility_xml(
            br#"<AppiumAUT><XCUIElementTypeApplication name="Demo"><XCUIElementTypeButton label="Continue" name="continue" x="10" y="20" width="100" height="40" enabled="true"/></XCUIElementTypeApplication></AppiumAUT>"#,
        )
        .unwrap();
        let projection = crate::accessibility::project(&tree, &Default::default());
        let projection: serde_json::Value = serde_json::from_str(&projection).unwrap();
        assert_eq!(projection["elements"][1]["label"], "Continue");
        assert_eq!(projection["elements"][1]["identifier"], "continue");
        assert_eq!(projection["elements"][1]["frame"]["width"], 100.0);
    }

    #[test]
    fn redacts_secure_text_field_values() {
        let tree = parse_accessibility_xml(
            br#"<AppiumAUT><XCUIElementTypeSecureTextField label="Password" value="private-value"/></AppiumAUT>"#,
        )
        .unwrap();
        let projection = crate::accessibility::project(&tree, &Default::default());
        let projection: serde_json::Value = serde_json::from_str(&projection).unwrap();
        assert_eq!(projection["elements"][0]["password"], true);
        assert!(projection["elements"][0].get("value").is_none());
    }
}
