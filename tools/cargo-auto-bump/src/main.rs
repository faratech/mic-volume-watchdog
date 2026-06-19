use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(name = "cargo-auto-bump", version, about = "Aggressively refresh Cargo dependencies and rebuild.")]
struct Cli {
    /// Path to a Cargo.toml manifest, or a crate directory.
    #[arg(long)]
    manifest_or_dir: Option<PathBuf>,

    /// Build target triple (e.g. x86_64-pc-windows-msvc).
    #[arg(long)]
    target: Option<String>,

    /// Build profile. Defaults to release.
    #[arg(long, default_value = "release")]
    profile: String,

    /// Include all workspace members when updating and building.
    #[arg(long)]
    workspace: bool,

    /// Aggressive duplicate reconciliation pass count. Set to 0 to skip.
    #[arg(long, default_value_t = 3)]
    aggressive_passes: u8,

    /// Copy built artifact to this location.
    #[arg(long)]
    copy_to: Option<PathBuf>,

    /// Do not copy the artifact.
    #[arg(long)]
    no_copy: bool,

    /// Print commands without running them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    resolve: Option<Resolve>,
}

#[derive(Debug, Deserialize)]
struct Package {
    id: String,
    name: String,
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Resolve {
    root: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let project_root = resolve_project_root(cli.manifest_or_dir.as_deref())?;
    println!("target project: {}", project_root.display());

    run_update(&project_root, &cli)?;

    let mut duplicate_roots = scan_duplicate_tree(&project_root, &cli)?;
    if duplicate_roots.is_empty() {
        println!("duplicate scan: no duplicate crate versions detected");
    } else {
        println!("duplicate scan detected:");
        for (name, versions) in &duplicate_roots {
            let mut version_list = versions.iter().cloned().collect::<Vec<_>>();
            version_list.sort_unstable();
            println!("  - {name} => {}", version_list.join(", "));
        }
    }

    if cli.aggressive_passes > 0 {
        duplicate_roots = reconcile_duplicate_trees(&project_root, &cli, duplicate_roots)?;
    } else {
        println!("aggressive reconciliation disabled via --aggressive-passes 0");
    }

    if !duplicate_roots.is_empty() {
        println!("post-reconcile duplicate crate roots remain (non-fatal):");
        for (name, versions) in &duplicate_roots {
            let mut version_list = versions.iter().cloned().collect::<Vec<_>>();
            version_list.sort_unstable();
            println!("  - {name} => {}", version_list.join(", "));
        }
    }

    run_build(&project_root, &cli)?;

    if !cli.no_copy {
        copy_artifact(&project_root, &cli)?;
    } else {
        println!("skipping copy (--no-copy)");
    }

    Ok(())
}

fn resolve_project_root(manifest_or_dir: Option<&Path>) -> Result<PathBuf> {
    let hinted_dir = manifest_or_dir.unwrap_or_else(|| Path::new("."));

    if !hinted_dir.exists() {
        bail!("manifest-or-dir path does not exist: {}", hinted_dir.display());
    }

    let candidate = if hinted_dir.is_file() {
        hinted_dir.parent().unwrap_or_else(|| Path::new("."))
    } else {
        hinted_dir
    };

    candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve project root: {}", candidate.display()))
}

fn run_update(project: &Path, cli: &Cli) -> Result<()> {
    let mut args = vec!["update".to_string()];
    add_workspace(&mut args, cli.workspace);
    add_manifest_path(&mut args, cli.manifest_or_dir.as_deref());
    run_command_status("cargo", &args, project, cli.dry_run)?;
    Ok(())
}

fn scan_duplicate_tree(project: &Path, cli: &Cli) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut args = vec!["tree".to_string(), "--all-targets".to_string(), "-d".to_string()];
    add_workspace(&mut args, cli.workspace);
    add_manifest_path(&mut args, cli.manifest_or_dir.as_deref());

    let output = run_command_capture("cargo", &args, project, cli.dry_run)?;
    Ok(summarize_tree_duplicates(&output))
}

