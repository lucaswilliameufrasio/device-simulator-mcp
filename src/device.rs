//! Target discovery and immutable per-call device selection.
use std::{path::PathBuf, sync::Arc};

use crate::{ios_device, platform, process::CommandRunner};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionSource {
    Override,
    Instance,
    LegacyEnvironment,
}

impl SelectionSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Override => "call_override",
            Self::Instance => "instance_default",
            Self::LegacyEnvironment => "legacy_environment",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IosSimulatorBackend {
    Cli,
    ServeSim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceTarget {
    IosSimulator {
        udid: String,
        name: String,
        availability: String,
        backend: IosSimulatorBackend,
    },
    IosDevice {
        udid: String,
        name: String,
        availability: String,
        wda_url: Option<reqwest::Url>,
    },
    AndroidAdb {
        serial: String,
        name: String,
        availability: String,
    },
    AndroidGrpc {
        endpoint: String,
        token_file: PathBuf,
    },
}

#[derive(Clone, Default)]
pub(crate) struct DeviceSelection {
    selected: Arc<tokio::sync::RwLock<Option<DeviceTarget>>>,
}

impl DeviceSelection {
    pub(crate) async fn selected(&self) -> Option<DeviceTarget> {
        self.selected.read().await.clone()
    }

    pub(crate) async fn set(&self, target: Option<DeviceTarget>) {
        *self.selected.write().await = target;
    }
}

impl DeviceTarget {
    pub(crate) fn id(&self) -> String {
        match self {
            Self::IosSimulator { udid, .. } => format!("ios-simulator:{udid}"),
            Self::IosDevice { udid, .. } => format!("ios-device:{udid}"),
            Self::AndroidAdb { serial, .. } => format!("android:{serial}"),
            Self::AndroidGrpc { endpoint, .. } => format!("android-grpc:{endpoint}"),
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::IosSimulator { .. } => "ios_simulator",
            Self::IosDevice { .. } => "ios_device",
            Self::AndroidAdb { .. } | Self::AndroidGrpc { .. } => "android",
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::IosSimulator { name, .. }
            | Self::IosDevice { name, .. }
            | Self::AndroidAdb { name, .. } => name,
            Self::AndroidGrpc { endpoint, .. } => endpoint,
        }
    }

    pub(crate) fn availability(&self) -> &str {
        match self {
            Self::IosSimulator { availability, .. }
            | Self::IosDevice { availability, .. }
            | Self::AndroidAdb { availability, .. } => availability,
            Self::AndroidGrpc { .. } => "configured_not_probed",
        }
    }

    pub(crate) fn backend_name(&self) -> &'static str {
        match self {
            Self::IosSimulator {
                backend: IosSimulatorBackend::Cli,
                ..
            } => "cli",
            Self::IosSimulator {
                backend: IosSimulatorBackend::ServeSim,
                ..
            } => "serve-sim",
            Self::IosDevice { .. } => "devicectl",
            Self::AndroidAdb { .. } => "adb",
            Self::AndroidGrpc { .. } => "grpc",
        }
    }

    pub(crate) fn cache_key(&self) -> String {
        match self {
            Self::AndroidGrpc { token_file, .. } => {
                format!(
                    "{}:{}:{}",
                    self.id(),
                    self.backend_name(),
                    token_file.display()
                )
            }
            _ => format!("{}:{}", self.id(), self.backend_name()),
        }
    }

    pub(crate) fn ios_simulator_udid(&self) -> Option<&str> {
        match self {
            Self::IosSimulator { udid, .. } => Some(udid),
            _ => None,
        }
    }

    pub(crate) fn is_persistent_ios(&self) -> bool {
        matches!(
            self,
            Self::IosSimulator {
                backend: IosSimulatorBackend::ServeSim,
                ..
            }
        )
    }

    pub(crate) fn has_wda(&self) -> bool {
        matches!(
            self,
            Self::IosDevice {
                wda_url: Some(_),
                ..
            }
        )
    }

    pub(crate) fn ios_device_configuration(&self) -> Option<(&str, Option<&reqwest::Url>)> {
        match self {
            Self::IosDevice { udid, wda_url, .. } => Some((udid, wda_url.as_ref())),
            _ => None,
        }
    }

    pub(crate) fn android_grpc_configuration(&self) -> Option<(&str, &std::path::Path)> {
        match self {
            Self::AndroidGrpc {
                endpoint,
                token_file,
            } => Some((endpoint, token_file.as_path())),
            _ => None,
        }
    }

    pub(crate) fn platform(&self) -> crate::platform::Platform {
        match self {
            Self::IosSimulator { .. } | Self::IosDevice { .. } => crate::platform::Platform::Ios,
            Self::AndroidAdb { .. } | Self::AndroidGrpc { .. } => {
                crate::platform::Platform::Android
            }
        }
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id(),
            "kind": self.kind(),
            "name": self.name(),
            "availability": self.availability(),
            "backend": self.backend_name(),
        })
    }
}

