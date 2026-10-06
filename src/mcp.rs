use crate::{
    android_grpc, frame_cache, ios, ios_lifecycle, observation, process, session, visual_wait,
};

use std::{io, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use process::{CommandOutput, CommandRunner, ProcessCommandRunner};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
};
use tokio::time::timeout;

const PREVIEW_TIMEOUT: Duration = Duration::from_secs(15);

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Ios,
    Android,
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
    #[tool(description = "Start inspection for the configured device")]
    async fn device_start(&self) -> CallToolResult {
        self.session
            .execute("start", Duration::from_secs(25), async {
                self.cache.lock().await.clear();
                let platform = match configured_platform() {
                    Ok(platform) => platform,
                    Err(error) => return failure(error),
                };
                let result = if matches!(platform, Platform::Ios) {
                    match self.ios.enabled() {
                        Ok(true) => self.ios.status().await,
                        Ok(false) => self.lifecycle.start(self.runner.as_ref()).await,
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
            Platform::Ios => match self.stop_ios().await {
                Ok(message) => success(message),
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
                    match self.ios.enabled() {
                        Ok(true) => self.ios.status().await,
                        Ok(false) => status_device(platform, self.runner.as_ref()).await,
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
        if (parameters.accessibility
            || matches!(
                parameters.wait_condition,
                Some(visual_wait::Condition::ElementPresent { .. })
            ))
            && (!matches!(platform, Platform::Ios) || !matches!(self.ios.enabled(), Ok(true)))
        {
            return failure(anyhow::anyhow!(
                "step accessibility/element wait requires persistent iOS; no actions were submitted"
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
                        None => compact_accessibility(self.ios.accessibility().await?),
                    };
                    return Ok(vec![ContentBlock::text(snapshot)]);
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
        description = "Inspect iOS accessibility on demand through the opt-in persistent serve-sim backend. Output is bounded and can be truncated; no screenshot is collected."
    )]
    async fn device_inspect(&self) -> CallToolResult {
        self.session
            .execute("inspect", Duration::from_secs(15), async {
                if !matches!(configured_platform(), Ok(Platform::Ios)) {
                    return failure(anyhow::anyhow!(
                        "accessibility inspection currently requires persistent iOS"
                    ));
                }
                match self.ios.accessibility().await {
                    Ok(value) => success(compact_accessibility(value)),
                    Err(error) => failure(error),
                }
            })
            .await
    }
}

#[rmcp::tool_handler(
    instructions = "Prefer device_step for already-known action sequences with one final capture. Do not batch actions depending on unseen UI. PNG fresh screenshots remain the default; choose JPEG/max_dimension for smaller observations, or latest_frame with persistent iOS when unknown source age is acceptable. Never automatically retry an uncertain input. Persistent backends are opt-in and require externally provisioned local services."
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
    async fn stop_ios(&self) -> anyhow::Result<String> {
        if self.ios.enabled()? {
            self.ios.stop().await
        } else {
            Ok(self.lifecycle.stop().await)
        }
    }

    async fn tap(&self, platform: Platform, x: f64, y: f64) -> anyhow::Result<String> {
        self.cache.lock().await.clear();
        if matches!(platform, Platform::Ios) && self.ios.enabled()? {
            self.ios.tap(x, y).await
        } else if matches!(platform, Platform::Android) && self.android.enabled()? {
            self.android.tap(x, y).await
        } else {
            tap_device(platform, x, y, self.runner.as_ref()).await
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.lifecycle.stop().await;
        let _ = self.ios.stop().await;
    }

    async fn swipe(&self, platform: Platform, points: SwipeParameters) -> anyhow::Result<String> {
        self.cache.lock().await.clear();
        if matches!(platform, Platform::Ios) && self.ios.enabled()? {
            self.ios
                .swipe((points.x1, points.y1, points.x2, points.y2))
                .await
        } else if matches!(platform, Platform::Android) && self.android.enabled()? {
            self.android
                .swipe((points.x1, points.y1, points.x2, points.y2))
                .await
        } else {
            swipe_device(platform, points, self.runner.as_ref()).await
        }
    }

    async fn type_text(&self, platform: Platform, text: &str) -> anyhow::Result<String> {
        self.cache.lock().await.clear();
        if matches!(platform, Platform::Ios) && self.ios.enabled()? {
            self.ios.type_text(text).await
        } else if matches!(platform, Platform::Android) && self.android.enabled()? {
            self.android.type_text(text).await
        } else {
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
            if matches!(platform, Platform::Android) && self.android.enabled()? {
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
    ) -> anyhow::Result<String> {
        for _ in 0..32 {
            let snapshot = compact_accessibility(self.ios.accessibility().await?);
            if element_matches(&snapshot, label, identifier)? {
                return Ok(snapshot);
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
            "{platform:?}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            parameters.latest_frame,
            std::env::var("IOS_SIMULATOR_UDID").unwrap_or_default(),
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
        Platform::Ios => std::env::var("IOS_SIMULATOR_UDID").is_ok_and(|value| !value.is_empty()),
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
    fn visit(
        value: &serde_json::Value,
        depth: usize,
        nodes: &mut Vec<serde_json::Value>,
        truncated: &mut bool,
    ) {
        if depth > 16 || nodes.len() >= 200 {
            *truncated = true;
            return;
        }
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, depth + 1, nodes, truncated);
                }
            }
            serde_json::Value::Object(values) => {
                let mut node = serde_json::Map::new();
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
                        let mut value = value.clone();
                        if let Some(text) = value.as_str() {
                            value = serde_json::Value::String(text.chars().take(512).collect());
                        }
                        if !value.is_object() || matches!(key, "frame" | "AXFrame") {
                            node.insert(key.to_owned(), value);
                        }
                    }
                }
                if !node.is_empty() {
                    nodes.push(serde_json::Value::Object(node));
                }
                for (key, value) in values {
                    if matches!(
                        key.as_str(),
                        "children" | "elements" | "AXChildren" | "tree"
                    ) {
                        visit(value, depth + 1, nodes, truncated);
                    }
                }
            }
            _ => {}
        }
    }
    let mut nodes = Vec::new();
    let mut truncated = false;
    visit(&value, 0, &mut nodes, &mut truncated);
    serde_json::json!({"elements": nodes, "truncated": truncated, "max_elements":200, "max_depth":16}).to_string()
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

fn configured_platform() -> anyhow::Result<Platform> {
    let platform = std::env::var("DEVICE_PLATFORM").unwrap_or_else(|_| "ios".to_owned());
    parse_platform(&platform)
}

fn parse_platform(platform: &str) -> anyhow::Result<Platform> {
    match platform.to_ascii_lowercase().as_str() {
        "ios" => Ok(Platform::Ios),
        "android" => Ok(Platform::Android),
        platform => Err(anyhow::anyhow!(
            "unsupported DEVICE_PLATFORM '{platform}'; use ios or android"
        )),
    }
}

async fn start_device(platform: Platform, runner: &dyn CommandRunner) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            anyhow::bail!("iOS lifecycle must use the ownership-aware helper")
        }
        Platform::Android => {
            run_adb_command(runner, &["start-server"]).await?;
            timeout(PREVIEW_TIMEOUT, run_adb(runner, &["wait-for-device"]))
                .await
                .map_err(|_| anyhow::anyhow!("Android Emulator did not become ready"))??;
            Ok("Android Emulator is ready".to_owned())
        }
    }
}

async fn status_device(platform: Platform, runner: &dyn CommandRunner) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim(runner, &["--list"]).await,
        Platform::Android => run_adb(runner, &["devices"]).await,
    }
}

async fn capture_device(
    platform: Platform,
    name: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<(Vec<u8>, String)> {
    match platform {
        Platform::Ios => {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join(format!("{name}.png"));
            let output = run_command(
                runner,
                "xcrun",
                &[
                    "simctl".to_owned(),
                    "io".to_owned(),
                    ios_device_target(),
                    "screenshot".to_owned(),
                    path.display().to_string(),
                ],
            )
            .await?;
            let image_bytes = if output.stdout.is_empty() {
                tokio::fs::read(&path).await?
            } else {
                output.stdout
            };
            let _ = tokio::fs::remove_file(&path).await;
            Ok((image_bytes, "Captured iOS Simulator display".to_owned()))
        }
        Platform::Android => {
            let image_bytes = run_adb_bytes(runner, &["exec-out", "screencap", "-p"]).await?;
            Ok((image_bytes, "Captured Android Emulator display".to_owned()))
        }
    }
}

async fn tap_device(
    platform: Platform,
    x: f64,
    y: f64,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            let x = x.to_string();
            let y = y.to_string();
            let arguments = ["tap", x.as_str(), y.as_str()];
            run_serve_sim(runner, &arguments).await
        }
        Platform::Android => {
            let (pixel_x, pixel_y) = android_pixels(x, y, runner).await?;
            run_adb(runner, &["shell", "input", "tap", &pixel_x, &pixel_y]).await
        }
    }
}