fn reconcile_duplicate_trees(
    project: &Path,
    cli: &Cli,
    mut duplicates: BTreeMap<String, BTreeSet<String>>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    if duplicates.is_empty() {
        return Ok(duplicates);
    }

    let mut previous_state: Option<BTreeMap<String, BTreeSet<String>>> = None;

    for pass in 1..=cli.aggressive_passes {
        println!("aggressive reconcile pass {pass}/{}", cli.aggressive_passes);
        let duplicate_names = duplicates.keys().cloned().collect::<Vec<_>>();
        if duplicate_names.is_empty() {
            println!("no duplicate roots left; stopping aggressive reconcile");
            break;
        }

        for dep in duplicate_names {
            let latest = match latest_crate_version(&dep, project, cli.dry_run) {
                Ok(version) => version,
                Err(err) => {
                    println!("  {dep}: failed to resolve latest version ({err})");
                    continue;
                }
            };

            let current_versions = duplicates.get(&dep).cloned().unwrap_or_default();
            if current_versions.contains(&latest) {
                continue;
            }

            let mut precise = vec![
                "update".to_string(),
                "--precise".to_string(),
                latest.clone(),
                "--package".to_string(),
                dep.clone(),
            ];
            add_workspace(&mut precise, cli.workspace);
            add_manifest_path(&mut precise, cli.manifest_or_dir.as_deref());

            if run_command_status("cargo", &precise, project, cli.dry_run).is_ok() {
                println!("  forced {dep} => {latest}");
            } else {
                println!("  unable to force {dep} => {latest} in this pass");

                let mut fallback = vec!["update".to_string(), "--package".to_string(), dep.clone()];
                add_workspace(&mut fallback, cli.workspace);
                add_manifest_path(&mut fallback, cli.manifest_or_dir.as_deref());

                if run_command_status("cargo", &fallback, project, cli.dry_run).is_ok() {
                    println!("  fallback update succeeded for {dep}");
                } else {
                    println!("  fallback update also failed for {dep}");
                }
            }
        }

        duplicates = scan_duplicate_tree(project, cli)?;
        if duplicates.is_empty() {
            println!("aggressive reconcile eliminated duplicate roots");
            break;
        }
        if previous_state.as_ref() == Some(&duplicates) {
            println!("aggressive reconcile stopped: no progress in duplicate state");
            break;
        }
        previous_state = Some(duplicates.clone());
        println!("remaining duplicates: {}", duplicates.len());
    }

    Ok(duplicates)
}