pub(crate) async fn discover(runner: &dyn CommandRunner) -> (Vec<DeviceTarget>, Vec<String>) {
    let mut devices = Vec::new();
    let mut errors = Vec::new();

    match platform::discover_ios_simulators(runner).await {
        Ok(mut discovered) => devices.append(&mut discovered),
        Err(error) => errors.push(format!("ios_simulator: {error}")),
    }
    match ios_device::discover_devices(runner).await {
        Ok(mut discovered) => devices.append(&mut discovered),
        Err(error) => errors.push(format!("ios_device: {error}")),
    }
    match platform::discover_android_devices(runner).await {
        Ok(mut discovered) => devices.append(&mut discovered),
        Err(error) => errors.push(format!("android: {error}")),
    }
    match android_grpc_from_environment() {
        Ok(Some(target)) => devices.push(target),
        Ok(None) => {}
        Err(error) => errors.push(format!("android_grpc: {error}")),
    }
    (devices, errors)
}

pub(crate) async fn resolve(
    requested_id: Option<&str>,
    selection: &DeviceSelection,
    runner: &dyn CommandRunner,
) -> anyhow::Result<(DeviceTarget, SelectionSource)> {
    if let Some(requested_id) = requested_id {
        anyhow::ensure!(
            !requested_id.trim().is_empty(),
            "device_id must not be empty"
        );
        let (devices, errors) = discover(runner).await;
        if let Some(target) = devices
            .into_iter()
            .find(|device| device.id() == requested_id)
        {
            return Ok((target, SelectionSource::Override));
        }
        anyhow::bail!(
            "device_id '{requested_id}' was not discovered; call device_list to inspect available targets{}",
            if errors.is_empty() {
                String::new()
            } else {
                format!(" (discovery errors: {})", errors.join("; "))
            }
        );
    }

    if let Some(target) = selection.selected().await {
        return Ok((target, SelectionSource::Instance));
    }

    let target = legacy_target()?.ok_or_else(|| {
        anyhow::anyhow!("no device is selected; call device_list and device_select")
    })?;
    Ok((target, SelectionSource::LegacyEnvironment))
}

pub(crate) fn legacy_target() -> anyhow::Result<Option<DeviceTarget>> {
    match platform::configured_platform()? {
        platform::Platform::Ios => legacy_ios_target(),
        platform::Platform::Android => legacy_android_target(),
    }
}

fn legacy_ios_target() -> anyhow::Result<Option<DeviceTarget>> {
    match std::env::var("DEVICE_IOS_TARGET").as_deref() {
        Ok("device") => {
            let Ok(udid) = std::env::var("IOS_DEVICE_UDID") else {
                return Ok(None);
            };
            anyhow::ensure!(
                !udid.trim().is_empty() && udid.len() <= 128,
                "IOS_DEVICE_UDID must contain between 1 and 128 characters"
            );
            Ok(Some(DeviceTarget::IosDevice {
                udid,
                name: "Configured iPhone".to_owned(),
                availability: "configured_not_probed".to_owned(),
                wda_url: std::env::var("IOS_WDA_URL")
                    .ok()
                    .map(|value| ios_device::validate_wda_url(&value))
                    .transpose()?,
            }))
        }
        Err(_) | Ok("simulator") => {
            let backend = ios_simulator_backend()?;
            let Ok(udid) = std::env::var("IOS_SIMULATOR_UDID") else {
                return Ok(None);
            };
            crate::ios::validate_udid(&udid)?;
            Ok(Some(DeviceTarget::IosSimulator {
                udid,
                name: "Configured iOS Simulator".to_owned(),
                availability: "configured_not_probed".to_owned(),
                backend,
            }))
        }
        Ok(_) => anyhow::bail!("DEVICE_IOS_TARGET must be simulator or device"),
    }
}

