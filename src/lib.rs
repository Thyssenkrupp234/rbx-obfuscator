use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, WeakDom,
};
use serde::Serialize;
use tempfile::{Builder as TempFileBuilder, NamedTempFile};

const SCRIPT_CLASSES: &[&str] = &["Script", "LocalScript", "ModuleScript"];
const PROMETHEUS_ENV_VAR: &str = "RBXL_OBFUSCATE_PROMETHEUS";
const PROMETHEUS_INSTALL_URL: &str =
    "https://raw.githubusercontent.com/prometheus-lua/Prometheus/master/install.sh";
const PROMETHEUS_TOOL_DIR: &str = ".tools/prometheus-lua";
const PROMETHEUS_HOME_DIR: &str = "home";
const PROMETHEUS_BIN_DIR: &str = "bin";
const PROMETHEUS_BIN_NAME: &str = "prometheus-lua";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObfuscationLevel {
    #[default]
    Minimal,
    Low,
    Medium,
    High,
}

impl ObfuscationLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

pub fn prometheus_preset_for_level(level: ObfuscationLevel) -> &'static str {
    match level {
        ObfuscationLevel::Minimal => "Weak",
        ObfuscationLevel::Low => "Weak",
        ObfuscationLevel::Medium => "Medium",
        ObfuscationLevel::High => "Strong",
    }
}

#[derive(Debug)]
pub struct Options {
    pub input: PathBuf,
    pub output: PathBuf,
    pub obfuscation_level: ObfuscationLevel,
    pub dry_run: bool,
    pub backup_dir: Option<PathBuf>,
    pub skip_paths: Vec<String>,
    pub manifest: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct ScriptCandidate {
    referent: Ref,
    path: String,
    class_name: String,
    source: String,
}

#[derive(Debug, Serialize)]
struct Manifest {
    input: PathBuf,
    output: Option<PathBuf>,
    dry_run: bool,
    backend: &'static str,
    prometheus_executable: PathBuf,
    prometheus_preset: &'static str,
    obfuscation_level: ObfuscationLevel,
    scripts: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize)]
