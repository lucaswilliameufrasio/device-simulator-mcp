use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use anyhow::Context;
use plist::Value;

const MAX_PROFILE_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProvisioningProfile {
    uuid: String,
    team_identifiers: Vec<String>,
    platforms: Vec<String>,
    application_identifier: String,
    provisioned_devices: Vec<String>,
    allows_debugging: bool,
    expiration_date: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MatchingProfile {
    uuid: String,
    exact_bundle_match: bool,
    expiration_date: SystemTime,
}

pub(super) fn validate_local_signing_profiles(
    team_identifier: &str,
    device_udid: &str,
    runner_bundle_identifier: &str,
    test_runner_bundle_identifier: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "macos"),
        "local Xcode-managed signing profiles are supported only on macOS"
    );

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is required to locate Xcode-managed profiles"))?;
    let profile_directories = [
        home.join("Library/Developer/Xcode/UserData/Provisioning Profiles"),
        home.join("Library/MobileDevice/Provisioning Profiles"),
    ];
    let profiles = read_profiles(&profile_directories)?;
    let current_time = SystemTime::now();
    if find_matching_profile(
        &profiles,
        team_identifier,
        device_udid,
        runner_bundle_identifier,
        current_time,
    )
    .is_none()
    {
        return Err(missing_profile_error("runner app"));
    }
    if find_matching_profile(
        &profiles,
        team_identifier,
        device_udid,
        test_runner_bundle_identifier,
        current_time,
    )
    .is_none()
    {
        return Err(missing_profile_error("XCTest runner app"));
    }

    Ok(())
}

fn missing_profile_error(target: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no valid local provisioning profile was found for the {target}; set IOS_RUNNER_SIGNING_MODE=local, run `device-simulator-mcp prepare-ios-runner --team-id <TEAM_ID>`, open its project in Xcode, select the connected iPhone, and choose Product > Build For > Testing (do not choose Product > Test)"
    )
}

fn read_profiles(profile_directories: &[PathBuf]) -> anyhow::Result<Vec<ProvisioningProfile>> {
    let mut profiles = Vec::new();
    for profile_directory in profile_directories {
        let directory_entries = match fs::read_dir(profile_directory) {
            Ok(directory_entries) => directory_entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "reading Xcode provisioning profile directory {}",
                        profile_directory.display()
                    )
                });
            }
        };
        for directory_entry in directory_entries {
            let directory_entry = directory_entry?;
            let file_type = directory_entry.file_type()?;
            if !file_type.is_file() || !is_profile_file(&directory_entry.path()) {
                continue;
            }
            let profile = decode_profile(&directory_entry.path())?;
            profiles.push(profile);
        }
    }
    Ok(profiles)
}

fn is_profile_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("mobileprovision" | "provisionprofile")
    )
}

fn decode_profile(path: &Path) -> anyhow::Result<ProvisioningProfile> {
    let metadata =
        fs::metadata(path).with_context(|| "reading Xcode provisioning profile metadata")?;
    anyhow::ensure!(
        metadata.len() <= MAX_PROFILE_FILE_BYTES,
        "Xcode provisioning profile exceeded the size limit"
    );

    let output = Command::new("security")
        .args(["cms", "-D", "-i"])
        .arg(path)
        .output()
        .context("decoding an Xcode-managed provisioning profile with the macOS security tool")?;
    anyhow::ensure!(
        output.status.success(),
        "macOS could not decode an Xcode-managed provisioning profile"
    );
    anyhow::ensure!(
        output.stdout.len() as u64 <= MAX_PROFILE_FILE_BYTES,
        "decoded provisioning profile exceeded the size limit"
    );
    let value: Value = plist::from_bytes(&output.stdout)
        .context("parsing an Xcode-managed provisioning profile")?;
    parse_profile(&value)
}

