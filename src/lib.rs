use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs::{self, File},
    io::ErrorKind,
    io::{BufReader, BufWriter},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, WeakDom,
};
use serde::Serialize;
use tempfile::{Builder as TempFileBuilder, NamedTempFile};

const SCRIPT_CLASSES: &[&str] = &["Script", "LocalScript", "ModuleScript"];
const PROMETHEUS_COMMAND: &str = "prometheus-lua";
const PROMETHEUS_INSTALL_URL: &str =
    "https://raw.githubusercontent.com/prometheus-lua/Prometheus/master/install.sh";
const PROMETHEUS_INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/prometheus-lua/Prometheus/master/install.sh | sh";
const PROMETHEUS_UPDATE_STATE_FILE: &str = "prometheus-last-update";
const PROMETHEUS_UPDATE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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

    fn as_label(self) -> &'static str {
        match self {
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RobloxFileFormat {
    Rbxl,
    Rbxm,
}

impl RobloxFileFormat {
    fn from_path(path: &Path) -> Result<Self> {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("rbxl") => Ok(Self::Rbxl),
            Some("rbxm") => Ok(Self::Rbxm),
            _ => bail!(
                "unsupported input file extension for {}: expected .rbxl or .rbxm",
                path.display()
            ),
        }
    }

    fn as_name(self) -> &'static str {
        match self {
            Self::Rbxl => "RBXL",
            Self::Rbxm => "RBXM",
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
    pub output: Option<PathBuf>,
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
    input_format: RobloxFileFormat,
    output: PathBuf,
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
    let input_format = validate_input_format(&options.input)?;
    let output = match &options.output {
        Some(output) => output.clone(),
        None => default_output_path(&options.input, options.obfuscation_level)?,
    };
    validate_options(&options, &output)?;
    let prometheus_path = resolve_or_install_prometheus(options.dry_run)?;
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

    eprintln!(
        "Loading {}: {}",
        input_format.as_name(),
        options.input.display()
    );
    let input = BufReader::new(
        File::open(&options.input)
            .with_context(|| format!("failed to open input file {}", options.input.display()))?,
    );
    let mut dom = rbx_binary::from_reader(input).with_context(|| {
        format!(
            "failed to read Roblox binary file {}",
            options.input.display()
        )
    })?;

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
        Some(create_prometheus_temp_dir()?)
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
            input_format,
            output: output.clone(),
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
        eprintln!("Dry run complete; no Roblox binary output written");
        return Ok(());
    }

    write_roblox_binary(&output, &dom, input_format)?;

    eprintln!("Done");
    Ok(())
}

fn validate_input_format(input: &Path) -> Result<RobloxFileFormat> {
    RobloxFileFormat::from_path(input)
}

fn validate_options(options: &Options, output: &Path) -> Result<()> {
    if !options.input.exists() {
        bail!("input file does not exist: {}", options.input.display());
    }
    if !options.input.is_file() {
        bail!("input path is not a file: {}", options.input.display());
    }
    if same_path(&options.input, output)? {
        bail!("output must not overwrite input: {}", output.display());
    }
    Ok(())
}

fn default_output_path(input: &Path, level: ObfuscationLevel) -> Result<PathBuf> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("input path has no file name: {}", input.display()))?;
    let extension = input
        .extension()
        .ok_or_else(|| anyhow!("input path has no extension: {}", input.display()))?;

    let mut file_name = OsString::from(stem);
    file_name.push(format!("-obfuscated_{}.", level.as_label()));
    file_name.push(extension);
    Ok(input.with_file_name(file_name))
}

trait PrometheusRuntime {
    fn prometheus_available(&mut self) -> Result<bool>;
    fn install_prometheus(&mut self) -> Result<()>;
    fn update_prometheus(&mut self) -> Result<()>;
    fn internet_available(&mut self) -> bool;
    fn now(&self) -> SystemTime;
}

struct SystemPrometheusRuntime;

impl PrometheusRuntime for SystemPrometheusRuntime {
    fn prometheus_available(&mut self) -> Result<bool> {
        match Command::new(PROMETHEUS_COMMAND).arg("--help").output() {
            Ok(output) if output.status.success() => Ok(true),
            Ok(output) => bail!(
                "{PROMETHEUS_COMMAND} --help exited with status {}. {}",
                output.status,
                command_output_summary(&output)
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to launch {PROMETHEUS_COMMAND} --help"))
            }
        }
    }

