use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstRootNode},
};
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::cli::{Client, DevicePlatform, InstallMcpArgs, Scope};

const SERVER_NAME: &str = "device-simulator";

pub(crate) fn run(args: InstallMcpArgs) -> Result<()> {
    let config_path = resolve_config_path(&args)?;
    let binary_path = resolve_binary_path(args.binary.as_deref())?;

    if args.apply {
        let original = match fs::read_to_string(&config_path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", config_path.display()));
            }
        };
        let updated = match args.client {
            Client::Codex => update_codex_config(&original, &binary_path, args.platform)?,
            _ => update_json_config(args.client, &original, &binary_path, args.platform)?,
        };
        if original == updated {
            println!("Already configured in {}", config_path.display());
            return Ok(());
        }
        write_atomic(&config_path, &updated)?;
        println!("Installed {SERVER_NAME} in {}", config_path.display());
        println!("Restart the AI client to load the MCP server.");
        return Ok(());
    }

    println!(
        "Preview for {} ({})",
        client_name(args.client),
        config_path.display()
    );
    println!("Run again with --apply to write this configuration.\n");
    match args.client {
        Client::Codex => print_codex_preview(&binary_path, args.platform),
        Client::OpenCode => print_json_preview("mcp", opencode_entry(&binary_path, args.platform))?,
        Client::ClaudeCode | Client::Cursor | Client::GeminiCli => {
            print_json_preview("mcpServers", stdio_entry(&binary_path, args.platform))?
        }
    }
    Ok(())
}

fn resolve_config_path(args: &InstallMcpArgs) -> Result<PathBuf> {
    if let Some(path) = &args.config_file {
        if path.is_absolute() {
            return Ok(path.clone());
        }
        return Ok(env::current_dir()?.join(path));
    }

    let home = home_dir()?;
    let root = match args.scope {
        Scope::User => match args.client {
            Client::ClaudeCode => home.join(".claude.json"),
            Client::Codex => home.join(".codex/config.toml"),
            Client::Cursor => home.join(".cursor/mcp.json"),
            Client::GeminiCli => home.join(".gemini/settings.json"),
            Client::OpenCode => {
                let config_home = env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"));
                config_home.join("opencode/opencode.json")
            }
        },
        Scope::Project => {
            let project = env::current_dir()?;
            match args.client {
                Client::ClaudeCode => project.join(".mcp.json"),
                Client::Codex => project.join(".codex/config.toml"),
                Client::Cursor => project.join(".cursor/mcp.json"),
                Client::GeminiCli => project.join(".gemini/settings.json"),
                Client::OpenCode => project.join("opencode.json"),
            }
        }
    };
    Ok(root)
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("HOME/USERPROFILE is not set; pass --config-file with an explicit path")
}

fn resolve_binary_path(binary: Option<&Path>) -> Result<PathBuf> {
    let path = match binary {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => env::current_dir()?.join(path),
        None => env::current_exe().context("resolving the running executable path")?,
    };
    if !path.is_file() {
        bail!("MCP executable does not exist: {}", path.display());
    }
    Ok(path)
}

fn client_name(client: Client) -> &'static str {
    match client {
        Client::ClaudeCode => "Claude Code",
        Client::Codex => "Codex CLI",
        Client::Cursor => "Cursor",
        Client::GeminiCli => "Gemini CLI",
        Client::OpenCode => "OpenCode",
    }
}

fn stdio_entry(binary: &Path, platform: DevicePlatform) -> Value {
    json!({
        "command": binary.to_string_lossy(),
        "args": [],
        "env": { "DEVICE_PLATFORM": platform.as_str() }
    })
}

fn opencode_entry(binary: &Path, platform: DevicePlatform) -> Value {
    json!({
        "type": "local",
        "command": [binary.to_string_lossy()],
        "environment": { "DEVICE_PLATFORM": platform.as_str() }
    })
}

fn print_json_preview(root_key: &str, entry: Value) -> Result<()> {
    let mut servers = Map::new();
    servers.insert(SERVER_NAME.to_owned(), entry);
    let mut root = Map::new();
    root.insert(root_key.to_owned(), Value::Object(servers));
    println!("{}", serde_json::to_string_pretty(&Value::Object(root))?);
    Ok(())
}

fn print_codex_preview(binary: &Path, platform: DevicePlatform) {
    let mut document = DocumentMut::new();
    upsert_codex_entry(&mut document, binary, platform)
        .expect("building MCP config for Codex should not fail");
    print!("{document}");
}