struct ManifestEntry {
    path: String,
    class_name: String,
    action: ManifestAction,
    source_bytes: usize,
    transformed_bytes: Option<usize>,
    backup_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ManifestAction {
    Processed,
    Skipped,
    Failed,
    DryRun,
}

#[derive(Debug)]
struct FailedScript {
    path: String,
    class_name: String,
    error: String,
}

pub fn run(options: Options) -> Result<()> {
    validate_options(&options)?;
    let project_root = project_root_for_tools();
    let prometheus_path = resolve_or_install_prometheus(&project_root, options.dry_run)?;
    let prometheus_preset = prometheus_preset_for_level(options.obfuscation_level);

    if options.dry_run {
        eprintln!(
            "Using Prometheus preset {prometheus_preset} for --level {} (dry run; Prometheus executable is not launched)",
            options.obfuscation_level.as_str()
        );
    } else {
        eprintln!("Using Prometheus executable: {}", prometheus_path.display());
        eprintln!(
            "Using Prometheus preset {prometheus_preset} for --level {}",
            options.obfuscation_level.as_str()
        );
    }

    eprintln!("Loading RBXL: {}", options.input.display());
    let input = BufReader::new(
        File::open(&options.input)
            .with_context(|| format!("failed to open input RBXL {}", options.input.display()))?,
    );
    let mut dom = rbx_binary::from_reader(input)
        .with_context(|| format!("failed to read RBXL {}", options.input.display()))?;

    let skip_paths: HashSet<String> = options.skip_paths.iter().cloned().collect();
    let scripts = collect_scripts(&dom)?;
    eprintln!("Found {} script instance(s)", scripts.len());

    let mut manifest_entries = Vec::with_capacity(scripts.len());
    let mut processed = 0usize;
    let mut skipped = 0usize;
    let mut failed_scripts = Vec::new();
    let prometheus_temp_dir = if options.dry_run {
        None
    } else {
        Some(create_prometheus_temp_dir(&project_root)?)
    };

    if let Some(backup_dir) = &options.backup_dir {
        if !options.dry_run {
            fs::create_dir_all(backup_dir).with_context(|| {
                format!("failed to create backup directory {}", backup_dir.display())
            })?;
        }
    }

    for script in scripts {
        if skip_paths.contains(&script.path) {
            skipped += 1;
            eprintln!("Skipping {} ({})", script.path, script.class_name);
            manifest_entries.push(ManifestEntry {
                path: script.path,
                class_name: script.class_name,
                action: ManifestAction::Skipped,
                source_bytes: script.source.len(),
                transformed_bytes: None,
                backup_path: None,
                error: None,
            });
            continue;
        }

        if options.dry_run {
            processed += 1;
            eprintln!(
                "Would process {} ({}) with Prometheus preset {prometheus_preset}",
                script.path, script.class_name
            );
            manifest_entries.push(ManifestEntry {
                path: script.path,
                class_name: script.class_name,
                action: ManifestAction::DryRun,
                source_bytes: script.source.len(),
                transformed_bytes: None,
                backup_path: None,
                error: None,
            });
            continue;
        }

        let backup_path = if let Some(backup_dir) = &options.backup_dir {
            let backup_path = backup_path_for(backup_dir, &script.path);
            fs::write(&backup_path, &script.source)
                .with_context(|| format!("failed to write backup {}", backup_path.display()))?;
            Some(backup_path)
        } else {
            None
        };

        eprintln!("Processing {} ({})", script.path, script.class_name);
        let temp_dir = prometheus_temp_dir
            .as_ref()
            .expect("Prometheus temp dir must exist outside dry-run")
            .path();
        let transformed = match run_prometheus(
            &prometheus_path,
            &script.source,
            options.obfuscation_level,
            temp_dir,
        ) {
            Ok(transformed) => transformed,
            Err(error) => {
                let error = format!("{error:#}");
                let summary = first_error_line(&error).to_owned();
                eprintln!(
                    "Failed {}; leaving source unobfuscated: {}",
                    script.path, summary
                );
                failed_scripts.push(FailedScript {
                    path: script.path.clone(),
                    class_name: script.class_name.clone(),
                    error: summary,
                });
                manifest_entries.push(ManifestEntry {
                    path: script.path,
                    class_name: script.class_name,
                    action: ManifestAction::Failed,
                    source_bytes: script.source.len(),
                    transformed_bytes: None,
                    backup_path,
                    error: Some(error),
                });
                continue;
            }
        };

        let instance = dom
            .get_by_ref_mut(script.referent)
            .ok_or_else(|| anyhow!("script disappeared from DOM: {}", script.path))?;
        instance
            .properties
            .insert(ustr("Source"), Variant::String(transformed.clone()));

        processed += 1;
        manifest_entries.push(ManifestEntry {
            path: script.path,
            class_name: script.class_name,
            action: ManifestAction::Processed,
            source_bytes: script.source.len(),
            transformed_bytes: Some(transformed.len()),
            backup_path,
            error: None,
        });
    }

    if let Some(manifest_path) = &options.manifest {
        let manifest = Manifest {
            input: options.input.clone(),
            output: if options.dry_run {
                None
            } else {
                Some(options.output.clone())
            },
            dry_run: options.dry_run,
            backend: "prometheus",
            prometheus_executable: prometheus_path.clone(),
            prometheus_preset,
            obfuscation_level: options.obfuscation_level,
            scripts: manifest_entries,
        };
        write_manifest(manifest_path, &manifest)?;
        eprintln!("Wrote manifest: {}", manifest_path.display());
    }

    eprintln!(
        "Processed: {processed}, skipped: {skipped}, failed: {}",
        failed_scripts.len()
    );

    if !failed_scripts.is_empty() {
        eprintln!("Failed scripts left unobfuscated:");
        for failed in &failed_scripts {
            eprintln!(
                "  - {} ({}): {}",
                failed.path, failed.class_name, failed.error
            );
        }
    }

    if options.dry_run {
        eprintln!("Dry run complete; no RBXL output written");
        return Ok(());
    }

    write_rbxl(&options.output, &dom)?;

    eprintln!("Done");
    Ok(())
}

fn validate_options(options: &Options) -> Result<()> {
    if !options.input.exists() {
        bail!("input file does not exist: {}", options.input.display());
    }
    if !options.input.is_file() {
        bail!("input path is not a file: {}", options.input.display());
    }
    if same_path(&options.input, &options.output)? {
        bail!(
            "output must not overwrite input: {}",
            options.output.display()
        );
    }
    Ok(())
}

fn project_root_for_tools() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn resolve_or_install_prometheus(project_root: &Path, dry_run: bool) -> Result<PathBuf> {
    resolve_or_install_prometheus_with(project_root, dry_run, install_prometheus)
}

fn resolve_or_install_prometheus_with<F>(
    project_root: &Path,
    dry_run: bool,
    installer: F,
) -> Result<PathBuf>
where
    F: FnOnce(&Path) -> Result<PathBuf>,
{
    if let Some(path) = std::env::var_os(PROMETHEUS_ENV_VAR) {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            bail!("{PROMETHEUS_ENV_VAR} is set but empty");
        }
        if !dry_run {
            ensure_prometheus_executable(&path).with_context(|| {
                format!(
                    "{PROMETHEUS_ENV_VAR} points to an unusable Prometheus executable: {}",
                    path.display()
                )
            })?;
        }
        return Ok(path);
    }

