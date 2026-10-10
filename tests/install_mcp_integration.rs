//! Native CLI registration in disposable homes and projects, never real configs.
use std::{fs, path::Path, process::Command};

const CLIENTS: [&str; 5] = ["claude-code", "codex", "cursor", "gemini-cli", "opencode"];

fn registration(root: &Path, client: &str, scope: &str, platform: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_device-simulator-mcp"));
    command
        .current_dir(root.join("project"))
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .args([
            "install-mcp",
            "--client",
            client,
            "--scope",
            scope,
            "--platform",
            platform,
        ]);
    command
}

fn config_path(root: &Path, client: &str, scope: &str) -> std::path::PathBuf {
    let base = root.join(if scope == "user" { "home" } else { "project" });
    match (client, scope) {
        ("claude-code", "user") => base.join(".claude.json"),
        ("claude-code", _) => base.join(".mcp.json"),
        ("codex", _) => base.join(".codex/config.toml"),
        ("cursor", _) => base.join(".cursor/mcp.json"),
        ("gemini-cli", _) => base.join(".gemini/settings.json"),
        ("opencode", "user") => root.join("xdg/opencode/opencode.json"),
        ("opencode", _) => base.join("opencode.json"),
        _ => panic!("unknown fixture client"),
    }
}

fn assert_registered(contents: &str, client: &str, platform: &str) {
    let binary = env!("CARGO_BIN_EXE_device-simulator-mcp");
    if client == "codex" {
        let config = contents.parse::<toml_edit::DocumentMut>().unwrap();
        let server = &config["mcp_servers"]["device-simulator"];
        assert_eq!(server["command"].as_str(), Some(binary));
        assert_eq!(server["args"].as_array().unwrap().len(), 0);
        assert_eq!(server["env"]["DEVICE_PLATFORM"].as_str(), Some(platform));
    } else {
        let config: serde_json::Value = serde_json::from_str(contents).unwrap();
        if client == "opencode" {
            let server = &config["mcp"]["device-simulator"];
            assert_eq!(server["type"], "local");
            assert_eq!(server["command"], serde_json::json!([binary]));
            assert_eq!(server["environment"]["DEVICE_PLATFORM"], platform);
        } else {
            let server = &config["mcpServers"]["device-simulator"];
            assert_eq!(server["command"], binary);
            assert_eq!(server["args"], serde_json::json!([]));
            assert_eq!(server["env"]["DEVICE_PLATFORM"], platform);
        }
    }
}

#[test]
fn previews_and_registers_all_clients_scopes_and_platforms_idempotently() {
    for client in CLIENTS {
        for scope in ["user", "project"] {
            for platform in ["ios", "android"] {
                let temporary = tempfile::tempdir().unwrap();
                let root = temporary.path();
                fs::create_dir(root.join("project")).unwrap();
                let path = config_path(root, client, scope);
                let preview = registration(root, client, scope, platform)
                    .output()
                    .unwrap();
                assert!(preview.status.success(), "{client}/{scope}/{platform}");
                assert!(!path.exists(), "preview must not create config");
                assert!(!root.join("home").exists());
                assert!(!root.join("xdg").exists());
                let applied = registration(root, client, scope, platform)
                    .arg("--apply")
                    .output()
                    .unwrap();
                assert!(applied.status.success(), "{client}/{scope}/{platform}");
                let original = fs::read_to_string(&path).unwrap();
                assert_registered(&original, client, platform);
                let repeated = registration(root, client, scope, platform)
                    .arg("--apply")
                    .output()
                    .unwrap();
                assert!(repeated.status.success());
                assert_eq!(fs::read_to_string(&path).unwrap(), original);
                assert!(
                    String::from_utf8(repeated.stdout)
                        .unwrap()
                        .contains("Already configured")
                );
            }
        }
    }
}

#[test]
fn registration_preserves_other_servers_settings_and_explicit_paths_with_spaces() {
    for client in CLIENTS {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::create_dir(root.join("project")).unwrap();
        let path = root.join("project/custom config");
        let initial = if client == "codex" {
            "model = 'fixture-model'\n[mcp_servers.other]\ncommand = 'other-server'\n".to_owned()
        } else {
            let key = if client == "opencode" {
                "mcp"
            } else {
                "mcpServers"
            };
            serde_json::json!({"fixture_setting":true,key:{"other":{"command":"other-server"}}})
                .to_string()
        };
        fs::write(&path, &initial).unwrap();
        let preview = registration(root, client, "project", "android")
            .args(["--config-file", "custom config"])
            .output()
            .unwrap();
        assert!(preview.status.success());
        assert_eq!(fs::read_to_string(&path).unwrap(), initial);
        let applied = registration(root, client, "project", "android")
            .args(["--config-file", "custom config", "--apply"])
            .output()
            .unwrap();
        assert!(applied.status.success());
        let contents = fs::read_to_string(&path).unwrap();
        assert_registered(&contents, client, "android");
        if client == "codex" {
            let config = contents.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(config["model"].as_str(), Some("fixture-model"));
            assert_eq!(
                config["mcp_servers"]["other"]["command"].as_str(),
                Some("other-server")
            );
        } else {
            let config: serde_json::Value = serde_json::from_str(&contents).unwrap();
            let key = if client == "opencode" {
                "mcp"
            } else {
                "mcpServers"
            };
            assert_eq!(config["fixture_setting"], true);
            assert_eq!(config[key]["other"]["command"], "other-server");
        }
        assert!(!config_path(root, client, "project").exists());
    }
}

#[test]
fn registration_rejects_malformed_configs_and_missing_binaries_without_writes() {
    for client in CLIENTS {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::create_dir(root.join("project")).unwrap();
        let path = root.join("project/config fixture");
        let malformed = "not valid JSON or TOML [[[";
        fs::write(&path, malformed).unwrap();
        let rejected = registration(root, client, "project", "ios")
            .args(["--config-file", "config fixture", "--apply"])
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert_eq!(fs::read_to_string(&path).unwrap(), malformed);
        let rejected = registration(root, client, "project", "ios")
            .args(["--binary", "missing executable", "--apply"])
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert!(!config_path(root, client, "project").exists());
        assert!(!root.join("home").exists());
        assert!(!root.join("xdg").exists());
    }
}