async fn swipe_device(
    platform: Platform,
    parameters: SwipeParameters,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            let gestures = [
                gesture_payload("begin", parameters.x1, parameters.y1),
                gesture_payload("move", parameters.x2, parameters.y2),
                gesture_payload("end", parameters.x2, parameters.y2),
            ];
            for gesture in gestures {
                run_serve_sim(runner, &["gesture", &gesture]).await?;
            }
            Ok("Swipe completed".to_owned())
        }
        Platform::Android => {
            let dimensions = android_dimensions(runner).await?;
            let (start_x, start_y) = normalized_pixels(parameters.x1, parameters.y1, dimensions);
            let (end_x, end_y) = normalized_pixels(parameters.x2, parameters.y2, dimensions);
            run_adb(
                runner,
                &[
                    "shell", "input", "swipe", &start_x, &start_y, &end_x, &end_y, "300",
                ],
            )
            .await
        }
    }
}

async fn type_on_device(
    platform: Platform,
    text: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim(runner, &["type", text]).await,
        Platform::Android => {
            let escaped_text = escape_android_text(text);
            run_adb(runner, &["shell", "input", "text", &escaped_text]).await
        }
    }
}

fn escape_android_text(text: &str) -> String {
    let mut escaped_text = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            ' ' => escaped_text.push_str("%s"),
            '\\' | '&' | ';' | '|' | '<' | '>' | '(' | ')' | '~' | '*' | '?' | '!' | '#' | '$'
            | '\'' | '"' => {
                escaped_text.push('\\');
                escaped_text.push(character);
            }
            _ => escaped_text.push(character),
        }
    }
    escaped_text
}