    let local_path = project_local_prometheus_path(project_root);
    if dry_run {
        return Ok(local_path);
    }

    if local_path.exists() {
        ensure_prometheus_executable(&local_path).with_context(|| {
            format!(
                "repo-local Prometheus executable is not usable: {}",
                local_path.display()
            )
        })?;
        return Ok(local_path);
    }

    installer(project_root).with_context(|| {
        format!(
            "failed to install Prometheus locally under {}. Install curl or wget and rerun, or set {PROMETHEUS_ENV_VAR} to an existing prometheus-lua executable",
            prometheus_tool_dir(project_root).display()
        )
    })?;

    ensure_prometheus_executable(&local_path).with_context(|| {
        format!(
            "Prometheus was installed but the expected executable is not usable: {}",
            local_path.display()
        )
    })?;
    Ok(local_path)
}

fn project_local_prometheus_path(project_root: &Path) -> PathBuf {
    prometheus_tool_dir(project_root)
        .join(PROMETHEUS_BIN_DIR)
        .join(PROMETHEUS_BIN_NAME)
}

fn prometheus_tool_dir(project_root: &Path) -> PathBuf {
    project_root.join(PROMETHEUS_TOOL_DIR)
}

fn install_prometheus(project_root: &Path) -> Result<PathBuf> {
    let tool_dir = prometheus_tool_dir(project_root);
    let home_dir = tool_dir.join(PROMETHEUS_HOME_DIR);
    let bin_dir = tool_dir.join(PROMETHEUS_BIN_DIR);

    fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create Prometheus home {}", home_dir.display()))?;
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create Prometheus bin {}", bin_dir.display()))?;

    let installer = TempFileBuilder::new()
        .prefix("install-prometheus-")
        .suffix(".sh")
        .tempfile_in(&tool_dir)
        .with_context(|| {
            format!(
                "failed to create temporary Prometheus installer in {}",
                tool_dir.display()
            )
        })?;

    download_prometheus_installer(installer.path())?;

    let output = Command::new("sh")
        .arg(installer.path())
        .env("PROMETHEUS_LUA_HOME", &home_dir)
        .env("PROMETHEUS_LUA_BIN", &bin_dir)
        .current_dir(project_root)
        .output()
        .context("failed to launch Prometheus installer with sh")?;

    if !output.status.success() {
        bail!(
            "Prometheus installer exited with status {}. {}",
            output.status,
            command_output_summary(&output)
        );
    }

    Ok(bin_dir.join(PROMETHEUS_BIN_NAME))
}

fn create_prometheus_temp_dir(project_root: &Path) -> Result<tempfile::TempDir> {
    let temp_parent = prometheus_tool_dir(project_root).join("tmp");
    fs::create_dir_all(&temp_parent).with_context(|| {
        format!(
            "failed to create Prometheus temporary directory {}",
            temp_parent.display()
        )
    })?;

    TempFileBuilder::new()
        .prefix("rbxl-obfuscate-")
        .tempdir_in(&temp_parent)
        .with_context(|| {
            format!(
                "failed to create temporary Prometheus workspace in {}",
                temp_parent.display()
            )
        })
}

fn download_prometheus_installer(destination: &Path) -> Result<()> {
    let mut failures = Vec::new();

    match Command::new("curl")
        .arg("-fsSL")
        .arg(PROMETHEUS_INSTALL_URL)
        .arg("-o")
        .arg(destination)
        .output()
    {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => failures.push(format!(
            "curl failed with status {}. {}",
            output.status,
            command_output_summary(&output)
        )),
        Err(error) => failures.push(format!("curl could not be started: {error}")),
    }

    match Command::new("wget")
        .arg("-q")
        .arg("-O")
        .arg(destination)
        .arg(PROMETHEUS_INSTALL_URL)
        .output()
    {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => failures.push(format!(
            "wget failed with status {}. {}",
            output.status,
            command_output_summary(&output)
        )),
        Err(error) => failures.push(format!("wget could not be started: {error}")),
    }

    bail!(
        "could not download Prometheus installer from {PROMETHEUS_INSTALL_URL}. Install curl or wget, then rerun, or set {PROMETHEUS_ENV_VAR} to an existing prometheus-lua executable. Attempts: {}",
        failures.join("; ")
    )
}