fn legacy_android_target() -> anyhow::Result<Option<DeviceTarget>> {
    match std::env::var("DEVICE_ANDROID_BACKEND").as_deref() {
        Err(_) | Ok("adb") => {
            let Ok(serial) = std::env::var("ANDROID_SERIAL") else {
                return Ok(None);
            };
            anyhow::ensure!(
                !serial.trim().is_empty() && serial.len() <= 128,
                "ANDROID_SERIAL must contain between 1 and 128 characters"
            );
            Ok(Some(DeviceTarget::AndroidAdb {
                serial,
                name: "Configured Android device".to_owned(),
                availability: "configured_not_probed".to_owned(),
            }))
        }
        Ok("grpc") => android_grpc_from_environment(),
        Ok(_) => anyhow::bail!("DEVICE_ANDROID_BACKEND must be adb or grpc"),
    }
}

fn android_grpc_from_environment() -> anyhow::Result<Option<DeviceTarget>> {
    if std::env::var("DEVICE_ANDROID_BACKEND").as_deref() != Ok("grpc") {
        return Ok(None);
    }
    let endpoint = std::env::var("ANDROID_GRPC_ENDPOINT")
        .map_err(|_| anyhow::anyhow!("Android gRPC requires ANDROID_GRPC_ENDPOINT"))?;
    crate::ios::local_url(&endpoint)?;
    let token_file = std::env::var_os("ANDROID_GRPC_TOKEN_FILE").ok_or_else(|| {
        anyhow::anyhow!(
            "Android gRPC requires ANDROID_GRPC_TOKEN_FILE; authentication is never disabled"
        )
    })?;
    Ok(Some(DeviceTarget::AndroidGrpc {
        endpoint,
        token_file: token_file.into(),
    }))
}

pub(crate) fn ios_simulator_backend() -> anyhow::Result<IosSimulatorBackend> {
    match std::env::var("DEVICE_IOS_BACKEND").as_deref() {
        Err(_) | Ok("cli") => Ok(IosSimulatorBackend::Cli),
        Ok("serve-sim") => Ok(IosSimulatorBackend::ServeSim),
        Ok(_) => anyhow::bail!("DEVICE_IOS_BACKEND must be cli or serve-sim"),
    }
}

pub(crate) fn physical_target(
    udid: String,
    name: String,
    availability: String,
    wda_url: Option<reqwest::Url>,
) -> DeviceTarget {
    DeviceTarget::IosDevice {
        udid,
        name,
        availability,
        wda_url,
    }
}