fn update_json_config(
    client: Client,
    original: &str,
    binary: &Path,
    platform: DevicePlatform,
) -> Result<String> {
    let input = if original.trim().is_empty() {
        "{}"
    } else {
        original
    };
    let root = CstRootNode::parse(input, &ParseOptions::default())
        .context("parsing the existing MCP configuration as JSON/JSONC")?;
    let root_object = root
        .object_value_or_create()
        .context("MCP configuration root must be a JSON object")?;
    let (root_key, is_opencode) = match client {
        Client::OpenCode => ("mcp", true),
        Client::ClaudeCode | Client::Cursor | Client::GeminiCli => ("mcpServers", false),
        Client::Codex => bail!("Codex uses TOML, not JSON"),
    };
    let servers = root_object
        .object_value_or_create(root_key)
        .with_context(|| format!("`{root_key}` must be a JSON object"))?;
    let server = servers
        .object_value_or_create(SERVER_NAME)
        .context("the existing device-simulator entry must be a JSON object")?;
    if is_opencode {
        set_cst_value(&server, "type", CstInputValue::String("local".to_owned()));
        set_cst_value(
            &server,
            "command",
            CstInputValue::Array(vec![CstInputValue::String(
                binary.to_string_lossy().into_owned(),
            )]),
        );
        let environment = server
            .object_value_or_create("environment")
            .context("`environment` must be a JSON object")?;
        set_cst_value(
            &environment,
            "DEVICE_PLATFORM",
            CstInputValue::String(platform.as_str().to_owned()),
        );
    } else {
        set_cst_value(
            &server,
            "command",
            CstInputValue::String(binary.to_string_lossy().into_owned()),
        );
        set_cst_value(&server, "args", CstInputValue::Array(Vec::new()));
        let environment = server
            .object_value_or_create("env")
            .context("`env` must be a JSON object")?;
        set_cst_value(
            &environment,
            "DEVICE_PLATFORM",
            CstInputValue::String(platform.as_str().to_owned()),
        );
    }
    Ok(root.to_string())
}

fn set_cst_value(object: &CstObject, key: &str, value: CstInputValue) {
    if let Some(existing) = object.get(key) {
        existing.set_value(value);
    } else {
        object.append(key, value);
    }
}

fn update_codex_config(original: &str, binary: &Path, platform: DevicePlatform) -> Result<String> {
    let mut document: DocumentMut = if original.trim().is_empty() {
        DocumentMut::new()
    } else {
        original
            .parse()
            .context("parsing the existing Codex configuration as TOML")?
    };
    upsert_codex_entry(&mut document, binary, platform)?;
    Ok(document.to_string())
}