fn ensure_prometheus_executable(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("Prometheus executable does not exist: {}", path.display());
    }
    if !path.is_file() {
        bail!("Prometheus executable is not a file: {}", path.display());
    }

    Command::new(path)
        .arg("--help")
        .output()
        .with_context(|| format!("failed to launch Prometheus executable {}", path.display()))?;

    Ok(())
}

fn collect_scripts(dom: &WeakDom) -> Result<Vec<ScriptCandidate>> {
    let mut scripts = Vec::new();
    let root_ref = dom.root_ref();
    let root = dom
        .get_by_ref(root_ref)
        .ok_or_else(|| anyhow!("DOM root is missing"))?;

    for child_ref in root.children().iter().copied() {
        collect_scripts_from(dom, child_ref, "game", &mut scripts)?;
    }

    Ok(scripts)
}

fn collect_scripts_from(
    dom: &WeakDom,
    referent: Ref,
    parent_path: &str,
    scripts: &mut Vec<ScriptCandidate>,
) -> Result<()> {
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("DOM contains missing child referent"))?;
    let path = child_path(parent_path, &instance.name);
    let class_name = instance.class.to_string();
    let children = instance.children().to_vec();

    if is_script_class(&class_name) {
        let source = match instance.properties.get(&ustr("Source")) {
            Some(Variant::String(source)) => source.clone(),
            Some(other) => bail!(
                "{} has Source property with unsupported type {:?}",
                path,
                other.ty()
            ),
            None => bail!("{} is missing Source property", path),
        };

        scripts.push(ScriptCandidate {
            referent,
            path: path.clone(),
            class_name,
            source,
        });
    }

    for child_ref in children {
        collect_scripts_from(dom, child_ref, &path, scripts)?;
    }

    Ok(())
}

fn is_script_class(class_name: &str) -> bool {
    SCRIPT_CLASSES.contains(&class_name)
}

fn run_prometheus(
    prometheus_path: &Path,
    input_source: &str,
    level: ObfuscationLevel,
    temp_dir: &Path,
) -> Result<String> {
    let input_file = TempFileBuilder::new()
        .prefix("rbxl-obfuscate-input-")
        .suffix(".luau")
        .tempfile_in(temp_dir)
        .with_context(|| {
            format!(
                "failed to create temporary Prometheus input in {}",
                temp_dir.display()
            )
        })?;
    fs::write(input_file.path(), input_source).with_context(|| {
        format!(
            "failed to write Prometheus input {}",
            input_file.path().display()
        )
    })?;

    let output_file = TempFileBuilder::new()
        .prefix("rbxl-obfuscate-output-")
        .suffix(".luau")
        .tempfile_in(temp_dir)
        .with_context(|| {
            format!(
                "failed to create temporary Prometheus output path in {}",
                temp_dir.display()
            )
        })?;
    let output_path = output_file.path().to_path_buf();
    drop(output_file);

    let status_output = Command::new(prometheus_path)
        .args(prometheus_args_for(level, &output_path, input_file.path()))
        .output()
        .with_context(|| {
            format!(
                "failed to launch Prometheus executable {}",
                prometheus_path.display()
            )
        })?;

    if !status_output.status.success() {
        bail!(
            "Prometheus exited with status {} using preset {}. {}",
            status_output.status,
            prometheus_preset_for_level(level),
            command_output_summary(&status_output)
        );
    }

    if !output_path.exists() {
        bail!(
            "Prometheus did not create output file {}",
            output_path.display()
        );
    }

    let transformed = fs::read_to_string(&output_path)
        .with_context(|| format!("failed to read Prometheus output {}", output_path.display()))?;
    if transformed.is_empty() {
        bail!(
            "Prometheus produced empty output file {} using preset {}",
            output_path.display(),
            prometheus_preset_for_level(level)
        );
    }

    let _ = fs::remove_file(&output_path);
    Ok(transformed)
}

