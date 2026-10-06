use crate::{
    accessibility, android_grpc, frame_cache, ios, ios_device, ios_lifecycle, observation, process,
    session, visual_wait,
};

use std::{sync::Arc, time::Duration};

use crate::platform::{
    Orientation, Platform, capture_device, configured_platform, inspect_android_accessibility,
    rotate_device, run_serve_sim, start_device, status_device, tap_device, type_on_device,
};
#[cfg(test)]
use crate::{
    platform::{
        actionable_command_error, actionable_device_failure, command_output, escape_android_text,
        gesture_payload, parse_display_size, parse_platform,
    },
    process::CommandOutput,
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use process::{CommandRunner, ProcessCommandRunner};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
};

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
struct CaptureParameters {
    #[schemars(description = "Screenshot name without the .png extension")]
    name: Option<String>,
    #[serde(flatten)]
    options: observation::ImageOptions,
    #[serde(default)]
    #[schemars(
        description = "Use serve-sim's latest JPEG frame (may be replayed); iOS persistent backend only. Defaults to a new platform screenshot."
    )]
    latest_frame: bool,
    /// Reuse a locally acquired frame no older than this (0..=5000 ms).
    /// Default 0 forces acquisition. Backend replay age remains unknown.
    max_age_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TapParameters {
    #[schemars(description = "Normalized horizontal coordinate from 0 to 1")]
    x: f64,
    #[schemars(description = "Normalized vertical coordinate from 0 to 1")]
    y: f64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SwipeParameters {
    #[schemars(description = "Normalized starting horizontal coordinate")]
    x1: f64,
    #[schemars(description = "Normalized starting vertical coordinate")]
    y1: f64,
    #[schemars(description = "Normalized ending horizontal coordinate")]
    x2: f64,
    #[schemars(description = "Normalized ending vertical coordinate")]
    y2: f64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TypeParameters {
    #[schemars(description = "Text to type into the focused control")]
    text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RotateParameters {
    #[schemars(
        description = "Target orientation: portrait, portrait_upside_down, landscape_left, or landscape_right"
    )]
    orientation: Orientation,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct MultiTouchParameters {
    /// Ordered frames submitted as simultaneous Android Emulator contacts.
    frames: Vec<android_grpc::MultiTouchFrame>,
    /// Total operation deadline in milliseconds (1..=20000), default 10000.
    timeout_ms: Option<u64>,
}

impl MultiTouchParameters {
    fn validate(&self) -> anyhow::Result<()> {
        android_grpc::validate_multitouch_frames(&self.frames)?;
        let timeout_ms = self.timeout_ms.unwrap_or(10_000);
        anyhow::ensure!(
            (1..=20_000).contains(&timeout_ms),
            "multitouch timeout_ms must be between 1 and 20000"
        );
        let frame_delay_ms = self
            .frames
            .iter()
            .take(self.frames.len().saturating_sub(1))
            .map(|frame| frame.delay_ms.unwrap_or(30))
            .sum::<u64>();
        anyhow::ensure!(
            frame_delay_ms < timeout_ms,
            "multitouch timeout_ms must exceed the configured inter-frame delays"
        );
        Ok(())
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Tap { x: f64, y: f64 },
    Swipe { x1: f64, y1: f64, x2: f64, y2: f64 },
    Type { text: String },
}

impl Action {
    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Tap { x, y } => validate_coordinates(&[*x, *y]),
            Self::Swipe { x1, y1, x2, y2 } => validate_coordinates(&[*x1, *y1, *x2, *y2]),
            Self::Type { text } => validate_text(text),
        }
    }
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StepParameters {
    /// Up to 16 already-known actions. All arguments are checked before input.
    actions: Vec<Action>,
    /// Optional screenshot after the actions. Omit for no observation.
    capture: Option<CaptureParameters>,
    /// Total operation deadline including queue wait (1..=20000 ms), default 10000.
    timeout_ms: Option<u64>,
    /// Explicit bounded delay before capture (0..=1000 ms). Not an idle guarantee.
    settle_ms: Option<u64>,
    /// Fresh visual heuristic or exact bounded accessibility element selector.
    wait_condition: Option<visual_wait::Condition>,
    /// Return bounded iOS accessibility instead of a screenshot.
    #[serde(default)]
    accessibility: bool,
    /// Optional filters/limits for accessibility output; requires accessibility=true.
    accessibility_options: Option<accessibility::Options>,
}

#[derive(Clone)]
pub(crate) struct DeviceSimulatorMcp {
    runner: Arc<dyn CommandRunner>,
    session: Arc<session::Session>,
    ios: Arc<ios::Backend>,
    android: Arc<android_grpc::Backend>,
    lifecycle: Arc<ios_lifecycle::Lifecycle>,
    cache: Arc<tokio::sync::Mutex<frame_cache::Cache>>,
}

impl Default for DeviceSimulatorMcp {
    fn default() -> Self {
        Self {
            runner: Arc::new(ProcessCommandRunner),
            session: Arc::new(session::Session::default()),
            ios: Arc::new(ios::Backend::default()),
            android: Arc::new(android_grpc::Backend::default()),
            lifecycle: Arc::new(ios_lifecycle::Lifecycle::default()),
            cache: Default::default(),
        }
    }
}

#[tool_router]
impl DeviceSimulatorMcp {
    #[tool(
        description = "Discover the configured backend's supported observations, wait conditions and operation limits without device I/O. This does not probe runtime availability; use device_status/start to verify readiness."
    )]
    async fn device_capabilities(&self) -> CallToolResult {
        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        let physical = if matches!(platform, Platform::Ios) {
            match ios_device::Target::configured() {
                Ok(target) => target,
                Err(error) => return failure(error),
            }
        } else {
            None
        };
        let persistent = match platform {
            Platform::Ios if physical.is_some() => Ok(false),
            Platform::Ios => self.ios.enabled(),
            Platform::Android => self.android.enabled(),
        };
        let persistent = match persistent {
            Ok(enabled) => enabled,
            Err(error) => return failure(error),
        };
        let accessibility = if let Some(target) = &physical {
            target.has_wda()
        } else {
            match platform {
                Platform::Ios => persistent,
                Platform::Android => !persistent,
            }
        };
        let step_accessibility = matches!(platform, Platform::Ios) && accessibility;
        let backend = if physical.is_some() {
            "devicectl"
        } else {
            match (platform, persistent) {
                (Platform::Ios, true) => "serve-sim",
                (Platform::Ios, false) => "cli",
                (Platform::Android, true) => "grpc",
                (Platform::Android, false) => "adb",
            }
        };
        success(serde_json::json!({
            "platform":if matches!(platform,Platform::Ios) {"ios"} else {"android"},
            "backend":backend,
            "target":if physical.is_some() {"physical_device"} else {"simulator_or_emulator"},
            "input_backend":if physical.as_ref().is_some_and(ios_device::Target::has_wda) {"webdriveragent"} else if physical.is_some() {"unavailable"} else {"native"},
            "experimental":matches!(platform,Platform::Android) && persistent,
            "availability_probed":false,
            "observations":{"fresh_screenshot":true,"latest_frame":matches!(platform, Platform::Ios) && persistent,"accessibility":accessibility,
                "step_accessibility":step_accessibility,
                "image_formats":["png","jpeg"],"crop":true,"resize":true,"cache_requires_explicit_target":true},
            "controls":{"orientation":matches!(platform,Platform::Ios) || !persistent,
                "multitouch":matches!(platform,Platform::Android) && persistent},
            "wait_conditions":{"visual_change":true,"visual_stability":true,"element_present":step_accessibility},
            "application_render_acknowledged":false,
            "limits":{"max_actions":16,"max_pending_operations":8,"max_step_timeout_ms":20000,
                "max_cache_age_ms":5000,"max_image_dimension":4096,"max_ax_elements":200,"max_ax_depth":16},
        }).to_string())
    }

    #[tool(description = "Start inspection for the configured device")]
    async fn device_start(&self) -> CallToolResult {
        self.session
            .execute("start", Duration::from_secs(60), async {
                self.cache.lock().await.clear();
                let platform = match configured_platform() {
                    Ok(platform) => platform,
                    Err(error) => return failure(error),
                };
                let result = if matches!(platform, Platform::Ios) {
                    match ios_device::Target::configured() {
                        Ok(Some(target)) => target.status(self.runner.as_ref()).await,
                        Ok(None) => match self.ios.enabled() {
                            Ok(true) => self.ios.status().await,
                            Ok(false) => self.lifecycle.start(self.runner.as_ref()).await,
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    }
                } else {
                    match self.android.enabled() {
                        Ok(true) => self.android.status().await,
                        Ok(false) => start_device(platform, self.runner.as_ref()).await,
                        Err(error) => Err(error),
                    }
                };
                match result {
                    Ok(message) => success(message),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(
        description = "Stop only MCP-owned iOS inspection resources or disconnect the persistent input socket. Externally managed streams and the Emulator are never stopped."
    )]
    async fn device_stop(&self) -> CallToolResult {
        self.session
            .execute("stop", Duration::from_secs(20), async {
                self.cache.lock().await.clear();
                let platform = match configured_platform() {
                    Ok(platform) => platform,
                    Err(error) => return failure(error),
                };
                match platform {
            Platform::Ios => match ios_device::Target::configured() {
                Ok(Some(_)) => success("Physical iPhone and external automation services were left running".to_owned()),
                Ok(None) => match self.stop_ios().await {
                    Ok(message) => success(message),
                    Err(error) => failure(error),
                },
                Err(error) => failure(error),
            },
            Platform::Android => success(
                "Android emulator lifecycle is managed externally; no emulator was stopped."
                    .to_owned(),
            ),
        }
            })
            .await
    }

    #[tool(description = "Show the configured device status")]
    async fn device_status(&self) -> CallToolResult {
        self.session
            .execute("status", Duration::from_secs(20), async {
                let platform = match configured_platform() {
                    Ok(platform) => platform,
                    Err(error) => return failure(error),
                };
                let result = if matches!(platform, Platform::Ios) {
                    match ios_device::Target::configured() {
                        Ok(Some(target)) => target.status(self.runner.as_ref()).await,
                        Ok(None) => match self.ios.enabled() {
                            Ok(true) => self.ios.status().await,
                            Ok(false) => status_device(platform, self.runner.as_ref()).await,
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    }
                } else {
                    match self.android.enabled() {
                        Ok(true) => self.android.status().await,
                        Ok(false) => status_device(platform, self.runner.as_ref()).await,
                        Err(error) => Err(error),
                    }
                };
                match result {
                    Ok(message) => success(message),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(description = "Capture the current device display as a PNG image")]
    async fn device_capture(
        &self,
        Parameters(parameters): Parameters<CaptureParameters>,
    ) -> CallToolResult {
        if let Err(error) = validate_capture(&parameters) {
            return failure(error);
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        self.session
            .execute("capture", Duration::from_secs(20), async {
                match self.capture(platform, parameters).await {
                    Ok(content) => CallToolResult::success(content),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(description = "Tap a normalized coordinate on the configured device")]
    async fn device_tap(
        &self,
        Parameters(parameters): Parameters<TapParameters>,
    ) -> CallToolResult {
        if let Err(error) = validate_coordinates(&[parameters.x, parameters.y]) {
            return failure(error);
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        self.session
            .execute("tap", Duration::from_secs(20), async {
                match self.tap(platform, parameters.x, parameters.y).await {
                    Ok(message) => success(message),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(description = "Swipe between normalized coordinates on the device")]
    async fn device_swipe(
        &self,
        Parameters(parameters): Parameters<SwipeParameters>,
    ) -> CallToolResult {
        if let Err(error) =
            validate_coordinates(&[parameters.x1, parameters.y1, parameters.x2, parameters.y2])
        {
            return failure(error);
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        self.session
            .execute("swipe", Duration::from_secs(20), async {
                match self.swipe(platform, parameters).await {
                    Ok(message) => success(message),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(description = "Type text into the currently focused device control")]
    async fn device_type(
        &self,
        Parameters(parameters): Parameters<TypeParameters>,
    ) -> CallToolResult {
        if let Err(error) = validate_text(&parameters.text) {
            return failure(error);
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        self.session
            .execute("type", Duration::from_secs(20), async {
                match self.type_text(platform, &parameters.text).await {
                    Ok(message) => success(message),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(
        description = "Set iOS device or Android Emulator orientation to portrait, portrait_upside_down, landscape_left, or landscape_right. Physical iOS requires a supported device; Android ADB mode disables automatic rotation and experimental Android gRPC does not support this operation."
    )]
    async fn device_rotate(
        &self,
        Parameters(parameters): Parameters<RotateParameters>,
    ) -> CallToolResult {
        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        self.session
            .execute("rotate", Duration::from_secs(20), async {
                self.cache.lock().await.clear();
                match platform {
                    Platform::Ios => {
                        match ios_device::Target::configured() {
                            Ok(Some(target)) => match target.rotate(parameters.orientation, self.runner.as_ref()).await {
                                Ok(message) => return success(message),
                                Err(error) => return failure(error),
                            },
                            Ok(None) => {}
                            Err(error) => return failure(error),
                        }
                        match self.ios.enabled() {
                            Ok(true) => match self.ios.rotate(parameters.orientation).await {
                                Ok(message) => return success(message),
                                Err(error) => return failure(error),
                            },
                            Ok(false) => {
                                if let Err(error) =
                                    self.lifecycle.ensure_running(self.runner.as_ref()).await
                                {
                                    return failure(error);
                                }
                            }
                            Err(error) => return failure(error),
                        }
                    }
                    Platform::Android => match self.android.enabled() {
                        Ok(true) => {
                            return failure(anyhow::anyhow!(
                                "device_rotate is not supported by the experimental Android gRPC backend; select the ADB backend"
                            ));
                        }
                        Ok(false) => {}
                        Err(error) => return failure(error),
                    },
                }
                match rotate_device(platform, parameters.orientation, self.runner.as_ref()).await {
                    Ok(message) => success(message),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(
        description = "Submit up to 32 validated simultaneous Android Emulator contact frames through the experimental authenticated gRPC backend. Each frame can contain up to 5 contacts with down/move/up phases; all contacts must be released in the final frame. Not supported by ADB."
    )]
    async fn device_multitouch(
        &self,
        Parameters(parameters): Parameters<MultiTouchParameters>,
    ) -> CallToolResult {
        if let Err(error) = parameters.validate() {
            return failure(error);
        }
        if !matches!(configured_platform(), Ok(Platform::Android)) {
            return failure(anyhow::anyhow!(
                "device_multitouch currently requires the Android platform"
            ));
        }
        let timeout = Duration::from_millis(parameters.timeout_ms.unwrap_or(10_000));
        self.session
            .execute("multitouch", timeout + Duration::from_millis(50), async {
                self.cache.lock().await.clear();
                match self.android.enabled() {
                    Ok(true) => match self.android.multitouch(&parameters.frames).await {
                        Ok(message) => success(message),
                        Err(error) => failure(error),
                    },
                    Ok(false) => failure(anyhow::anyhow!(
                        "device_multitouch requires the experimental Android gRPC backend; ADB cannot submit simultaneous contacts"
                    )),
                    Err(error) => failure(error),
                }
            })
            .await
    }

    #[tool(
        description = "Repair iOS Simulator input when keyboard or touch input stops working. This restarts SpringBoard and closes running apps."
    )]
    async fn device_repair_input(&self) -> CallToolResult {
        self.session
            .execute("repair", Duration::from_secs(20), async {
                self.cache.lock().await.clear();
                let platform = match configured_platform() {
                    Ok(platform) => platform,
                    Err(error) => return failure(error),
                };
                match platform {
                    Platform::Ios => {
                        match ios_device::Target::configured() {
                            Ok(Some(_)) => {
                                return failure(anyhow::anyhow!(
                                    "device_repair_input is only supported for iOS Simulators"
                                ));
                            }
                            Ok(None) => {}
                            Err(error) => return failure(error),
                        }
                        match run_serve_sim(self.runner.as_ref(), &["repair-input"]).await {
                            Ok(message) => success(message),
                            Err(error) => failure(error),
                        }
                    }
                    Platform::Android => failure(anyhow::anyhow!(
                        "device_repair_input is only supported for iOS Simulators"
                    )),
                }
            })
            .await
    }

    #[tool(
        description = "Execute up to 16 known actions in order and optionally capture the resulting screen in one call. Never batch actions that depend on an unknown screen. Reports partial completion; inputs are never automatically retried."
    )]
    async fn device_step(
        &self,
        Parameters(parameters): Parameters<StepParameters>,
    ) -> CallToolResult {
        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        if let Err(error) = validate_step(&parameters) {
            return failure(error);
        }
        let physical_target = if matches!(platform, Platform::Ios) {
            match ios_device::Target::configured() {
                Ok(target) => target,
                Err(error) => return failure(error),
            }
        } else {
            None
        };
        let physical_wda = physical_target
            .as_ref()
            .is_some_and(ios_device::Target::has_wda);
        if (parameters.accessibility
            || matches!(
                parameters.wait_condition,
                Some(visual_wait::Condition::ElementPresent { .. })
            ))
            && !physical_wda
            && (!matches!(platform, Platform::Ios) || !matches!(self.ios.enabled(), Ok(true)))
        {
            return failure(anyhow::anyhow!(
                "step accessibility/element wait requires persistent iOS; no actions were submitted"
            ));
        }
        if physical_target.as_ref().is_some_and(|target| {
            (!parameters.actions.is_empty()
                || parameters.accessibility
                || matches!(
                    parameters.wait_condition,
                    Some(visual_wait::Condition::ElementPresent { .. })
                ))
                && !target.has_wda()
        }) {
            return failure(anyhow::anyhow!(
                "physical iPhone input/accessibility requires IOS_WDA_URL; no actions were submitted"
            ));
        }
        if parameters
            .capture
            .as_ref()
            .is_some_and(|capture| capture.max_age_ms.unwrap_or(0) > 0)
            && let Err(error) = cache_target(platform)
        {
            return failure(error);
        }
        if matches!(platform, Platform::Ios) {
            match self.ios.enabled() {
                Ok(true) => {
                    for action in &parameters.actions {
                        if let Action::Type { text } = action
                            && let Err(error) = self.ios.validate_text(text)
                        {
                            return failure(error);
                        }
                    }
                }
                Ok(false) => {}
                Err(error) => return failure(error),
            }
        }
        if matches!(platform, Platform::Android) {
            match self.android.enabled() {
                Ok(true) => {
                    for action in &parameters.actions {
                        if let Action::Type { text } = action
                            && let Err(error) = android_grpc::validate_text(text)
                        {
                            return failure(error);
                        }
                    }
                }
                Ok(false) => {}
                Err(error) => return failure(error),
            }
        }
        if parameters
            .capture
            .as_ref()
            .is_some_and(|capture| capture.latest_frame)
            && (!matches!(platform, Platform::Ios) || !matches!(self.ios.enabled(), Ok(true)))
        {
            return failure(anyhow::anyhow!(
                "latest_frame requires persistent iOS; no actions were submitted"
            ));
        }
        let duration = Duration::from_millis(parameters.timeout_ms.unwrap_or(10_000));
        let capture_requested = parameters.capture.is_some();
        let visual_wait_requested = matches!(
            parameters.wait_condition,
            Some(
                visual_wait::Condition::VisualChange
                    | visual_wait::Condition::VisualStability { .. }
            )
        );
        let deadline = tokio::time::Instant::now() + duration;
        self.session.execute("step", duration + Duration::from_millis(50), async {
            let mut completed = 0;
            let baseline = if matches!(parameters.wait_condition, Some(visual_wait::Condition::VisualChange)) {
                match tokio::time::timeout_at(deadline, async {
                    let frame = self.acquire_frame(platform, &CaptureParameters::default()).await?;
                    visual_wait::signature(frame.bytes.as_ref().clone()).await
                }).await {
                    Ok(Ok(signature)) => Some(signature),
                    _ => return step_failure(0, "wait_baseline", false),
                }
            } else { None };
            for action in &parameters.actions {
                let result = tokio::time::timeout_at(deadline, async {
                    match action {
                        Action::Tap { x, y } => self.tap(platform, *x, *y).await,
                        Action::Swipe { x1, y1, x2, y2 } => self.swipe(platform,
                            SwipeParameters { x1: *x1, y1: *y1, x2: *x2, y2: *y2 }).await,
                        Action::Type { text } => self.type_text(platform, text).await,
                    }
                }).await;
                match result {
                    Ok(Ok(_)) => completed += 1,
                    Ok(Err(_)) => return step_failure(completed, "action", true),
                    Err(_) => return step_failure(completed, "deadline", true),
                }
            }
            let mut observation_stage = "observation";
            let observation = tokio::time::timeout_at(deadline, async {
                tokio::time::sleep(Duration::from_millis(parameters.settle_ms.unwrap_or(0))).await;
                let mut wait_accessibility = None;
                let wait_frame = if let Some(condition) = parameters.wait_condition {
                    if let visual_wait::Condition::ElementPresent {label,identifier} = condition {
                        observation_stage = "element_wait";
                        wait_accessibility = Some(self.wait_element(label.as_deref(),identifier.as_deref()).await?);
                        None
                    } else {
                        observation_stage = "visual_wait";
                        Some(self.wait_visual(platform, condition, baseline).await?)
                    }
                } else { None };
                observation_stage = "observation";
                if parameters.accessibility {
                    let snapshot = match wait_accessibility {
                        Some(snapshot) => snapshot,
                        None => self.accessibility_snapshot().await?,
                    };
                    return Ok(vec![ContentBlock::text(accessibility::project(&snapshot,
                        &parameters.accessibility_options.unwrap_or_default()))]);
                }
                match parameters.capture {
                    Some(capture) => self.capture_with_frame(platform, capture, wait_frame).await,
                    None => Ok(Vec::new()),
                }
            }).await;
            match observation {
                Ok(Ok(mut content)) => {
                    content.insert(0, ContentBlock::text(serde_json::json!({
                        "completed_actions": completed, "input_confirmation": "backend_submission_only",
                        "observation_source": if capture_requested && visual_wait_requested {
                            "visual_wait_sample"
                        } else if parameters.accessibility { "accessibility" } else { "requested_observation" },
                    }).to_string()));
                    CallToolResult::success(content)
                }
                Ok(Err(_)) => step_failure(completed, observation_stage, false),
                Err(_) => step_failure(completed, match observation_stage {
                    "visual_wait" => "visual_wait_deadline",
                    "element_wait" => "element_wait_deadline",
                    _ => "observation_deadline",
                }, false),
            }
        }).await
    }

    #[tool(
        description = "Inspect a bounded accessibility tree on iOS through persistent serve-sim or physical-device WebDriverAgent, or on Android through ADB UIAutomator. Android gRPC does not expose accessibility; no screenshot is collected."
    )]
    async fn device_inspect(
        &self,
        Parameters(options): Parameters<accessibility::Options>,
    ) -> CallToolResult {
        if let Err(error) = options.validate() {
            return failure(error);
        }
        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        self.session
            .execute("inspect", Duration::from_secs(20), async {
                let result = match platform {
                    Platform::Ios => match ios_device::Target::configured() {
                        Ok(Some(target)) => target.accessibility().await,
                        Ok(None) => match self.ios.enabled() {
                        Ok(true) => self.ios.accessibility().await,
                        Ok(false) => Err(anyhow::anyhow!(
                            "iOS accessibility inspection requires the persistent serve-sim backend"
                        )),
                        Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    },
                    Platform::Android => match self.android.enabled() {
                        Ok(true) => Err(anyhow::anyhow!(
                            "Android accessibility inspection is not supported by the experimental gRPC backend; select ADB"
                        )),
                        Ok(false) => inspect_android_accessibility(self.runner.as_ref()).await,
                        Err(error) => Err(error),
                    },
                };
                match result {
                    Ok(value) => success(accessibility::project(&value, &options)),
                    Err(error) => failure(error),
                }
            })
            .await
    }
}

#[rmcp::tool_handler(
    instructions = "Prefer device_step for already-known sequential actions with one final capture. Do not batch actions depending on unseen UI. Use device_multitouch only for a verified screen and the experimental authenticated Android gRPC backend; its contacts are simultaneous, not sequential. PNG fresh screenshots remain the default; choose JPEG/max_dimension for smaller observations, or latest_frame with persistent iOS when unknown source age is acceptable. Use device_rotate for explicit orientation changes. Never automatically retry an uncertain input. Persistent backends are opt-in and require externally provisioned local services."
)]
impl rmcp::ServerHandler for DeviceSimulatorMcp {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let cancellation = context.ct.clone();
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let router = Self::tool_router();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Ok(rmcp::model::CallToolResponse::Complete(failure(anyhow::anyhow!(
                "request cancelled; an in-flight input may have applied, do not retry blindly"
            )))),
            result = router.call(call) => result,
        }
    }
}

impl DeviceSimulatorMcp {
    async fn accessibility_snapshot(&self) -> anyhow::Result<serde_json::Value> {
        if let Some(target) = ios_device::Target::configured()? {
            return target.accessibility().await;
        }
        self.ios.accessibility().await
    }

    async fn stop_ios(&self) -> anyhow::Result<String> {
        if self.ios.enabled()? {
            self.ios.stop().await
        } else {
            Ok(self.lifecycle.stop().await)
        }
    }

    async fn tap(&self, platform: Platform, x: f64, y: f64) -> anyhow::Result<String> {
        self.cache.lock().await.clear();
        if matches!(platform, Platform::Ios)
            && let Some(target) = ios_device::Target::configured()?
        {
            return target.tap(x, y, self.runner.as_ref()).await;
        }
        if matches!(platform, Platform::Ios) && self.ios.enabled()? {
            self.ios.tap(x, y).await
        } else if matches!(platform, Platform::Android) && self.android.enabled()? {
            self.android.tap(x, y).await
        } else {
            if matches!(platform, Platform::Ios) {
                self.lifecycle.ensure_running(self.runner.as_ref()).await?;
            }
            tap_device(platform, x, y, self.runner.as_ref()).await
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.lifecycle.stop().await;
        let _ = self.ios.stop().await;
    }

    async fn swipe(&self, platform: Platform, points: SwipeParameters) -> anyhow::Result<String> {
        self.cache.lock().await.clear();
        if matches!(platform, Platform::Ios)
            && let Some(target) = ios_device::Target::configured()?
        {
            return target
                .swipe(
                    (points.x1, points.y1, points.x2, points.y2),
                    self.runner.as_ref(),
                )
                .await;
        }
        if matches!(platform, Platform::Ios) && self.ios.enabled()? {
            self.ios
                .swipe((points.x1, points.y1, points.x2, points.y2))
                .await
        } else if matches!(platform, Platform::Android) && self.android.enabled()? {
            self.android
                .swipe((points.x1, points.y1, points.x2, points.y2))
                .await
        } else {
            if matches!(platform, Platform::Ios) {
                self.lifecycle.ensure_running(self.runner.as_ref()).await?;
            }
            swipe_device(platform, points, self.runner.as_ref()).await
        }
    }

    async fn type_text(&self, platform: Platform, text: &str) -> anyhow::Result<String> {
        self.cache.lock().await.clear();
        if matches!(platform, Platform::Ios)
            && let Some(target) = ios_device::Target::configured()?
        {
            return target.type_text(text).await;
        }
        if matches!(platform, Platform::Ios) && self.ios.enabled()? {
            self.ios.type_text(text).await
        } else if matches!(platform, Platform::Android) && self.android.enabled()? {
            self.android.type_text(text).await
        } else {
            if matches!(platform, Platform::Ios) {
                self.lifecycle.ensure_running(self.runner.as_ref()).await?;
            }
            type_on_device(platform, text, self.runner.as_ref()).await
        }
    }

    async fn acquire_frame(
        &self,
        platform: Platform,
        parameters: &CaptureParameters,
    ) -> anyhow::Result<frame_cache::Frame> {
        let (bytes, description) = if parameters.latest_frame {
            anyhow::ensure!(
                matches!(platform, Platform::Ios) && self.ios.enabled()?,
                "latest_frame requires the persistent iOS backend"
            );
            (
                self.ios.latest_frame().await?,
                "Latest serve-sim frame; source age unknown, may be replayed".to_owned(),
            )
        } else {
            if matches!(platform, Platform::Ios)
                && let Some(target) = ios_device::Target::configured()?
            {
                (
                    target
                        .capture(
                            parameters.name.as_deref().unwrap_or("device"),
                            self.runner.as_ref(),
                        )
                        .await?,
                    "Captured physical iPhone via devicectl".to_owned(),
                )
            } else if matches!(platform, Platform::Android) && self.android.enabled()? {
                (
                    self.android.capture().await?,
                    "Captured Android Emulator via experimental gRPC".to_owned(),
                )
            } else {
                capture_device(
                    platform,
                    parameters.name.as_deref().unwrap_or("device"),
                    self.runner.as_ref(),
                )
                .await?
            }
        };
        Ok(frame_cache::Frame {
            bytes: Arc::new(bytes),
            description,
            received: std::time::Instant::now(),
            received_at: std::time::SystemTime::now(),
        })
    }

    async fn wait_visual(
        &self,
        platform: Platform,
        condition: visual_wait::Condition,
        baseline: Option<visual_wait::Signature>,
    ) -> anyhow::Result<frame_cache::Frame> {
        let mut tracker = visual_wait::Tracker::new(condition, baseline);
        for _ in 0..32 {
            let frame = self
                .acquire_frame(platform, &CaptureParameters::default())
                .await?;
            if tracker.observe(visual_wait::signature(frame.bytes.as_ref().clone()).await?) {
                return Ok(frame);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        anyhow::bail!("visual wait sample limit exceeded; application idle is not guaranteed")
    }

    async fn wait_element(
        &self,
        label: Option<&str>,
        identifier: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        for _ in 0..32 {
            let value = self.accessibility_snapshot().await?;
            let snapshot = compact_accessibility(value.clone());
            if element_matches(&snapshot, label, identifier)? {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        anyhow::bail!("element wait sample limit exceeded; bounded tree may be truncated")
    }

    async fn capture(
        &self,
        platform: Platform,
        parameters: CaptureParameters,
    ) -> anyhow::Result<Vec<ContentBlock>> {
        self.capture_with_frame(platform, parameters, None).await
    }

    async fn capture_with_frame(
        &self,
        platform: Platform,
        parameters: CaptureParameters,
        wait_frame: Option<frame_cache::Frame>,
    ) -> anyhow::Result<Vec<ContentBlock>> {
        if parameters.max_age_ms.unwrap_or(0) > 0 {
            cache_target(platform)?;
        }
        let key = format!(
            "{platform:?}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            parameters.latest_frame,
            std::env::var("IOS_SIMULATOR_UDID").unwrap_or_default(),
            std::env::var("DEVICE_IOS_TARGET").unwrap_or_default(),
            std::env::var("IOS_DEVICE_UDID").unwrap_or_default(),
            std::env::var("IOS_WDA_URL").unwrap_or_default(),
            std::env::var("ANDROID_SERIAL").unwrap_or_default(),
            std::env::var("DEVICE_ANDROID_BACKEND").unwrap_or_default(),
            std::env::var("DEVICE_IOS_BACKEND").unwrap_or_default(),
            parameters.name.as_deref().unwrap_or("device"),
            std::env::var("SERVE_SIM_URL").unwrap_or_default(),
            std::env::var("SERVE_SIM_HELPER_URL").unwrap_or_default(),
            std::env::var("ANDROID_GRPC_ENDPOINT").unwrap_or_default()
        );
        let cached = if wait_frame.is_some() {
            None
        } else {
            self.cache.lock().await.get(
                &key,
                Duration::from_millis(parameters.max_age_ms.unwrap_or(0)),
            )
        };
        let cache_hit = cached.is_some();
        let frame = match cached {
            Some(frame) => frame,
            None => {
                let frame = match wait_frame {
                    Some(frame) => frame,
                    None => self.acquire_frame(platform, &parameters).await?,
                };
                self.cache.lock().await.store(key, frame.clone());
                frame
            }
        };
        let received_at = frame
            .received_at
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        let processing_started = std::time::Instant::now();
        if parameters.options.is_default()
            && !parameters.latest_frame
            && parameters.max_age_ms.unwrap_or(0) == 0
        {
            return Ok(vec![
                ContentBlock::text(frame.description),
                ContentBlock::image(BASE64_STANDARD.encode(frame.bytes.as_ref()), "image/png"),
            ]);
        }
        let image = observation::prepare(frame.bytes.as_ref().clone(), parameters.options).await?;
        static NEXT_OBSERVATION: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        Ok(vec![ContentBlock::text(serde_json::json!({
            "description": frame.description, "image": image.metadata,
            "freshness": if parameters.latest_frame { "unknown_backend_cache_age" } else if cache_hit { "local_cache" } else { "on_demand_capture" },
            "cache_hit": cache_hit, "local_age_ms": frame.received.elapsed().as_millis(),
            "observation_id": NEXT_OBSERVATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            "received_at_unix_ms": received_at,
            "processing_ms": processing_started.elapsed().as_millis(),
        }).to_string()), ContentBlock::image(BASE64_STANDARD.encode(image.bytes), image.mime)])
    }
}

fn validate_text(text: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!text.is_empty(), "text must not be empty");
    anyhow::ensure!(text.len() <= 4096, "text must not exceed 4096 bytes");
    Ok(())
}

fn cache_target(platform: Platform) -> anyhow::Result<()> {
    let explicit = match platform {
        Platform::Ios => ios_device::Target::configured()
            .map(|target| {
                target.is_some()
                    || std::env::var("IOS_SIMULATOR_UDID").is_ok_and(|value| !value.is_empty())
            })
            .unwrap_or(false),
        Platform::Android if std::env::var("DEVICE_ANDROID_BACKEND").as_deref() == Ok("grpc") => {
            std::env::var("ANDROID_GRPC_ENDPOINT").is_ok_and(|value| !value.is_empty())
        }
        Platform::Android => std::env::var("ANDROID_SERIAL").is_ok_and(|value| !value.is_empty()),
    };
    anyhow::ensure!(
        explicit,
        "cache reuse requires an explicit device target (IOS_SIMULATOR_UDID, ANDROID_SERIAL or gRPC endpoint)"
    );
    Ok(())
}

fn validate_capture(parameters: &CaptureParameters) -> anyhow::Result<()> {
    anyhow::ensure!(
        parameters.max_age_ms.unwrap_or(0) <= 5000,
        "max_age_ms must be between 0 and 5000"
    );
    validate_screenshot_name(parameters.name.as_deref().unwrap_or("device"))?;
    parameters.options.validate()
}

fn validate_step(parameters: &StepParameters) -> anyhow::Result<()> {
    if let Some(options) = &parameters.accessibility_options {
        anyhow::ensure!(
            parameters.accessibility,
            "accessibility_options requires accessibility=true"
        );
        options.validate()?;
    }
    anyhow::ensure!(
        parameters.actions.len() <= 16,
        "a step supports at most 16 actions"
    );
    anyhow::ensure!(
        !parameters.actions.is_empty() || parameters.capture.is_some() || parameters.accessibility,
        "a step needs actions or a capture"
    );
    anyhow::ensure!(
        !parameters.accessibility || parameters.capture.is_none(),
        "choose either capture or accessibility observation"
    );
    if let Some(condition) = &parameters.wait_condition {
        condition.validate()?;
        if !matches!(condition, visual_wait::Condition::ElementPresent { .. }) {
            anyhow::ensure!(
                parameters.capture.as_ref().is_none_or(
                    |capture| !capture.latest_frame && capture.max_age_ms.unwrap_or(0) == 0
                ),
                "visual waits require fresh final captures, not latest_frame or cache reuse"
            );
        }
    }
    anyhow::ensure!(
        (1..=20_000).contains(&parameters.timeout_ms.unwrap_or(10_000)),
        "timeout_ms must be between 1 and 20000"
    );
    anyhow::ensure!(
        parameters.settle_ms.unwrap_or(0) <= 1000,
        "settle_ms must not exceed 1000"
    );
    for action in &parameters.actions {
        action.validate()?;
    }
    if let Some(capture) = &parameters.capture {
        validate_capture(capture)?;
    }
    Ok(())
}

fn step_failure(completed: usize, stage: &str, uncertain: bool) -> CallToolResult {
    failure(anyhow::anyhow!(
        serde_json::json!({
            "completed_actions": completed, "failure_stage": stage,
            "current_action_may_have_applied": uncertain,
            "automatic_retry": false,
            "message": "step stopped; previously submitted actions were not rolled back",
        })
        .to_string()
    ))
}

fn compact_accessibility(value: serde_json::Value) -> String {
    accessibility::project(&value, &Default::default())
}

fn element_matches(
    snapshot: &str,
    label: Option<&str>,
    identifier: Option<&str>,
) -> anyhow::Result<bool> {
    let value: serde_json::Value = serde_json::from_str(snapshot)?;
    Ok(value["elements"].as_array().is_some_and(|elements| {
        elements.iter().any(|element| {
            label.is_none_or(|label| {
                ["label", "AXLabel"]
                    .iter()
                    .any(|key| element[*key].as_str() == Some(label))
            }) && identifier.is_none_or(|identifier| {
                ["identifier", "AXIdentifier"]
                    .iter()
                    .any(|key| element[*key].as_str() == Some(identifier))
            })
        })
    }))
}

async fn swipe_device(
    platform: Platform,
    parameters: SwipeParameters,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    crate::platform::swipe_device(
        platform,
        (parameters.x1, parameters.y1, parameters.x2, parameters.y2),
        runner,
    )
    .await
}

fn validate_coordinates(coordinates: &[f64]) -> anyhow::Result<()> {
    if coordinates
        .iter()
        .any(|coordinate| !coordinate.is_finite() || !(0.0..=1.0).contains(coordinate))
    {
        return Err(anyhow::anyhow!(
            "coordinates must be finite normalized numbers between 0 and 1"
        ));
    }
    Ok(())
}

fn validate_screenshot_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(char::is_control)
    {
        return Err(anyhow::anyhow!(
            "name must be a non-empty file name without path separators"
        ));
    }
    Ok(())
}

fn success(message: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(message)])
}

fn failure(error: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io,
        sync::{Arc, Mutex},
    };

    use crate::android_grpc;
    use async_trait::async_trait;
    use rmcp::handler::server::wrapper::Parameters;

    use super::{
        CaptureParameters, CommandOutput, CommandRunner, DeviceSimulatorMcp, MultiTouchParameters,
        Orientation, Platform, RotateParameters, SwipeParameters, TapParameters, TypeParameters,
        actionable_command_error, actionable_device_failure, command_output, escape_android_text,
        gesture_payload, parse_display_size, parse_platform, rotate_device, status_device,
        swipe_device, tap_device, type_on_device, validate_coordinates, validate_screenshot_name,
    };

    static DEVICE_PLATFORM_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Default)]
    struct FakeCommandRunner {
        outputs: Mutex<VecDeque<CommandOutput>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeCommandRunner {
        fn with_outputs(outputs: Vec<CommandOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeCommandRunner {
        async fn run_with_timeout(
            &self,
            command: &str,
            arguments: &[String],
            _command_timeout: std::time::Duration,
        ) -> anyhow::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((command.to_owned(), arguments.to_vec()));
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("fake command output was not configured"))
        }
    }

    fn successful_output(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn successful_bytes(stdout: &[u8]) -> CommandOutput {
        CommandOutput {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn accepts_normalized_coordinates() {
        assert!(validate_coordinates(&[0.0, 0.5, 1.0]).is_ok());
    }

    #[test]
    fn rejects_coordinates_outside_normalized_range() {
        assert!(validate_coordinates(&[-0.1]).is_err());
        assert!(validate_coordinates(&[1.1]).is_err());
    }

    #[test]
    fn rejects_non_finite_coordinates() {
        assert!(validate_coordinates(&[f64::NAN]).is_err());
        assert!(validate_coordinates(&[f64::INFINITY]).is_err());
        assert!(validate_coordinates(&[f64::NEG_INFINITY]).is_err());
    }

    #[test]
    fn accepts_empty_coordinate_list() {
        assert!(validate_coordinates(&[]).is_ok());
    }

    #[test]
    fn rejects_screenshot_path_traversal() {
        assert!(validate_screenshot_name("../screenshot").is_err());
        assert!(validate_screenshot_name("nested/screenshot").is_err());
    }

    #[test]
    fn rejects_invalid_screenshot_names() {
        assert!(validate_screenshot_name("").is_err());
        assert!(validate_screenshot_name(".").is_err());
        assert!(validate_screenshot_name("..").is_err());
        assert!(validate_screenshot_name("nested\\screenshot").is_err());
    }

    #[test]
    fn accepts_safe_screenshot_names() {
        assert!(validate_screenshot_name("checkout-01").is_ok());
        assert!(validate_screenshot_name("screen_2.final").is_ok());
    }

    #[test]
    fn creates_serve_sim_gesture_payload() {
        assert_eq!(
            gesture_payload("begin", 0.5, 0.25),
            r#"{"type":"begin","x":0.5,"y":0.25}"#
        );
    }

    #[test]
    fn preserves_gesture_payload_precision_and_sign() {
        assert_eq!(
            gesture_payload("move", -0.125, 1.0),
            r#"{"type":"move","x":-0.125,"y":1}"#
        );
    }

    #[test]
    fn parses_android_display_size() {
        assert_eq!(parse_display_size("1080x2400").unwrap(), (1080.0, 2400.0));
    }

    #[test]
    fn rejects_invalid_android_display_sizes() {
        assert!(parse_display_size("1080").is_err());
        assert!(parse_display_size("1080x").is_err());
        assert!(parse_display_size("widthx2400").is_err());
    }

    #[test]
    fn parses_decimal_android_display_size() {
        assert_eq!(
            parse_display_size("1080.5x2400.25").unwrap(),
            (1080.5, 2400.25)
        );
    }

    #[test]
    fn escapes_android_text_for_adb_shell() {
        assert_eq!(escape_android_text("hello world!"), "hello%sworld\\!");
    }

    #[test]
    fn escapes_all_android_shell_sensitive_characters() {
        assert_eq!(
            escape_android_text("\\&;|<>()[~*?!#$'\""),
            "\\\\\\&\\;\\|\\<\\>\\(\\)[\\~\\*\\?\\!\\#\\$\\'\\\""
        );
    }

    #[test]
    fn preserves_android_text_without_sensitive_characters() {
        assert_eq!(escape_android_text("hello-world_42"), "hello-world_42");
        assert_eq!(escape_android_text(""), "");
    }

    #[test]
    fn rejects_unknown_platforms() {
        assert!(parse_platform("windows").is_err());
        assert_eq!(parse_platform("ANDROID").unwrap(), Platform::Android);
    }

    #[test]
    fn parses_platform_case_insensitively() {
        assert_eq!(parse_platform("IOS").unwrap(), Platform::Ios);
        assert_eq!(parse_platform("Android").unwrap(), Platform::Android);
    }

    #[test]
    fn rejects_platform_with_whitespace() {
        assert!(parse_platform(" ios ").is_err());
    }

    #[test]
    fn prefers_standard_output_when_command_succeeds() {
        assert_eq!(command_output("  success  ", "warning"), "success");
    }

    #[test]
    fn uses_standard_error_when_standard_output_is_empty() {
        assert_eq!(command_output("  ", "  warning  "), "warning");
    }

    #[test]
    fn returns_empty_command_output_when_both_streams_are_empty() {
        assert_eq!(command_output("  ", "\n"), "");
    }

    #[tokio::test]
    async fn reports_android_device_status_from_runner_output() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        let runner = FakeCommandRunner::with_outputs(vec![successful_output(
            "List of devices attached\nemulator-5554\tdevice",
        )]);

        let status = status_device(Platform::Android, &runner).await.unwrap();

        assert_eq!(status, "List of devices attached\nemulator-5554\tdevice");
        assert_eq!(
            runner.calls(),
            vec![("adb".to_owned(), vec!["devices".to_owned()])]
        );
    }

    #[tokio::test]
    async fn taps_android_device_using_normalized_pixels() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        let runner = FakeCommandRunner::with_outputs(vec![
            successful_output("Physical size: 1080x2400"),
            successful_output("tap completed"),
        ]);

        let result = tap_device(Platform::Android, 0.5, 0.25, &runner)
            .await
            .unwrap();

        assert_eq!(result, "tap completed");
        assert_eq!(
            runner.calls(),
            vec![
                (
                    "adb".to_owned(),
                    vec!["shell".to_owned(), "wm".to_owned(), "size".to_owned()]
                ),
                (
                    "adb".to_owned(),
                    vec![
                        "shell".to_owned(),
                        "input".to_owned(),
                        "tap".to_owned(),
                        "540".to_owned(),
                        "600".to_owned()
                    ]
                )
            ]
        );
    }

    #[tokio::test]
    async fn swipes_ios_device_with_begin_move_and_end_events() {
        let runner = FakeCommandRunner::with_outputs(vec![
            successful_output("begin"),
            successful_output("move"),
            successful_output("end"),
        ]);
        let parameters = super::SwipeParameters {
            x1: 0.1,
            y1: 0.2,
            x2: 0.8,
            y2: 0.9,
        };

        let result = swipe_device(Platform::Ios, parameters, &runner)
            .await
            .unwrap();

        assert_eq!(result, "Swipe completed");
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "npx");
        assert!(calls[0].1[3].contains(r#""type":"begin""#));
        assert!(calls[1].1[3].contains(r#""type":"move""#));
        assert!(calls[2].1[3].contains(r#""type":"end""#));
    }

    #[tokio::test]
    async fn types_android_text_after_escaping_it() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        let runner = FakeCommandRunner::with_outputs(vec![successful_output("")]);

        type_on_device(Platform::Android, "hello world!", &runner)
            .await
            .unwrap();

        assert_eq!(
            runner.calls()[0].1,
            vec![
                "shell".to_owned(),
                "input".to_owned(),
                "text".to_owned(),
                "hello%sworld\\!".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn captures_android_bytes_without_decoding_them() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        let runner = FakeCommandRunner::with_outputs(vec![successful_bytes(&[0, 1, 2, 255])]);

        let (bytes, description) = super::capture_device(Platform::Android, "screen", &runner)
            .await
            .unwrap();

        assert_eq!(bytes, vec![0, 1, 2, 255]);
        assert_eq!(description, "Captured Android Emulator display");
    }

    #[tokio::test]
    async fn returns_runner_error_for_failed_android_command() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        let runner = FakeCommandRunner::with_outputs(vec![CommandOutput {
            success: false,
            status: "exit status: 1".to_owned(),
            stdout: Vec::new(),
            stderr: b"device unavailable".to_vec(),
        }]);

        let result = status_device(Platform::Android, &runner).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "adb failed with exit status: 1: device unavailable"
        );
    }

    #[tokio::test]
    async fn delegates_android_tool_handlers_to_the_command_runner() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_output("Android Emulator is ready"),
            successful_output("waited"),
            successful_output("List of devices attached"),
            successful_output("Physical size: 1080x2400"),
            successful_output("tap"),
            successful_output("Physical size: 1080x2400"),
            successful_output("swipe"),
            successful_output("type"),
            successful_bytes(b"png"),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let start_result = service.device_start().await;
        let status_result = service.device_status().await;
        let tap_result = service
            .device_tap(Parameters(TapParameters { x: 0.5, y: 0.25 }))
            .await;
        let swipe_result = service
            .device_swipe(Parameters(SwipeParameters {
                x1: 0.1,
                y1: 0.2,
                x2: 0.8,
                y2: 0.9,
            }))
            .await;
        let type_result = service
            .device_type(Parameters(TypeParameters {
                text: "test".to_owned(),
            }))
            .await;
        let capture_result = service
            .device_capture(Parameters(CaptureParameters {
                name: Some("handler".to_owned()),
                options: Default::default(),
                latest_frame: false,
                max_age_ms: None,
            }))
            .await;
        let stop_result = service.device_stop().await;

        assert_eq!(start_result.is_error, Some(false));
        assert_eq!(status_result.is_error, Some(false));
        assert_eq!(tap_result.is_error, Some(false));
        assert_eq!(swipe_result.is_error, Some(false));
        assert_eq!(type_result.is_error, Some(false));
        assert_eq!(capture_result.is_error, Some(false));
        assert_eq!(stop_result.is_error, Some(false));
        assert_eq!(runner.calls().len(), 9);
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn rejects_the_entire_batch_before_any_input() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![
                    super::Action::Tap { x: 0.5, y: 0.5 },
                    super::Action::Tap { x: 2.0, y: 0.5 },
                ],
                capture: None,
                timeout_ms: None,
                settle_ms: None,
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn reports_partial_completion_and_never_replays_inputs() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_output("first"),
            CommandOutput {
                success: false,
                status: "exit 1".to_owned(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![
                    super::Action::Type {
                        text: "first".to_owned(),
                    },
                    super::Action::Type {
                        text: "second".to_owned(),
                    },
                    super::Action::Type {
                        text: "third".to_owned(),
                    },
                ],
                capture: None,
                timeout_ms: None,
                settle_ms: None,
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(runner.calls().len(), 2);
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(encoded.contains("completed_actions\\\":1"));
        assert!(!encoded.contains("first"));
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn runs_known_actions_and_capture_without_extra_round_trips() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_output("type"),
            successful_bytes(b"png"),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![super::Action::Type {
                    text: "sample".to_owned(),
                }],
                capture: Some(CaptureParameters {
                    name: None,
                    options: Default::default(),
                    latest_frame: false,
                    max_age_ms: None,
                }),
                timeout_ms: None,
                settle_ms: None,
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(runner.calls().len(), 2);
        assert_eq!(result.content.len(), 3);
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn queries_android_dimensions_once_per_swipe_and_clamps_edges() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        let runner = FakeCommandRunner::with_outputs(vec![
            successful_output("Physical size: 1080x2400"),
            successful_output("swipe"),
        ]);
        swipe_device(
            Platform::Android,
            SwipeParameters {
                x1: 0.0,
                y1: 0.0,
                x2: 1.0,
                y2: 1.0,
            },
            &runner,
        )
        .await
        .unwrap();
        assert_eq!(runner.calls().len(), 2);
        assert_eq!(
            runner.calls()[1].1,
            ["shell", "input", "swipe", "0", "0", "1079", "2399", "300"]
        );
    }

    fn sample_png(value: u8) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            16,
            image::Rgb([value; 3]),
        ));
        let mut output = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[tokio::test]
    async fn returns_the_exact_fresh_sample_that_satisfied_the_wait() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let expected = sample_png(255);
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_bytes(&sample_png(0)),
            successful_bytes(&expected),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                capture: Some(Default::default()),
                wait_condition: Some(crate::visual_wait::Condition::VisualChange),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(runner.calls().len(), 2);
        let serialized = serde_json::to_value(result).unwrap();
        assert!(
            serialized["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("visual_wait_sample")
        );
        use base64::Engine;
        assert_eq!(
            serialized["content"][2]["data"],
            base64::engine::general_purpose::STANDARD.encode(expected)
        );
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn cache_reuses_capture_but_is_invalidated_even_by_failed_input() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("ANDROID_SERIAL", "test-cache-target");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_bytes(&sample_png(0)),
            CommandOutput {
                success: false,
                status: "failed".to_owned(),
                stdout: vec![],
                stderr: vec![],
            },
            successful_bytes(&sample_png(1)),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let capture = || CaptureParameters {
            max_age_ms: Some(5000),
            ..Default::default()
        };
        let first = service.capture(Platform::Android, capture()).await.unwrap();
        let second = service.capture(Platform::Android, capture()).await.unwrap();
        assert!(
            serde_json::to_string(&first)
                .unwrap()
                .contains("cache_hit\\\":false")
        );
        assert!(
            serde_json::to_string(&second)
                .unwrap()
                .contains("cache_hit\\\":true")
        );
        assert_eq!(runner.calls().len(), 1);
        assert!(
            service
                .type_text(Platform::Android, "sample")
                .await
                .is_err()
        );
        let third = service.capture(Platform::Android, capture()).await.unwrap();
        assert!(
            serde_json::to_string(&third)
                .unwrap()
                .contains("cache_hit\\\":false")
        );
        assert_eq!(runner.calls().len(), 3);
        unsafe {
            std::env::remove_var("ANDROID_SERIAL");
        }
    }

    #[tokio::test]
    async fn visual_change_uses_pre_action_baseline_and_fresh_samples() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_bytes(&sample_png(0)),
            successful_output("submitted"),
            successful_bytes(&sample_png(0)),
            successful_bytes(&sample_png(255)),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![super::Action::Type {
                    text: "sample".to_owned(),
                }],
                wait_condition: Some(crate::visual_wait::Condition::VisualChange),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(runner.calls().len(), 4);
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn visual_wait_timeout_reports_completed_actions_without_replaying() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_output("submitted"),
            successful_bytes(&sample_png(0)),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![super::Action::Type {
                    text: "sample".to_owned(),
                }],
                timeout_ms: Some(100),
                wait_condition: Some(crate::visual_wait::Condition::VisualStability {
                    stable_samples: Some(3),
                }),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(encoded.contains("visual_wait_deadline"));
        assert!(encoded.contains("completed_actions\\\":1"));
        assert_eq!(runner.calls().len(), 2);
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn unsupported_step_accessibility_is_rejected_before_input() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![super::Action::Type {
                    text: "sample".to_owned(),
                }],
                accessibility: true,
                accessibility_options: Some(crate::accessibility::Options {
                    identifier: Some("sample".to_owned()),
                    max_elements: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![super::Action::Type {
                    text: "sample".to_owned(),
                }],
                wait_condition: Some(crate::visual_wait::Condition::ElementPresent {
                    label: Some("Example".to_owned()),
                    identifier: None,
                }),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn step_accessibility_fetches_only_the_requested_observation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "ios");
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let length = stream.read(&mut request).await.unwrap();
            assert!(
                std::str::from_utf8(&request[..length])
                    .unwrap()
                    .starts_with("GET /helper/test/ax ")
            );
            let body = r#"{"elements":[{"label":"Example","identifier":"sample"}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ios: Arc::new(
                crate::ios::Backend::for_helper(&format!("http://{address}/helper/test")).unwrap(),
            ),
            ..Default::default()
        };
        let result = service
            .device_step(Parameters(super::StepParameters {
                accessibility: true,
                wait_condition: Some(crate::visual_wait::Condition::ElementPresent {
                    label: Some("Example".to_owned()),
                    identifier: Some("sample".to_owned()),
                }),
                accessibility_options: Some(crate::accessibility::Options {
                    identifier: Some("sample".to_owned()),
                    max_elements: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.content.len(), 2);
        assert!(serde_json::to_string(&result).unwrap().contains("Example"));
        assert!(runner.calls().is_empty());
        server.await.unwrap();
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[test]
    fn rejects_cached_visual_wait_observations_and_conflicting_outputs() {
        let parameters = super::StepParameters {
            actions: vec![],
            capture: Some(CaptureParameters {
                max_age_ms: Some(100),
                ..Default::default()
            }),
            wait_condition: Some(crate::visual_wait::Condition::VisualChange),
            ..Default::default()
        };
        assert!(super::validate_step(&parameters).is_err());
        let parameters = super::StepParameters {
            accessibility: true,
            capture: Some(Default::default()),
            ..Default::default()
        };
        assert!(super::validate_step(&parameters).is_err());
    }

    #[test]
    fn element_selectors_must_match_the_same_projected_element() {
        let snapshot = r#"{"elements":[{"label":"Example","identifier":"first"},{"AXLabel":"Other","AXIdentifier":"second"}]}"#;
        assert!(super::element_matches(snapshot, Some("Example"), Some("first")).unwrap());
        assert!(!super::element_matches(snapshot, Some("Example"), Some("second")).unwrap());
        assert!(super::element_matches(snapshot, None, Some("second")).unwrap());
        assert!(
            crate::visual_wait::Condition::ElementPresent {
                label: None,
                identifier: None
            }
            .validate()
            .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_invalid_inspection_options_before_backend_io() {
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_inspect(Parameters(crate::accessibility::Options {
                max_elements: Some(0),
                ..Default::default()
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn reports_configured_capabilities_without_claiming_readiness_or_device_io() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
            std::env::set_var("DEVICE_ANDROID_BACKEND", "adb");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service.device_capabilities().await;
        assert_eq!(result.is_error, Some(false));
        let result = serde_json::to_value(result).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(value["backend"], "adb");
        assert_eq!(value["availability_probed"], false);
        assert_eq!(value["controls"]["orientation"], true);
        assert_eq!(value["controls"]["multitouch"], false);
        assert_eq!(value["observations"]["accessibility"], true);
        assert_eq!(value["observations"]["step_accessibility"], false);
        assert_eq!(value["wait_conditions"]["element_present"], false);
        assert_eq!(value["wait_conditions"]["visual_stability"], true);
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
            std::env::remove_var("DEVICE_ANDROID_BACKEND");
        }
    }

    #[tokio::test]
    async fn rejects_physical_iphone_steps_without_webdriveragent_before_device_io() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "ios");
            std::env::set_var("DEVICE_IOS_TARGET", "device");
            std::env::set_var("IOS_DEVICE_UDID", "physical-device-test");
            std::env::remove_var("IOS_WDA_URL");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service
            .device_step(Parameters(super::StepParameters {
                actions: vec![super::Action::Tap { x: 0.5, y: 0.5 }],
                ..Default::default()
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("IOS_WDA_URL")
        );
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
            std::env::remove_var("DEVICE_IOS_TARGET");
            std::env::remove_var("IOS_DEVICE_UDID");
        }
    }

    #[tokio::test]
    async fn inspects_android_uiautomator_tree_through_serial_scoped_adb() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
            std::env::set_var("DEVICE_ANDROID_BACKEND", "adb");
            std::env::set_var("ANDROID_SERIAL", "emulator-5554");
        }
        let tree = br#"<hierarchy><node class="root"><node text="Continue" resource-id="app:id/continue" class="Button" bounds="[0,0][200,80]" enabled="true"/></node></hierarchy>"#;
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![successful_bytes(
            tree,
        )]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service
            .device_inspect(Parameters(crate::accessibility::Options {
                identifier: Some("app:id/continue".to_owned()),
                ..Default::default()
            }))
            .await;

        assert_eq!(result.is_error, Some(false));
        let text = serde_json::to_value(result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        let projected: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(projected["elements"].as_array().unwrap().len(), 1);
        assert_eq!(projected["elements"][0]["label"], "Continue");
        assert_eq!(runner.calls().len(), 1);
        assert_eq!(runner.calls()[0].0, "adb");
        assert_eq!(runner.calls()[0].1[0..3], ["-s", "emulator-5554", "shell"]);
        assert!(runner.calls()[0].1[5].contains("uiautomator dump --compressed"));
        assert!(runner.calls()[0].1[5].contains("rm -f"));
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
            std::env::remove_var("DEVICE_ANDROID_BACKEND");
            std::env::remove_var("ANDROID_SERIAL");
        }
    }

    #[tokio::test]
    async fn rotates_android_with_serial_scoped_adb_and_disables_auto_rotation() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
            std::env::set_var("DEVICE_ANDROID_BACKEND", "adb");
            std::env::set_var("ANDROID_SERIAL", "emulator-5554");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_output(""),
            successful_output(""),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service
            .device_rotate(Parameters(RotateParameters {
                orientation: Orientation::LandscapeRight,
            }))
            .await;

        assert_eq!(result.is_error, Some(false));
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("automatic rotation is disabled")
        );
        assert_eq!(
            runner.calls(),
            vec![
                (
                    "adb".to_owned(),
                    vec![
                        "-s".to_owned(),
                        "emulator-5554".to_owned(),
                        "shell".to_owned(),
                        "settings".to_owned(),
                        "put".to_owned(),
                        "system".to_owned(),
                        "accelerometer_rotation".to_owned(),
                        "0".to_owned(),
                    ],
                ),
                (
                    "adb".to_owned(),
                    vec![
                        "-s".to_owned(),
                        "emulator-5554".to_owned(),
                        "shell".to_owned(),
                        "settings".to_owned(),
                        "put".to_owned(),
                        "system".to_owned(),
                        "user_rotation".to_owned(),
                        "3".to_owned(),
                    ],
                ),
            ]
        );
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
            std::env::remove_var("DEVICE_ANDROID_BACKEND");
            std::env::remove_var("ANDROID_SERIAL");
        }
    }

    #[tokio::test]
    async fn maps_each_orientation_to_android_rotation_settings() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("ANDROID_SERIAL", "emulator-5554");
        }
        let runner =
            FakeCommandRunner::with_outputs((0..8).map(|_| successful_output("")).collect());
        for (orientation, expected) in [
            (Orientation::Portrait, "0"),
            (Orientation::PortraitUpsideDown, "2"),
            (Orientation::LandscapeLeft, "1"),
            (Orientation::LandscapeRight, "3"),
        ] {
            rotate_device(Platform::Android, orientation, &runner)
                .await
                .unwrap();
            assert_eq!(
                runner.calls().last().unwrap().1.last().map(String::as_str),
                Some(expected)
            );
        }
        unsafe {
            std::env::remove_var("ANDROID_SERIAL");
        }
    }

    #[tokio::test]
    async fn rotates_ios_through_the_pinned_serve_sim_command() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("IOS_SIMULATOR_UDID", "11111111-1111-4111-8111-111111111111");
            std::env::set_var("SERVE_SIM_BINARY", "serve-sim-test");
        }
        let runner = FakeCommandRunner::with_outputs(vec![successful_output("Rotated")]);

        rotate_device(Platform::Ios, Orientation::LandscapeLeft, &runner)
            .await
            .unwrap();

        assert_eq!(
            runner.calls(),
            vec![(
                "serve-sim-test".to_owned(),
                vec![
                    "rotate".to_owned(),
                    "--device".to_owned(),
                    "11111111-1111-4111-8111-111111111111".to_owned(),
                    "landscape_left".to_owned(),
                ],
            )]
        );
        unsafe {
            std::env::remove_var("IOS_SIMULATOR_UDID");
            std::env::remove_var("SERVE_SIM_BINARY");
        }
    }

    #[tokio::test]
    async fn does_not_fall_back_to_adb_rotation_when_android_grpc_is_enabled() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
            std::env::set_var("DEVICE_ANDROID_BACKEND", "grpc");
            std::env::set_var("ANDROID_GRPC_ENDPOINT", "http://127.0.0.1:8554");
            std::env::set_var("ANDROID_GRPC_TOKEN_FILE", "/path/not-read-by-this-test");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service
            .device_rotate(Parameters(RotateParameters {
                orientation: Orientation::Portrait,
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("not supported by the experimental Android gRPC backend")
        );
        assert!(runner.calls().is_empty());
        let capabilities = service.device_capabilities().await;
        let capabilities: serde_json::Value = serde_json::from_str(
            serde_json::to_value(capabilities).unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(capabilities["controls"]["orientation"], false);
        assert_eq!(capabilities["controls"]["multitouch"], true);
        assert_eq!(capabilities["observations"]["accessibility"], false);

        let inspect = service
            .device_inspect(Parameters(crate::accessibility::Options::default()))
            .await;
        assert_eq!(inspect.is_error, Some(true));
        assert!(
            serde_json::to_string(&inspect)
                .unwrap()
                .contains("not supported by the experimental gRPC backend")
        );
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
            std::env::remove_var("DEVICE_ANDROID_BACKEND");
            std::env::remove_var("ANDROID_GRPC_ENDPOINT");
            std::env::remove_var("ANDROID_GRPC_TOKEN_FILE");
        }
    }

    #[test]
    fn validates_simultaneous_multitouch_contact_lifecycles_before_io() {
        let frames = vec![
            android_grpc::MultiTouchFrame {
                delay_ms: Some(40),
                contacts: vec![
                    android_grpc::MultiTouchContact {
                        id: 0,
                        x: 0.1,
                        y: 0.8,
                        phase: android_grpc::ContactPhase::Down,
                    },
                    android_grpc::MultiTouchContact {
                        id: 1,
                        x: 0.8,
                        y: 0.7,
                        phase: android_grpc::ContactPhase::Down,
                    },
                ],
            },
            android_grpc::MultiTouchFrame {
                delay_ms: Some(40),
                contacts: vec![
                    android_grpc::MultiTouchContact {
                        id: 0,
                        x: 0.2,
                        y: 0.8,
                        phase: android_grpc::ContactPhase::Move,
                    },
                    android_grpc::MultiTouchContact {
                        id: 1,
                        x: 0.8,
                        y: 0.6,
                        phase: android_grpc::ContactPhase::Move,
                    },
                ],
            },
            android_grpc::MultiTouchFrame {
                delay_ms: Some(0),
                contacts: vec![
                    android_grpc::MultiTouchContact {
                        id: 0,
                        x: 0.2,
                        y: 0.8,
                        phase: android_grpc::ContactPhase::Move,
                    },
                    android_grpc::MultiTouchContact {
                        id: 1,
                        x: 0.8,
                        y: 0.6,
                        phase: android_grpc::ContactPhase::Up,
                    },
                ],
            },
            android_grpc::MultiTouchFrame {
                delay_ms: None,
                contacts: vec![android_grpc::MultiTouchContact {
                    id: 0,
                    x: 0.2,
                    y: 0.8,
                    phase: android_grpc::ContactPhase::Up,
                }],
            },
        ];
        assert!(
            MultiTouchParameters {
                frames: frames.clone(),
                timeout_ms: Some(1000),
            }
            .validate()
            .is_ok()
        );

        let mut invalid = frames;
        invalid[1].contacts[0].phase = android_grpc::ContactPhase::Up;
        assert!(
            MultiTouchParameters {
                frames: invalid,
                timeout_ms: None,
            }
            .validate()
            .is_err()
        );
    }

    #[tokio::test]
    async fn reports_multitouch_unsupported_in_default_android_adb_mode() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
            std::env::set_var("DEVICE_ANDROID_BACKEND", "adb");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };
        let result = service
            .device_multitouch(Parameters(MultiTouchParameters {
                frames: vec![
                    android_grpc::MultiTouchFrame {
                        delay_ms: Some(0),
                        contacts: vec![android_grpc::MultiTouchContact {
                            id: 0,
                            x: 0.5,
                            y: 0.5,
                            phase: android_grpc::ContactPhase::Down,
                        }],
                    },
                    android_grpc::MultiTouchFrame {
                        delay_ms: None,
                        contacts: vec![android_grpc::MultiTouchContact {
                            id: 0,
                            x: 0.5,
                            y: 0.5,
                            phase: android_grpc::ContactPhase::Up,
                        }],
                    },
                ],
                timeout_ms: None,
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("ADB cannot submit simultaneous contacts")
        );
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
            std::env::remove_var("DEVICE_ANDROID_BACKEND");
        }
    }

    #[test]
    fn rejects_step_inspection_options_without_accessibility_output() {
        assert!(
            super::validate_step(&super::StepParameters {
                actions: vec![super::Action::Type {
                    text: "sample".to_owned()
                }],
                accessibility_options: Some(Default::default()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn uses_android_override_dimensions_when_present() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        let runner = FakeCommandRunner::with_outputs(vec![
            successful_output("Physical size: 1080x2400\nOverride size: 720x1600"),
            successful_output("tap"),
        ]);
        tap_device(Platform::Android, 1.0, 1.0, &runner)
            .await
            .unwrap();
        assert_eq!(
            runner.calls()[1].1,
            ["shell", "input", "tap", "719", "1599"]
        );
    }

    #[tokio::test]
    async fn removes_unique_screenshot_directories_after_failure() {
        struct FailedCapture(std::sync::Mutex<Option<std::path::PathBuf>>);
        #[async_trait]
        impl CommandRunner for FailedCapture {
            async fn run_with_timeout(
                &self,
                _: &str,
                arguments: &[String],
                _command_timeout: std::time::Duration,
            ) -> anyhow::Result<CommandOutput> {
                *self.0.lock().unwrap() = arguments.last().map(std::path::PathBuf::from);
                Err(anyhow::anyhow!("capture failed"))
            }
        }
        let runner = FailedCapture(Default::default());
        assert!(
            super::capture_device(Platform::Ios, "screen", &runner)
                .await
                .is_err()
        );
        let path = runner.0.lock().unwrap().clone().unwrap();
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn rejects_invalid_display_dimensions_and_bounds_accessibility_output() {
        for size in ["0x1", "1x0", "NaNx1", "infx1", "-1x2"] {
            assert!(parse_display_size(size).is_err());
        }
        let tree = serde_json::json!({"elements":(0..250).map(|i| serde_json::json!({"label":i.to_string()})).collect::<Vec<_>>()});
        let result: serde_json::Value =
            serde_json::from_str(&super::compact_accessibility(tree)).unwrap();
        assert_eq!(result["elements"].as_array().unwrap().len(), 200);
        assert_eq!(result["truncated"], true);
    }

    #[tokio::test]
    async fn rejects_empty_text_before_calling_external_commands() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service
            .device_type(Parameters(TypeParameters {
                text: String::new(),
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn repair_input_runs_serve_sim_repair_for_ios() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "ios");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![successful_output(
            "Input services repaired",
        )]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service.device_repair_input().await;

        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            runner.calls(),
            vec![(
                "npx".to_owned(),
                vec![
                    "--yes".to_owned(),
                    "serve-sim@0.1.47".to_owned(),
                    "repair-input".to_owned()
                ]
            )]
        );
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn rejects_repair_input_on_android_without_running_a_command() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service.device_repair_input().await;

        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn returns_repair_command_failure_to_the_caller() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "ios");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![CommandOutput {
            success: false,
            status: "exit status: 1".to_owned(),
            stdout: Vec::new(),
            stderr: b"repair failed".to_vec(),
        }]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service.device_repair_input().await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(runner.calls().len(), 1);
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn rejects_repair_input_for_an_unsupported_platform() {
        let _environment_lock = DEVICE_PLATFORM_LOCK.lock().await;
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "unsupported");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
            ..Default::default()
        };

        let result = service.device_repair_input().await;

        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[test]
    fn explains_how_to_install_missing_platform_commands() {
        let missing_adb = actionable_command_error(
            "adb",
            anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound)),
        );
        let missing_npx = actionable_command_error(
            "npx",
            anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound)),
        );
        let missing_xcrun = actionable_command_error(
            "xcrun",
            anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound)),
        );

        assert!(missing_adb.to_string().contains("Platform-Tools"));
        assert!(missing_npx.to_string().contains("Node.js 24.21.0"));
        assert!(missing_xcrun.to_string().contains("xcode-select --install"));
    }

    #[test]
    fn preserves_non_missing_command_errors() {
        let original_error = anyhow::anyhow!("permission denied");

        let error = actionable_command_error("adb", original_error);

        assert_eq!(error.to_string(), "permission denied");
    }

    #[test]
    fn explains_how_to_start_an_android_device() {
        let error = actionable_device_failure("adb", "error: no devices/emulators found").unwrap();

        assert!(error.to_string().contains("Start an Android Emulator"));
        assert!(error.to_string().contains("ANDROID_SERIAL"));
    }

    #[test]
    fn explains_how_to_repair_serve_sim_startup() {
        let error =
            actionable_device_failure("npx", "could not determine executable to run").unwrap();

        assert!(error.to_string().contains("serve-sim"));
        assert!(error.to_string().contains("Node.js 24.21.0"));
    }

    #[test]
    fn does_not_replace_unrelated_command_failures() {
        assert!(actionable_device_failure("adb", "permission denied").is_none());
        assert!(actionable_device_failure("npx", "network timeout").is_none());
    }
}