fn upsert_codex_entry(
    document: &mut DocumentMut,
    binary: &Path,
    platform: DevicePlatform,
) -> Result<()> {
    let mcp_servers = document
        .entry("mcp_servers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("`mcp_servers` must be a TOML table")?;

    let server = mcp_servers
        .entry(SERVER_NAME)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("the device-simulator entry must be a TOML table")?;
    server.insert("command", value(binary.to_string_lossy().as_ref()));
    let arguments = Array::new();
    server.insert("args", Item::Value(arguments.into()));
    let environment = server
        .entry("env")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("`env` must be a TOML table")?;
    environment.insert("DEVICE_PLATFORM", value(platform.as_str()));
    Ok(())
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let path = if path.exists() {
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?
    } else {
        path.to_path_buf()
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating a temporary file in {}", parent.display()))?;
    temporary
        .write_all(contents.as_bytes())
        .with_context(|| format!("writing temporary config for {}", path.display()))?;
    if let Ok(metadata) = fs::metadata(&path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .with_context(|| format!("preserving permissions for {}", path.display()))?;
    }
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    #[test]
    fn merges_jsonc_server_and_preserves_other_configuration() {
        let existing = r#"{
  // Keep this user's other MCP server.
  "mcpServers": {
    "other": { "command": "other-server" }
  }
}"#;

        let updated = update_json_config(
            Client::Cursor,
            existing,
            Path::new("/opt/device-simulator-mcp"),
            DevicePlatform::Ios,
        )
        .unwrap();

        assert!(updated.contains("// Keep this user's other MCP server."));
        assert!(updated.contains("\"other\""));
        assert!(updated.contains("\"device-simulator\""));
        assert!(updated.contains("/opt/device-simulator-mcp"));
        assert!(updated.contains("DEVICE_PLATFORM"));
    }

    #[test]
    fn generates_client_specific_json_roots() {
        let claude = update_json_config(
            Client::ClaudeCode,
            "{}",
            Path::new("/opt/device-simulator-mcp"),
            DevicePlatform::Ios,
        )
        .unwrap();
        let gemini = update_json_config(
            Client::GeminiCli,
            "{}",
            Path::new("/opt/device-simulator-mcp"),
            DevicePlatform::Ios,
        )
        .unwrap();

        assert!(claude.contains("\"mcpServers\""));
        assert!(gemini.contains("\"mcpServers\""));
        assert!(claude.contains("\"command\": \"/opt/device-simulator-mcp\""));
        assert!(gemini.contains("\"DEVICE_PLATFORM\": \"ios\""));
    }

    #[test]
    fn replacing_existing_entry_is_idempotent() {
        let original = update_json_config(
            Client::OpenCode,
            r#"{"mcp":{"device-simulator":{"enabled":false,"environment":{"ANDROID_SERIAL":"emulator-1"}}}}"#,
            Path::new("/opt/device-simulator-mcp"),
            DevicePlatform::Android,
        )
        .unwrap();

        let updated = update_json_config(
            Client::OpenCode,
            &original,
            Path::new("/opt/device-simulator-mcp"),
            DevicePlatform::Android,
        )
        .unwrap();

        assert_eq!(original, updated);
        assert!(updated.contains("\"enabled\":false"));
        assert!(updated.contains("\"ANDROID_SERIAL\":\"emulator-1\""));
    }

    #[test]
    fn merges_codex_server_without_removing_other_tables() {
        let original = "model = \"gpt\"\n\n[mcp_servers.other]\ncommand = \"other\"\n\n[mcp_servers.device-simulator.env]\nANDROID_SERIAL = \"emulator-1\"\n";

        let updated = update_codex_config(
            original,
            Path::new("/opt/device-simulator-mcp"),
            DevicePlatform::Android,
        )
        .unwrap();

        assert!(updated.contains("model = \"gpt\""));
        assert!(updated.contains("[mcp_servers.other]"));
        assert!(updated.contains("[mcp_servers.device-simulator]"));
        assert!(updated.contains("DEVICE_PLATFORM = \"android\""));
        assert!(updated.contains("ANDROID_SERIAL = \"emulator-1\""));
    }

    #[test]
    fn rejects_invalid_existing_configuration_without_replacing_it() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let config_path = temporary_directory.path().join("settings.json");
        fs::write(&config_path, "not-json").unwrap();
        let original = fs::read_to_string(&config_path).unwrap();
        let updated = update_json_config(
            Client::GeminiCli,
            &original,
            Path::new("/opt/device-simulator-mcp"),
            DevicePlatform::Ios,
        );

        assert!(updated.is_err());
        assert_eq!(fs::read_to_string(config_path).unwrap(), "not-json");
    }

    #[test]
    fn config_path_override_uses_relative_to_current_directory() {
        let args = InstallMcpArgs {
            client: Client::Cursor,
            scope: Scope::Project,
            platform: DevicePlatform::Ios,
            config_file: Some(PathBuf::from("custom/mcp.json")),
            binary: None,
            apply: false,
        };

        assert_eq!(
            resolve_config_path(&args).unwrap(),
            env::current_dir().unwrap().join("custom/mcp.json")
        );
    }

    #[test]
    fn apply_preserves_jsonc_comments_and_is_idempotent() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let config_path = temporary_directory.path().join("mcp.json");
        fs::write(
            &config_path,
            "{\n  // Existing client setting.\n  \"other\": true\n}\n",
        )
        .unwrap();
        let binary_path = temporary_directory.path().join("device-simulator-mcp");
        fs::write(&binary_path, "test executable").unwrap();

        run(InstallMcpArgs {
            client: Client::Cursor,
            scope: Scope::User,
            platform: DevicePlatform::Android,
            config_file: Some(config_path.clone()),
            binary: Some(binary_path.clone()),
            apply: true,
        })
        .unwrap();
        let installed = fs::read_to_string(&config_path).unwrap();
        run(InstallMcpArgs {
            client: Client::Cursor,
            scope: Scope::User,
            platform: DevicePlatform::Android,
            config_file: Some(config_path.clone()),
            binary: Some(binary_path),
            apply: true,
        })
        .unwrap();

        assert_eq!(fs::read_to_string(config_path).unwrap(), installed);
        assert!(installed.contains("// Existing client setting."));
        assert!(installed.contains("\"other\": true"));
        assert!(installed.contains("\"DEVICE_PLATFORM\": \"android\""));
    }
}