fn prometheus_args_for(
    level: ObfuscationLevel,
    output_file: &Path,
    input_file: &Path,
) -> Vec<OsString> {
    vec![
        OsString::from("--preset"),
        OsString::from(prometheus_preset_for_level(level)),
        OsString::from("--LuaU"),
        OsString::from("--out"),
        output_file.as_os_str().to_owned(),
        OsString::from("--nocolors"),
        input_file.as_os_str().to_owned(),
    ]
}

fn command_output_summary(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("stdout: {} stderr: {}", stdout.trim(), stderr.trim())
}

fn first_error_line(error: &str) -> &str {
    error
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("unknown error")
        .trim()
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create manifest directory {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(manifest).context("failed to serialize manifest")?;
    fs::write(path, json).with_context(|| format!("failed to write manifest {}", path.display()))
}

fn write_rbxl(path: &Path, dom: &WeakDom) -> Result<()> {
    eprintln!("Writing RBXL: {}", path.display());
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create output directory {}", parent.display()))?;

    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary output in {}", parent.display()))?;
    {
        let output = BufWriter::new(&mut temp);
        let top_level_refs = dom.root().children().to_vec();
        rbx_binary::to_writer(output, dom, &top_level_refs)
            .with_context(|| format!("failed to write temporary RBXL for {}", path.display()))?;
    }
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to move temporary RBXL into {}", path.display()))?;
    Ok(())
}

fn same_path(input: &Path, output: &Path) -> Result<bool> {
    let input = input
        .canonicalize()
        .with_context(|| format!("failed to canonicalize input {}", input.display()))?;

    let output = if output.exists() {
        output
            .canonicalize()
            .with_context(|| format!("failed to canonicalize output {}", output.display()))?
    } else {
        absolute_lexical(output)?
    };

    Ok(input == output)
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to read current directory")?
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    Ok(normalized)
}

fn child_path(parent_path: &str, child_name: &str) -> String {
    format!("{parent_path}.{}", escape_path_segment(child_name))
}

fn escape_path_segment(segment: &str) -> String {
    segment.replace('\\', "\\\\").replace('.', "\\.")
}

fn backup_path_for(backup_dir: &Path, instance_path: &str) -> PathBuf {
    let hash = stable_hash(instance_path);
    let sanitized = sanitize_filename(instance_path);
    backup_dir.join(format!("{sanitized}-{hash:016x}.luau"))
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn sanitize_filename(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "script".to_owned()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, env, sync::Mutex};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    fn write_fake_executable(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn script_class_filter_is_exact() {
        assert!(is_script_class("Script"));
        assert!(is_script_class("LocalScript"));
        assert!(is_script_class("ModuleScript"));
        assert!(!is_script_class("Folder"));
        assert!(!is_script_class("ScriptClone"));
    }

    #[test]
    fn path_segments_escape_dots() {
        assert_eq!(
            child_path("game.ServerScriptService", "Main"),
            "game.ServerScriptService.Main"
        );
        assert_eq!(child_path("game", "A.B"), "game.A\\.B");
    }

    #[test]
    fn backup_paths_are_sanitized_and_stable() {
        let dir = Path::new("backups");
        let first = backup_path_for(dir, "game.ServerScriptService.Main");
        let second = backup_path_for(dir, "game.ServerScriptService.Main");
        assert_eq!(first, second);
        assert!(first.to_string_lossy().ends_with(".luau"));
        assert!(!first.file_name().unwrap().to_string_lossy().contains('/'));
    }

    #[test]
    fn filename_sanitizer_has_fallback() {
        assert_eq!(sanitize_filename(""), "script");
        assert_eq!(
            sanitize_filename("game.Service.Script"),
            "game.Service.Script"
        );
        assert_eq!(
            sanitize_filename("game.Service.Bad/Name"),
            "game.Service.Bad_Name"
        );
    }

    #[test]
    fn same_path_rejects_existing_input() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.rbxl");
        fs::write(&input, "").unwrap();

        assert!(same_path(&input, &input).unwrap());
        assert!(!same_path(&input, &dir.path().join("output.rbxl")).unwrap());
    }

    #[test]
    fn levels_map_to_prometheus_presets() {
        assert_eq!(
            prometheus_preset_for_level(ObfuscationLevel::Minimal),
            "Weak"
        );
        assert_eq!(prometheus_preset_for_level(ObfuscationLevel::Low), "Weak");
        assert_eq!(
            prometheus_preset_for_level(ObfuscationLevel::Medium),
            "Medium"
        );
        assert_eq!(
            prometheus_preset_for_level(ObfuscationLevel::High),
            "Strong"
        );
    }

    #[test]
    fn obfuscation_level_names_are_cli_values() {
        assert_eq!(ObfuscationLevel::Minimal.as_str(), "minimal");
        assert_eq!(ObfuscationLevel::Low.as_str(), "low");
        assert_eq!(ObfuscationLevel::Medium.as_str(), "medium");
        assert_eq!(ObfuscationLevel::High.as_str(), "high");
    }

    #[test]
    fn prometheus_command_args_match_expected_shape() {
        let args = prometheus_args_for(
            ObfuscationLevel::Medium,
            Path::new("output.luau"),
            Path::new("input.luau"),
        );

        assert_eq!(
            args,
            vec![
                OsString::from("--preset"),
                OsString::from("Medium"),
                OsString::from("--LuaU"),
                OsString::from("--out"),
                OsString::from("output.luau"),
                OsString::from("--nocolors"),
                OsString::from("input.luau"),
            ]
        );
    }

    #[test]
    fn dry_run_resolver_does_not_install_or_verify_prometheus() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var(PROMETHEUS_ENV_VAR);
        let dir = tempfile::tempdir().unwrap();
        let installer_called = Cell::new(false);

        let path = resolve_or_install_prometheus_with(dir.path(), true, |_| {
            installer_called.set(true);
            Err(anyhow!("installer should not run during dry-run"))
        })
        .unwrap();

        assert!(!installer_called.get());
        assert_eq!(path, project_local_prometheus_path(dir.path()));
        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn resolver_prefers_prometheus_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var(PROMETHEUS_ENV_VAR);
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("custom-prometheus-lua");
        let local_path = project_local_prometheus_path(dir.path());
        write_fake_executable(&env_path);
        write_fake_executable(&local_path);
        env::set_var(PROMETHEUS_ENV_VAR, &env_path);

        let resolved = resolve_or_install_prometheus_with(dir.path(), false, |_| {
            Err(anyhow!("installer should not run when env var is set"))
        })
        .unwrap();

        env::remove_var(PROMETHEUS_ENV_VAR);
        assert_eq!(resolved, env_path);
    }

    #[test]
    #[cfg(unix)]
    fn resolver_uses_repo_local_prometheus_binary() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var(PROMETHEUS_ENV_VAR);
        let dir = tempfile::tempdir().unwrap();
        let local_path = project_local_prometheus_path(dir.path());
        write_fake_executable(&local_path);

        let resolved = resolve_or_install_prometheus_with(dir.path(), false, |_| {
            Err(anyhow!("installer should not run when local binary exists"))
        })
        .unwrap();

        assert_eq!(resolved, local_path);
    }

    #[test]
    #[cfg(unix)]
    fn missing_local_binary_in_non_dry_run_attempts_install() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var(PROMETHEUS_ENV_VAR);
        let dir = tempfile::tempdir().unwrap();
        let installer_called = Cell::new(false);

        let resolved = resolve_or_install_prometheus_with(dir.path(), false, |root| {
            installer_called.set(true);
            let local_path = project_local_prometheus_path(root);
            write_fake_executable(&local_path);
            Ok(local_path)
        })
        .unwrap();

        assert!(installer_called.get());
        assert_eq!(resolved, project_local_prometheus_path(dir.path()));
    }

    #[test]
    fn failed_install_returns_clear_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var(PROMETHEUS_ENV_VAR);
        let dir = tempfile::tempdir().unwrap();

        let error = resolve_or_install_prometheus_with(dir.path(), false, |_| {
            Err(anyhow!("download failed in test"))
        })
        .unwrap_err();
        let error = format!("{error:#}");

        assert!(error.contains("failed to install Prometheus locally"));
        assert!(error.contains(".tools/prometheus-lua"));
        assert!(error.contains(PROMETHEUS_ENV_VAR));
    }

    #[test]
    fn tools_directory_is_ignored_by_git() {
        let gitignore = fs::read_to_string(".gitignore").unwrap();
        assert!(gitignore
            .lines()
            .map(str::trim)
            .any(|line| line == ".tools/" || line == "/.tools/"));
    }

    #[test]
    fn error_summary_uses_first_non_empty_line() {
        assert_eq!(
            first_error_line("\n\n Prometheus exited with status 1\nmore detail"),
            "Prometheus exited with status 1"
        );
        assert_eq!(first_error_line(""), "unknown error");
    }
}