fn latest_crate_version(name: &str, project: &Path, dry_run: bool) -> Result<String> {
    if dry_run {
        return Ok("0.0.0".to_string());
    }

    let output = run_command_capture("cargo", &["info".to_string(), name.to_string()], project, false)?;
    output
        .lines()
        .find_map(|line| {
            let line = line.trim();
            if line.starts_with("version: ") {
                Some(line.trim_start_matches("version: ").to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("no version output from `cargo info` for {name}"))
}

fn run_build(project: &Path, cli: &Cli) -> Result<()> {
    let mut args = vec!["build".to_string()];

    if cli.profile == "release" {
        args.push("--release".to_string());
    } else if cli.profile != "debug" {
        args.push("--profile".to_string());
        args.push(cli.profile.clone());
    }

    if let Some(target) = &cli.target {
        args.push("--target".to_string());
        args.push(target.clone());
    }

    add_workspace(&mut args, cli.workspace);
    add_manifest_path(&mut args, cli.manifest_or_dir.as_deref());

    run_command_status("cargo", &args, project, cli.dry_run)?;
    Ok(())
}

fn copy_artifact(project: &Path, cli: &Cli) -> Result<()> {
    let Some(copy_to) = &cli.copy_to else {
        return Ok(());
    };

    let metadata = cargo_metadata(project, cli)?;

    let mut root_id = metadata
        .resolve
        .as_ref()
        .and_then(|r| r.root.as_deref())
        .map(|s| s.to_string());
    if root_id.is_none() {
        root_id = metadata.workspace_members.first().cloned();
    }

    let root_package = root_id
        .and_then(|id| metadata.packages.iter().find(|p| p.id == id))
        .or_else(|| metadata.packages.first())
        .context("failed to determine root package from cargo metadata")?;

    let target_dir = if let Some(target) = &cli.target {
        project
            .join("target")
            .join(target)
            .join(profile_dir(&cli.profile))
    } else {
        project.join("target").join(profile_dir(&cli.profile))
    };

    let bin_candidates: Vec<String> = root_package
        .targets
        .iter()
        .filter(|target| target.kind.iter().any(|k| k == "bin"))
        .map(|target| target.name.clone())
        .collect();
    let bin_candidates = if bin_candidates.is_empty() {
        vec![root_package.name.clone()]
    } else {
        bin_candidates
    };

    let artifact_name = locate_artifact_file(&target_dir, &bin_candidates, is_windows_target(cli.target.as_deref()))
        .or_else(|| locate_artifact_file(&target_dir, std::slice::from_ref(&root_package.name), is_windows_target(cli.target.as_deref())))
        .context("could not find built artifact to copy")?;

    let destination = if copy_to.is_dir() {
        copy_to.join(
            artifact_name
                .file_name()
                .context("artifact path missing file name")?,
        )
    } else {
        copy_to.clone()
    };

    if let Some(parent) = destination.parent() {
        if !cli.dry_run {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create destination directory {}", parent.display())
            })?;
        }
    }

    if cli.dry_run {
        println!(
            "[dry-run] would copy {} -> {}",
            artifact_name.display(),
            destination.display()
        );
        return Ok(());
    }

    let _ = fs::remove_file(&destination);
    fs::copy(&artifact_name, &destination).with_context(|| {
        format!(
            "failed to copy artifact {:?} -> {:?}",
            artifact_name, destination
        )
    })?;

    println!("copied artifact to {}", destination.display());
    Ok(())
}

fn cargo_metadata(project: &Path, cli: &Cli) -> Result<CargoMetadata> {
    let mut args = vec![
        "metadata".to_string(),
        "--no-deps".to_string(),
        "--format-version".to_string(),
        "1".to_string(),
    ];
    add_manifest_path(&mut args, cli.manifest_or_dir.as_deref());

    let output = run_command_capture("cargo", &args, project, cli.dry_run)?;
    serde_json::from_str::<CargoMetadata>(&output)
        .context("failed to parse cargo metadata JSON")
}

fn summarize_tree_duplicates(output: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut seen: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for line in output.lines() {
        let clean = line.trim_start_matches(|c| matches!(c, '│' | '├' | '└' | '─' | ' '));
        let clean = clean.trim();
        if clean.is_empty() || clean.starts_with("warning:") {
            continue;
        }

        let Some((name, rest)) = clean.split_once(" v") else {
            continue;
        };
        let version = match rest.split_whitespace().next() {
            Some(v) => v.trim().to_string(),
            None => continue,
        };
        if version.is_empty() {
            continue;
        }

        seen.entry(name.to_string()).or_default().insert(version);
    }

    seen.retain(|_, versions| versions.len() > 1);
    seen
}

fn locate_artifact_file(target_dir: &Path, names: &[String], is_windows: bool) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for name in names {
        let base = target_dir.join(name);
        candidates.push(base.clone());
        if is_windows && base.extension().is_none() {
            candidates.push(base.with_extension("exe"));
        }
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn is_windows_target(target: Option<&str>) -> bool {
    target.map(|t| t.contains("windows")).unwrap_or(cfg!(target_os = "windows"))
}

fn profile_dir(profile: &str) -> String {
    if profile == "release" {
        "release".to_string()
    } else if profile == "debug" {
        "debug".to_string()
    } else {
        profile.to_string()
    }
}

fn add_workspace(args: &mut Vec<String>, workspace: bool) {
    if workspace {
        args.push("--workspace".to_string());
    }
}

fn add_manifest_path(args: &mut Vec<String>, manifest_or_dir: Option<&Path>) {
    if let Some(path) = manifest_or_dir {
        if path.is_file() {
            args.push("--manifest-path".to_string());
            args.push(path.to_string_lossy().to_string());
        }
    }
}

fn run_command_status(cmd: &str, args: &[String], dir: &Path, dry_run: bool) -> Result<()> {
    run_command(cmd, args, dir, dry_run, false).map(|_| ())
}

fn run_command_capture(cmd: &str, args: &[String], dir: &Path, dry_run: bool) -> Result<String> {
    run_command(cmd, args, dir, dry_run, true)
}

fn run_command(cmd: &str, args: &[String], dir: &Path, dry_run: bool, capture: bool) -> Result<String> {
    let cmdline = format!("{} {}", cmd, args.join(" "));
    println!("> {cmdline}");

    if dry_run {
        return Ok(String::new());
    }

    let mut command = Command::new(cmd);
    command.args(args).current_dir(dir);

    if capture {
        let output = command
            .output()
            .with_context(|| format!("failed running command: {cmdline}"))?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let mut msg = format!("command failed with status {}: {cmdline}", output.status);
            if !stdout.is_empty() {
                msg.push_str(&format!("\nstdout:\n{stdout}"));
            }
            if !stderr.is_empty() {
                msg.push_str(&format!("\nstderr:\n{stderr}"));
            }
            bail!(msg);
        }

        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }

    let status = command.status().with_context(|| format!("failed running command: {cmdline}"))?;
    if !status.success() {
        bail!("command failed with status {status}: {cmdline}");
    }

    Ok(String::new())
}