async fn android_pixels(
    x: f64,
    y: f64,
    runner: &dyn CommandRunner,
) -> anyhow::Result<(String, String)> {
    let dimensions = android_dimensions(runner).await?;
    Ok(normalized_pixels(x, y, dimensions))
}

async fn android_dimensions(runner: &dyn CommandRunner) -> anyhow::Result<(f64, f64)> {
    let output = run_adb(runner, &["shell", "wm", "size"]).await?;
    let dimensions = output
        .lines()
        .rev()
        .find_map(|line| line.rsplit_once(' ').map(|(_, value)| value.trim()))
        .ok_or_else(|| anyhow::anyhow!("could not determine Android display size"))?;
    parse_display_size(dimensions)
}

fn normalized_pixels(x: f64, y: f64, (width, height): (f64, f64)) -> (String, String) {
    (
        (x * (width - 1.0)).round().to_string(),
        (y * (height - 1.0)).round().to_string(),
    )
}

async fn run_serve_sim(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    let mut command_arguments = Vec::new();
    let device = std::env::var("IOS_SIMULATOR_UDID").ok();
    if let Some(device) = device {
        if arguments
            .first()
            .is_some_and(|argument| !argument.starts_with('-'))
        {
            command_arguments.push(arguments[0].to_owned());
            command_arguments.extend(["--device".to_owned(), device]);
            command_arguments.extend(arguments[1..].iter().map(|argument| (*argument).to_owned()));
        } else {
            command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
            command_arguments.push(device);
        }
    } else {
        command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    }
    let (program, command_arguments) = ios::command(command_arguments)?;
    run_command(runner, &program, &command_arguments)
        .await
        .map(|output| {
            command_output(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )
        })
}

fn ios_device_target() -> String {
    std::env::var("IOS_SIMULATOR_UDID").unwrap_or_else(|_| "booted".to_owned())
}

async fn run_adb(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    let mut command_arguments = Vec::with_capacity(arguments.len() + 2);
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        command_arguments.extend(["-s".to_owned(), serial]);
    }
    command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    run_command(runner, "adb", &command_arguments)
        .await
        .map(|output| {
            command_output(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )
        })
}

async fn run_adb_command(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    let command_arguments = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
    run_command(runner, "adb", &command_arguments)
        .await
        .map(|output| {
            command_output(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )
        })
}