    fn install_prometheus(&mut self) -> Result<()> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(PROMETHEUS_INSTALL_COMMAND)
            .output()
            .context("failed to launch Prometheus installer with sh")?;

        if !output.status.success() {
            bail!(
                "Prometheus installer exited with status {}. {}",
                output.status,
                command_output_summary(&output)
            );
        }

        Ok(())
    }

    fn update_prometheus(&mut self) -> Result<()> {
        let output = Command::new(PROMETHEUS_COMMAND)
            .arg("update")
            .output()
            .with_context(|| format!("failed to launch {PROMETHEUS_COMMAND} update"))?;

        if !output.status.success() {
            bail!(
                "{PROMETHEUS_COMMAND} update exited with status {}. {}",
                output.status,
                command_output_summary(&output)
            );
        }

        Ok(())
    }

    fn internet_available(&mut self) -> bool {
        Command::new("curl")
            .args(["-fsI", "--max-time", "5", PROMETHEUS_INSTALL_URL])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

fn resolve_or_install_prometheus(dry_run: bool) -> Result<PathBuf> {
    let state_file = prometheus_update_state_file();
    let mut runtime = SystemPrometheusRuntime;
    resolve_or_install_prometheus_with(dry_run, state_file.as_deref(), &mut runtime)
}

fn resolve_or_install_prometheus_with(
    dry_run: bool,
    state_file: Option<&Path>,
    runtime: &mut impl PrometheusRuntime,
) -> Result<PathBuf> {
    let prometheus_path = PathBuf::from(PROMETHEUS_COMMAND);
    if dry_run {
        return Ok(prometheus_path);
    }

    if !runtime.prometheus_available()? {
        eprintln!("{PROMETHEUS_COMMAND} not found on PATH; installing Prometheus");
        runtime.install_prometheus().with_context(|| {
            format!(
                "failed to install Prometheus with `{PROMETHEUS_INSTALL_COMMAND}`. Install curl and rerun, or install {PROMETHEUS_COMMAND} on PATH"
            )
        })?;

        if !runtime.prometheus_available()? {
            bail!("Prometheus installed, but {PROMETHEUS_COMMAND} is still not usable on PATH");
        }

        write_prometheus_update_timestamp(state_file, runtime.now())?;
        return Ok(prometheus_path);
    }

    maybe_update_prometheus(state_file, runtime)?;
    Ok(prometheus_path)
}

fn maybe_update_prometheus(
    state_file: Option<&Path>,
    runtime: &mut impl PrometheusRuntime,
) -> Result<()> {
    if !prometheus_update_is_stale(state_file, runtime.now())? {
        return Ok(());
    }

    if !runtime.internet_available() {
        eprintln!("Prometheus update check skipped; no internet connection detected");
        return Ok(());
    }

    eprintln!("Updating Prometheus installation");
    if let Err(update_error) = runtime.update_prometheus() {
        eprintln!(
            "{PROMETHEUS_COMMAND} update failed; retrying with installer: {}",
            first_error_line(&format!("{update_error:#}"))
        );
        runtime.install_prometheus().with_context(|| {
            format!(
                "failed to update Prometheus with `{PROMETHEUS_COMMAND} update` or `{PROMETHEUS_INSTALL_COMMAND}`"
            )
        })?;
    }

    if !runtime.prometheus_available()? {
        bail!("Prometheus updated, but {PROMETHEUS_COMMAND} is not usable on PATH");
    }

    write_prometheus_update_timestamp(state_file, runtime.now())
}

fn create_prometheus_temp_dir() -> Result<tempfile::TempDir> {
    TempFileBuilder::new()
        .prefix("rbxl-obfuscate-")
        .tempdir()
        .context("failed to create temporary Prometheus workspace")
}

fn prometheus_update_state_file() -> Option<PathBuf> {
    if let Some(state_home) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Some(
            PathBuf::from(state_home)
                .join("rbxl-obfuscate")
                .join(PROMETHEUS_UPDATE_STATE_FILE),
        );
    }

    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("rbxl-obfuscate")
                .join(PROMETHEUS_UPDATE_STATE_FILE)
        })
}

