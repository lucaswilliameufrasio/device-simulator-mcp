use std::{env, fs, path::PathBuf};

use anyhow::{Context, bail};

use crate::cli::PrepareIosRunnerArgs;

const GENERATED_MARKER: &str = "device-simulator-mcp-personal-team-runner";

pub(crate) fn run(arguments: PrepareIosRunnerArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "macos"),
        "prepare-ios-runner requires macOS and Xcode"
    );
    super::validate_team_id(&arguments.team_id)?;

    let output_directory = match arguments.output_dir {
        Some(output_directory) if output_directory.is_absolute() => output_directory,
        Some(output_directory) => env::current_dir()?.join(output_directory),
        None => default_output_directory(&arguments.team_id)?,
    };
    let marker_contents = format!(
        "{GENERATED_MARKER}\n{}\n{}\n",
        env!("CARGO_PKG_VERSION"),
        arguments.team_id
    );

    if output_directory.exists() {
        if is_prepared_project(&output_directory, &marker_contents)? {
            println!("Existing Xcode signing project left unchanged:");
            print_next_steps(&output_directory);
            return Ok(());
        }
        bail!(
            "output directory already exists and was left untouched: {}",
            output_directory.display()
        );
    }

    let parent_directory = output_directory
        .parent()
        .filter(|parent_directory| !parent_directory.as_os_str().is_empty())
        .context("output directory must have a parent")?;
    fs::create_dir_all(parent_directory).with_context(|| {
        format!(
            "creating persistent Xcode project parent directory {}",
            parent_directory.display()
        )
    })?;
    let staged_directory = tempfile::Builder::new()
        .prefix(".ios-runner-signing-")
        .tempdir_in(parent_directory)
        .context("creating a staging directory beside the persistent Xcode project")?;
    super::write_runner_project_to(staged_directory.path(), &arguments.team_id)?;
    fs::write(
        staged_directory
            .path()
            .join(".device-simulator-mcp-signing-project"),
        marker_contents,
    )?;
    fs::rename(staged_directory.path(), &output_directory).with_context(|| {
        format!(
            "publishing the prepared Xcode project at {}",
            output_directory.display()
        )
    })?;

    println!("Prepared the Xcode-managed signing project:");
    print_next_steps(&output_directory);
    Ok(())
}

fn default_output_directory(team_id: &str) -> anyhow::Result<PathBuf> {
    let home_directory = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is required; pass --output-dir explicitly"))?;
    Ok(home_directory
        .join("Library/Application Support/device-simulator-mcp/ios-runner/signing")
        .join(env!("CARGO_PKG_VERSION"))
        .join(team_id))
}

fn is_prepared_project(directory: &std::path::Path, expected_marker: &str) -> anyhow::Result<bool> {
    let marker_path = directory.join(".device-simulator-mcp-signing-project");
    let project_path = directory.join("DeviceRunner.xcodeproj/project.pbxproj");
    if !marker_path.is_file() || !project_path.is_file() {
        return Ok(false);
    }
    Ok(fs::read_to_string(marker_path)? == expected_marker)
}

fn print_next_steps(output_directory: &std::path::Path) {
    println!(
        "  Project: {}",
        output_directory.join("DeviceRunner.xcodeproj").display()
    );
    println!("  1. Open the project in Xcode and select the connected iPhone as destination.");
    println!("  2. Choose Product > Build For > Testing to let Xcode create local profiles.");
    println!("  3. Do not choose Product > Test; the runner test intentionally stays active.");
    println!("  4. After the build succeeds, start the iPhone from the MCP again.");
    println!("  This only prepares signing; it does not send device input.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_custom_project_directory_without_overwriting_existing_content() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let output_directory = temporary_directory.path().join("runner-project");
        fs::create_dir(&output_directory).unwrap();
        fs::write(output_directory.join("keep.txt"), "user content").unwrap();

        let marker = format!(
            "{GENERATED_MARKER}\n{}\nABCDEFGHIJ\n",
            env!("CARGO_PKG_VERSION")
        );
        assert!(!is_prepared_project(&output_directory, &marker).unwrap());
        assert_eq!(
            fs::read_to_string(output_directory.join("keep.txt")).unwrap(),
            "user content"
        );
    }

    #[test]
    fn recognizes_an_existing_project_only_when_its_marker_matches() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let project_directory = temporary_directory.path();
        let project_file = project_directory.join("DeviceRunner.xcodeproj/project.pbxproj");
        fs::create_dir_all(project_file.parent().unwrap()).unwrap();
        fs::write(&project_file, "generated project").unwrap();
        let marker_path = project_directory.join(".device-simulator-mcp-signing-project");
        let expected_marker = format!(
            "{GENERATED_MARKER}\n{}\nABCDEFGHIJ\n",
            env!("CARGO_PKG_VERSION")
        );
        fs::write(&marker_path, &expected_marker).unwrap();

        assert!(is_prepared_project(project_directory, &expected_marker).unwrap());
        assert!(!is_prepared_project(project_directory, "different marker").unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn creates_and_reuses_a_team_scoped_persistent_signing_project() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let output_directory = temporary_directory.path().join("prepared-runner");
        let arguments = PrepareIosRunnerArgs {
            team_id: "ABCDEFGHIJ".to_owned(),
            output_dir: Some(output_directory.clone()),
        };

        run(arguments).unwrap();

        let project_file = output_directory.join("DeviceRunner.xcodeproj/project.pbxproj");
        let original_project = fs::read_to_string(&project_file).unwrap();
        assert!(original_project.contains("DEVELOPMENT_TEAM = ABCDEFGHIJ;"));
        assert!(original_project.contains("org.devicesimulatormcp.runner.teamabcdefghij"));
        assert!(original_project.contains("org.devicesimulatormcp.tests.teamabcdefghij"));

        run(PrepareIosRunnerArgs {
            team_id: "ABCDEFGHIJ".to_owned(),
            output_dir: Some(output_directory),
        })
        .unwrap();
        assert_eq!(fs::read_to_string(project_file).unwrap(), original_project);
    }
}