fn parse_display_size(dimensions: &str) -> anyhow::Result<(f64, f64)> {
    let (width, height) = dimensions
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("invalid Android display size: {dimensions}"))?;
    let dimensions = (width.parse::<f64>()?, height.parse::<f64>()?);
    anyhow::ensure!(
        dimensions.0.is_finite()
            && dimensions.1.is_finite()
            && dimensions.0 >= 1.0
            && dimensions.1 >= 1.0
            && dimensions.0 <= 8192.0
            && dimensions.1 <= 8192.0,
        "invalid Android display dimensions"
    );
    Ok(dimensions)
}

async fn run_adb_bytes(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut command_arguments = Vec::with_capacity(arguments.len() + 2);
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        command_arguments.extend(["-s".to_owned(), serial]);
    }
    command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    let output = runner
        .run("adb", &command_arguments)
        .await
        .map_err(|error| actionable_command_error("adb", error))?;
    if !output.success {
        let output_message = command_output(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        );
        if let Some(error) = actionable_device_failure("adb", &output_message) {
            return Err(error);
        }
        return Err(anyhow::anyhow!(
            "adb failed with {}: {}",
            output.status,
            output_message
        ));
    }
    Ok(output.stdout)
}

async fn run_command(
    runner: &dyn CommandRunner,
    command: &str,
    arguments: &[String],
) -> anyhow::Result<CommandOutput> {
    let output = runner
        .run(command, arguments)
        .await
        .map_err(|error| actionable_command_error(command, error))?;
    let standard_output = String::from_utf8_lossy(&output.stdout);
    let standard_error = String::from_utf8_lossy(&output.stderr);
    if !output.success {
        let output_message = command_output(&standard_output, &standard_error);
        if let Some(error) = actionable_device_failure(command, &output_message) {
            return Err(error);
        }
        return Err(anyhow::anyhow!(
            "{command} failed with {}: {}",
            output.status,
            output_message
        ));
    }
    Ok(output)
}

fn actionable_command_error(command: &str, error: anyhow::Error) -> anyhow::Error {
    let is_missing_command = error
        .downcast_ref::<io::Error>()
        .is_some_and(|io_error| io_error.kind() == io::ErrorKind::NotFound);
    if !is_missing_command {
        return error;
    }

    let message = match command {
        "adb" => {
            "`adb` was not found. Install Android SDK Platform-Tools, add its ".to_owned()
                + "platform-tools directory to PATH, and retry."
        }
        "npx" => {
            "`npx` was not found. Install Node.js 24.21.0 or newer, ensure ".to_owned()
                + "npx is on PATH, and retry."
        }
        "xcrun" => {
            "`xcrun` was not found. Install Xcode Command Line Tools with ".to_owned()
                + "`xcode-select --install`, then retry."
        }
        _ => format!("`{command}` was not found. Install it and ensure it is on PATH, then retry."),
    };
    anyhow::anyhow!(message)
}

fn actionable_device_failure(command: &str, output: &str) -> Option<anyhow::Error> {
    if command == "adb"
        && (output.contains("no devices/emulators found") || output.contains("device offline"))
    {
        return Some(anyhow::anyhow!(
            "No usable Android device was found. Start an Android Emulator or connect a device, "
                .to_owned()
                + "then retry. Set ANDROID_SERIAL when more than one device is available."
        ));
    }

    if command == "npx" && output.contains("could not determine executable to run") {
        return Some(anyhow::anyhow!(
            "`serve-sim` could not be started through npx. Install Node.js 24.21.0 or newer, "
                .to_owned()
                + "ensure npx is on PATH, and retry."
        ));
    }

    None
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

fn gesture_payload(gesture_type: &str, x: f64, y: f64) -> String {
    format!(r#"{{"type":"{gesture_type}","x":{x},"y":{y}}}"#)
}

fn command_output(standard_output: &str, standard_error: &str) -> String {
    let output = standard_output.trim();
    if !output.is_empty() {
        return output.to_owned();
    }
    standard_error.trim().to_owned()
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

    use async_trait::async_trait;
    use rmcp::handler::server::wrapper::Parameters;

    use super::{
        CaptureParameters, CommandOutput, CommandRunner, DeviceSimulatorMcp, Platform,
        SwipeParameters, TapParameters, TypeParameters, actionable_command_error,
        actionable_device_failure, command_output, escape_android_text, gesture_payload,
        parse_display_size, parse_platform, status_device, swipe_device, tap_device,
        type_on_device, validate_coordinates, validate_screenshot_name,
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
        async fn run(&self, command: &str, arguments: &[String]) -> anyhow::Result<CommandOutput> {
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
            async fn run(&self, _: &str, arguments: &[String]) -> anyhow::Result<CommandOutput> {
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