fn prometheus_update_is_stale(state_file: Option<&Path>, now: SystemTime) -> Result<bool> {
    let Some(state_file) = state_file else {
        return Ok(true);
    };
    let Some(last_update) = read_prometheus_update_timestamp(state_file)? else {
        return Ok(true);
    };
    Ok(now.duration_since(last_update).unwrap_or(Duration::ZERO) >= PROMETHEUS_UPDATE_INTERVAL)
}

fn read_prometheus_update_timestamp(path: &Path) -> Result<Option<SystemTime>> {
    let timestamp = match fs::read_to_string(path) {
        Ok(timestamp) => timestamp,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read Prometheus update state {}", path.display())
            })
        }
    };

    let Ok(seconds) = timestamp.trim().parse::<u64>() else {
        return Ok(None);
    };
    Ok(Some(UNIX_EPOCH + Duration::from_secs(seconds)))
}

fn write_prometheus_update_timestamp(state_file: Option<&Path>, now: SystemTime) -> Result<()> {
    let Some(state_file) = state_file else {
        return Ok(());
    };
    if let Some(parent) = state_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create Prometheus update state directory {}",
                parent.display()
            )
        })?;
    }
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    fs::write(state_file, format!("{seconds}\n")).with_context(|| {
        format!(
            "failed to write Prometheus update state {}",
            state_file.display()
        )
    })
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