fn parse_profile(value: &Value) -> anyhow::Result<ProvisioningProfile> {
    let dictionary = value
        .as_dictionary()
        .context("provisioning profile root must be a dictionary")?;
    let uuid = dictionary
        .get("UUID")
        .and_then(Value::as_string)
        .context("provisioning profile has no UUID")?
        .to_owned();
    anyhow::ensure!(
        !uuid.is_empty()
            && uuid.len() <= 64
            && uuid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "provisioning profile has an invalid UUID"
    );

    let team_identifiers = dictionary
        .get("TeamIdentifier")
        .and_then(Value::as_array)
        .context("provisioning profile has no team identifier list")?
        .iter()
        .filter_map(Value::as_string)
        .map(str::to_owned)
        .collect();
    let platforms = dictionary
        .get("Platform")
        .and_then(Value::as_array)
        .context("provisioning profile has no platform list")?
        .iter()
        .filter_map(Value::as_string)
        .map(str::to_owned)
        .collect();
    let provisioned_devices = dictionary
        .get("ProvisionedDevices")
        .and_then(Value::as_array)
        .context("provisioning profile has no registered device list")?
        .iter()
        .filter_map(Value::as_string)
        .map(str::to_owned)
        .collect();
    let expiration_date = dictionary
        .get("ExpirationDate")
        .and_then(Value::as_date)
        .context("provisioning profile has no expiration date")?;
    let entitlements = dictionary
        .get("Entitlements")
        .and_then(Value::as_dictionary)
        .context("provisioning profile has no entitlements")?;
    let application_identifier = entitlements
        .get("application-identifier")
        .and_then(Value::as_string)
        .context("provisioning profile has no application identifier entitlement")?
        .to_owned();
    let allows_debugging = entitlements
        .get("get-task-allow")
        .and_then(Value::as_boolean)
        .unwrap_or(false);

    Ok(ProvisioningProfile {
        uuid,
        team_identifiers,
        platforms,
        application_identifier,
        provisioned_devices,
        allows_debugging,
        expiration_date: expiration_date.into(),
    })
}

fn find_matching_profile(
    profiles: &[ProvisioningProfile],
    team_identifier: &str,
    device_udid: &str,
    bundle_identifier: &str,
    current_time: SystemTime,
) -> Option<MatchingProfile> {
    profiles
        .iter()
        .filter_map(|profile| {
            matching_profile(
                profile,
                team_identifier,
                device_udid,
                bundle_identifier,
                current_time,
            )
        })
        .max_by(compare_matching_profiles)
}

fn matching_profile(
    profile: &ProvisioningProfile,
    team_identifier: &str,
    device_udid: &str,
    bundle_identifier: &str,
    current_time: SystemTime,
) -> Option<MatchingProfile> {
    if !profile.allows_debugging
        || !profile.platforms.iter().any(|platform| platform == "iOS")
        || profile.expiration_date <= current_time
        || !profile
            .team_identifiers
            .iter()
            .any(|profile_team| profile_team == team_identifier)
        || !profile
            .provisioned_devices
            .iter()
            .any(|profile_device| profile_device.eq_ignore_ascii_case(device_udid))
    {
        return None;
    }

    let (_, application_pattern) = profile.application_identifier.split_once('.')?;
    let exact_bundle_match = application_pattern == bundle_identifier;
    if !exact_bundle_match && !wildcard_matches(application_pattern, bundle_identifier) {
        return None;
    }

    Some(MatchingProfile {
        uuid: profile.uuid.clone(),
        exact_bundle_match,
        expiration_date: profile.expiration_date,
    })
}

fn wildcard_matches(application_pattern: &str, bundle_identifier: &str) -> bool {
    if application_pattern == "*" {
        return true;
    }
    if let Some(prefix) = application_pattern.strip_suffix(".*") {
        return bundle_identifier
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('.'));
    }
    false
}