pub(crate) fn android_adb_target(
    serial: String,
    name: String,
    availability: String,
) -> DeviceTarget {
    DeviceTarget::AndroidAdb {
        serial,
        name,
        availability,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::process::CommandOutput;

    #[derive(Default)]
    struct InventoryRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl InventoryRunner {
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for InventoryRunner {
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
            let output = match (command, arguments.first().map(String::as_str)) {
                ("xcrun", Some("simctl")) => {
                    r#"{"devices":{"iOS 26.0":[{"udid":"11111111-1111-4111-8111-111111111111","name":"iPhone Simulator","state":"Booted","isAvailable":true},{"udid":"22222222-2222-4222-8222-222222222222","name":"Unavailable Simulator","state":"Shutdown","isAvailable":false}]}}"#
                }
                ("xcrun", Some("devicectl")) => {
                    r#"{"result":{"devices":[{"identifier":"physical-iphone-123","deviceProperties":{"name":"Test iPhone"},"connectionProperties":{"tunnelState":"connected","pairingState":"paired"}}]}}"#
                }
                ("adb", _) => {
                    "List of devices attached\nemulator-5554 device model:Pixel_8\nphone-123 unauthorized model:Test_Phone"
                }
                _ => anyhow::bail!("unexpected inventory command: {command} {arguments:?}"),
            };
            Ok(CommandOutput {
                success: true,
                status: "exit status: 0".to_owned(),
                stdout: output.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn discovery_lists_explicit_targets_and_availability_with_fake_commands() {
        let runner = InventoryRunner::default();
        let simulators = crate::platform::discover_ios_simulators_with_backend(
            &runner,
            IosSimulatorBackend::Cli,
        )
        .await
        .unwrap();
        let physical_devices = ios_device::discover_devices_with_wda(&runner, None)
            .await
            .unwrap();
        let android_devices = crate::platform::discover_android_devices(&runner)
            .await
            .unwrap();

        assert_eq!(simulators.len(), 2);
        assert_eq!(
            simulators[0].id(),
            "ios-simulator:11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(simulators[0].availability(), "booted");
        assert_eq!(simulators[1].availability(), "unavailable");
        assert_eq!(physical_devices.len(), 1);
        assert_eq!(physical_devices[0].id(), "ios-device:physical-iphone-123");
        assert_eq!(physical_devices[0].availability(), "available");
        assert_eq!(android_devices.len(), 2);
        assert_eq!(android_devices[0].id(), "android:emulator-5554");
        assert_eq!(android_devices[0].name(), "Pixel 8");
        assert_eq!(android_devices[1].availability(), "unauthorized");
        assert_eq!(runner.calls().len(), 3);
    }

    #[tokio::test]
    async fn call_override_precedes_instance_selection_and_selection_is_shared() {
        let runner = InventoryRunner::default();
        let selection = DeviceSelection::default();
        let shared_selection = selection.clone();
        let selected_target = android_adb_target(
            "selected-serial".to_owned(),
            "Selected Android device".to_owned(),
            "available".to_owned(),
        );
        selection.set(Some(selected_target)).await;

        let (override_target, override_source) =
            resolve(Some("android:emulator-5554"), &shared_selection, &runner)
                .await
                .unwrap();
        let (default_target, default_source) = resolve(None, &selection, &runner).await.unwrap();

        assert_eq!(override_source, SelectionSource::Override);
        assert_eq!(override_target.id(), "android:emulator-5554");
        assert_eq!(default_source, SelectionSource::Instance);
        assert_eq!(default_target.id(), "android:selected-serial");
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn stable_ids_are_scoped_by_device_kind() {
        let simulator = DeviceTarget::IosSimulator {
            udid: "same-identifier".to_owned(),
            name: "Simulator".to_owned(),
            availability: "booted".to_owned(),
            backend: IosSimulatorBackend::Cli,
        };
        let physical_device = DeviceTarget::IosDevice {
            udid: "same-identifier".to_owned(),
            name: "iPhone".to_owned(),
            availability: "available".to_owned(),
            wda_url: None,
        };

        assert_eq!(simulator.id(), "ios-simulator:same-identifier");
        assert_eq!(physical_device.id(), "ios-device:same-identifier");
        assert_ne!(simulator.id(), physical_device.id());
    }

    #[test]
    fn grpc_backend_cache_keys_include_the_credential_file_configuration() {
        let first_target = DeviceTarget::AndroidGrpc {
            endpoint: "http://127.0.0.1:8554".to_owned(),
            token_file: PathBuf::from("/credentials/first-token"),
        };
        let second_target = DeviceTarget::AndroidGrpc {
            endpoint: "http://127.0.0.1:8554".to_owned(),
            token_file: PathBuf::from("/credentials/second-token"),
        };

        assert_eq!(first_target.id(), second_target.id());
        assert_ne!(first_target.cache_key(), second_target.cache_key());
    }
}