fn write_roblox_binary(path: &Path, dom: &WeakDom, input_format: RobloxFileFormat) -> Result<()> {
    eprintln!("Writing {}: {}", input_format.as_name(), path.display());
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
        rbx_binary::to_writer(output, dom, &top_level_refs).with_context(|| {
            format!(
                "failed to write temporary Roblox binary for {}",
                path.display()
            )
        })?;
    }
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "failed to move temporary Roblox binary into {}",
                path.display()
            )
        })?;
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

    #[derive(Debug)]
    struct FakePrometheusRuntime {
        available: bool,
        internet_available: bool,
        install_fails: bool,
        update_fails: bool,
        now: SystemTime,
        availability_checks: usize,
        install_calls: usize,
        update_calls: usize,
        internet_checks: usize,
    }

    impl Default for FakePrometheusRuntime {
        fn default() -> Self {
            Self {
                available: true,
                internet_available: true,
                install_fails: false,
                update_fails: false,
                now: fixed_now(),
                availability_checks: 0,
                install_calls: 0,
                update_calls: 0,
                internet_checks: 0,
            }
        }
    }

    impl PrometheusRuntime for FakePrometheusRuntime {
        fn prometheus_available(&mut self) -> Result<bool> {
            self.availability_checks += 1;
            Ok(self.available)
        }

        fn install_prometheus(&mut self) -> Result<()> {
            self.install_calls += 1;
            if self.install_fails {
                bail!("install failed in test");
            }
            self.available = true;
            Ok(())
        }

        fn update_prometheus(&mut self) -> Result<()> {
            self.update_calls += 1;
            if self.update_fails {
                bail!("update failed in test");
            }
            Ok(())
        }

        fn internet_available(&mut self) -> bool {
            self.internet_checks += 1;
            self.internet_available
        }

        fn now(&self) -> SystemTime {
            self.now
        }
    }

    fn fixed_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    fn stale_time() -> SystemTime {
        fixed_now() - PROMETHEUS_UPDATE_INTERVAL - Duration::from_secs(1)
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
    fn default_output_path_preserves_extension_and_formats_level() {
        assert_eq!(
            default_output_path(
                Path::new("/tmp/Ro-Translink.rbxl"),
                ObfuscationLevel::Medium
            )
            .unwrap(),
            PathBuf::from("/tmp/Ro-Translink-obfuscated_Medium.rbxl")
        );
        assert_eq!(
            default_output_path(Path::new("Model.rbxm"), ObfuscationLevel::High).unwrap(),
            PathBuf::from("Model-obfuscated_High.rbxm")
        );
    }

    #[test]
    fn rbxl_and_rbxm_extensions_are_supported() {
        assert_eq!(
            validate_input_format(Path::new("place.rbxl")).unwrap(),
            RobloxFileFormat::Rbxl
        );
        assert_eq!(
            validate_input_format(Path::new("model.RBXM")).unwrap(),
            RobloxFileFormat::Rbxm
        );
    }

    #[test]
    fn unknown_input_extension_is_rejected() {
        let error = validate_input_format(Path::new("model.rbxmx")).unwrap_err();
        assert!(format!("{error:#}").contains("expected .rbxl or .rbxm"));
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
        let mut runtime = FakePrometheusRuntime {
            available: false,
            ..Default::default()
        };

        let path = resolve_or_install_prometheus_with(true, None, &mut runtime).unwrap();

        assert_eq!(path, PathBuf::from(PROMETHEUS_COMMAND));
        assert_eq!(runtime.availability_checks, 0);
        assert_eq!(runtime.install_calls, 0);
        assert_eq!(runtime.update_calls, 0);
    }

    #[test]
    fn missing_prometheus_attempts_install() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join(PROMETHEUS_UPDATE_STATE_FILE);
        let mut runtime = FakePrometheusRuntime {
            available: false,
            ..Default::default()
        };

        let resolved =
            resolve_or_install_prometheus_with(false, Some(&state_file), &mut runtime).unwrap();

        assert_eq!(resolved, PathBuf::from(PROMETHEUS_COMMAND));
        assert_eq!(runtime.install_calls, 1);
        assert_eq!(runtime.availability_checks, 2);
        assert_eq!(
            read_prometheus_update_timestamp(&state_file).unwrap(),
            Some(fixed_now())
        );
    }

    #[test]
    fn failed_install_returns_clear_error() {
        let mut runtime = FakePrometheusRuntime {
            available: false,
            install_fails: true,
            ..Default::default()
        };

        let error = resolve_or_install_prometheus_with(false, None, &mut runtime).unwrap_err();
        let error = format!("{error:#}");

        assert!(error.contains("failed to install Prometheus"));
        assert!(error.contains(PROMETHEUS_INSTALL_COMMAND));
    }

    #[test]
    fn fresh_prometheus_install_does_not_update() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join(PROMETHEUS_UPDATE_STATE_FILE);
        write_prometheus_update_timestamp(Some(&state_file), fixed_now()).unwrap();
        let mut runtime = FakePrometheusRuntime::default();

        resolve_or_install_prometheus_with(false, Some(&state_file), &mut runtime).unwrap();

        assert_eq!(runtime.internet_checks, 0);
        assert_eq!(runtime.update_calls, 0);
        assert_eq!(runtime.install_calls, 0);
    }

    #[test]
    fn stale_prometheus_install_skips_update_when_offline() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join(PROMETHEUS_UPDATE_STATE_FILE);
        write_prometheus_update_timestamp(Some(&state_file), stale_time()).unwrap();
        let mut runtime = FakePrometheusRuntime {
            internet_available: false,
            ..Default::default()
        };

        resolve_or_install_prometheus_with(false, Some(&state_file), &mut runtime).unwrap();

        assert_eq!(runtime.internet_checks, 1);
        assert_eq!(runtime.update_calls, 0);
        assert_eq!(runtime.install_calls, 0);
    }

    #[test]
    fn stale_prometheus_install_updates_when_online() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join(PROMETHEUS_UPDATE_STATE_FILE);
        write_prometheus_update_timestamp(Some(&state_file), stale_time()).unwrap();
        let mut runtime = FakePrometheusRuntime::default();

        resolve_or_install_prometheus_with(false, Some(&state_file), &mut runtime).unwrap();

        assert_eq!(runtime.internet_checks, 1);
        assert_eq!(runtime.update_calls, 1);
        assert_eq!(runtime.install_calls, 0);
        assert_eq!(
            read_prometheus_update_timestamp(&state_file).unwrap(),
            Some(fixed_now())
        );
    }

    #[test]
    fn failed_prometheus_update_falls_back_to_installer() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join(PROMETHEUS_UPDATE_STATE_FILE);
        write_prometheus_update_timestamp(Some(&state_file), stale_time()).unwrap();
        let mut runtime = FakePrometheusRuntime {
            update_fails: true,
            ..Default::default()
        };

        resolve_or_install_prometheus_with(false, Some(&state_file), &mut runtime).unwrap();

        assert_eq!(runtime.update_calls, 1);
        assert_eq!(runtime.install_calls, 1);
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