fn compare_matching_profiles(left: &MatchingProfile, right: &MatchingProfile) -> Ordering {
    left.exact_bundle_match
        .cmp(&right.exact_bundle_match)
        .then_with(|| left.expiration_date.cmp(&right.expiration_date))
        .then_with(|| left.uuid.cmp(&right.uuid))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn profile(
        uuid: &str,
        application_identifier: &str,
        expiration_seconds: u64,
    ) -> ProvisioningProfile {
        ProvisioningProfile {
            uuid: uuid.to_owned(),
            team_identifiers: vec!["ABCDEFGHIJ".to_owned()],
            platforms: vec!["iOS".to_owned()],
            application_identifier: application_identifier.to_owned(),
            provisioned_devices: vec!["00008110-001234567890001E".to_owned()],
            allows_debugging: true,
            expiration_date: UNIX_EPOCH + Duration::from_secs(expiration_seconds),
        }
    }

    #[test]
    fn selects_exact_valid_profile_for_team_and_device() {
        let profiles = [
            profile(
                "WILDCARD-UUID",
                "ABCDEFGHIJ.org.devicesimulatormcp.*",
                5_000,
            ),
            profile(
                "EXACT-UUID",
                "ABCDEFGHIJ.org.devicesimulatormcp.runner.teamabcdefghij",
                4_000,
            ),
        ];
        let selected = find_matching_profile(
            &profiles,
            "ABCDEFGHIJ",
            "00008110-001234567890001E",
            "org.devicesimulatormcp.runner.teamabcdefghij",
            UNIX_EPOCH + Duration::from_secs(1_000),
        )
        .unwrap();

        assert_eq!(selected.uuid, "EXACT-UUID");
    }

    #[test]
    fn rejects_profiles_for_another_team_device_or_expired_date() {
        let valid_profile = profile(
            "PROFILE-UUID",
            "ABCDEFGHIJ.org.devicesimulatormcp.runner.teamabcdefghij",
            2_000,
        );
        let current_time = UNIX_EPOCH + Duration::from_secs(1_000);

        assert!(
            find_matching_profile(
                std::slice::from_ref(&valid_profile),
                "KLMNOPQRST",
                "00008110-001234567890001E",
                "org.devicesimulatormcp.runner.teamabcdefghij",
                current_time,
            )
            .is_none()
        );
        assert!(
            find_matching_profile(
                std::slice::from_ref(&valid_profile),
                "ABCDEFGHIJ",
                "another-device",
                "org.devicesimulatormcp.runner.teamabcdefghij",
                current_time,
            )
            .is_none()
        );
        assert!(
            find_matching_profile(
                &[valid_profile],
                "ABCDEFGHIJ",
                "00008110-001234567890001E",
                "org.devicesimulatormcp.runner.teamabcdefghij",
                UNIX_EPOCH + Duration::from_secs(2_000),
            )
            .is_none()
        );
    }

    #[test]
    fn matches_personal_team_wildcard_profiles_only_for_subbundles() {
        assert!(wildcard_matches("org.example.*", "org.example.runner"));
        assert!(wildcard_matches("*", "org.example.runner"));
        assert!(!wildcard_matches("org.example.*", "org.example"));
        assert!(!wildcard_matches("org.example.*", "org.examples.runner"));
    }

    #[test]
    fn parses_development_profile_metadata_without_exposing_other_fields() {
        let encoded_profile = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>UUID</key><string>PROFILE-UUID</string>
<key>TeamIdentifier</key><array><string>ABCDEFGHIJ</string></array>
<key>Platform</key><array><string>iOS</string></array>
<key>ProvisionedDevices</key><array><string>00008110-001234567890001E</string></array>
<key>ExpirationDate</key><date>2099-01-01T00:00:00Z</date>
<key>Entitlements</key><dict>
<key>application-identifier</key><string>ABCDEFGHIJ.org.example.runner</string>
<key>get-task-allow</key><true/>
</dict></dict></plist>"#;
        let value: Value = plist::from_bytes(encoded_profile).unwrap();
        let profile = parse_profile(&value).unwrap();

        assert_eq!(profile.uuid, "PROFILE-UUID");
        assert_eq!(profile.team_identifiers, ["ABCDEFGHIJ"]);
        assert_eq!(
            profile.application_identifier,
            "ABCDEFGHIJ.org.example.runner"
        );
        assert!(profile.allows_debugging);
    }
}
