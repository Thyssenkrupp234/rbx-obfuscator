use std::{
    collections::{HashSet, VecDeque},
    env,
    ffi::OsString,
    fs::{self, File},
    io::ErrorKind,
    io::{BufReader, BufWriter},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, WeakDom,
};
use serde::Serialize;
use tempfile::{Builder as TempFileBuilder, NamedTempFile};

pub mod extract;

pub(crate) const SCRIPT_CLASSES: &[&str] = &["Script", "LocalScript", "ModuleScript"];
const PROMETHEUS_COMMAND: &str = "prometheus-lua";
const PROMETHEUS_INSTALL_URL: &str =
    "https://raw.githubusercontent.com/prometheus-lua/Prometheus/master/install.sh";
const PROMETHEUS_INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/prometheus-lua/Prometheus/master/install.sh | sh";
const PROMETHEUS_UPDATE_STATE_FILE: &str = "prometheus-last-update";
const PROMETHEUS_UPDATE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LUAU_IF_HELPER_NAME: &str = "__rbx_obfuscator_luau_if";
const LUAU_IF_HELPER: &str = "local function __rbx_obfuscator_luau_if(condition, truthy, falsy)\n\tif condition then\n\t\treturn truthy()\n\tend\n\treturn falsy()\nend\n\n";

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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn as_label(self) -> &'static str {
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
    Rbxlx,
    Rbxmx,
}

impl RobloxFileFormat {
    pub fn from_path(path: &Path) -> Result<Self> {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("rbxl") => Ok(Self::Rbxl),
            Some("rbxm") => Ok(Self::Rbxm),
            Some("rbxlx") => Ok(Self::Rbxlx),
            Some("rbxmx") => Ok(Self::Rbxmx),
            _ => bail!(
                "unsupported input file extension for {}: expected .rbxl, .rbxm, .rbxlx, or .rbxmx",
                path.display()
            ),
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            Self::Rbxl => "RBXL",
            Self::Rbxm => "RBXM",
            Self::Rbxlx => "RBXLX",
            Self::Rbxmx => "RBXMX",
        }
    }

    fn is_xml(self) -> bool {
        matches!(self, Self::Rbxlx | Self::Rbxmx)
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
    pub strip_types: bool,
    pub verbose: bool,
    pub backup_dir: Option<PathBuf>,
    pub skip_paths: Vec<String>,
    pub manifest: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressEvent {
    StageStarted {
        stage_index: usize,
        stage_total: usize,
        name: String,
    },
    StageCompleted {
        stage_index: usize,
        stage_total: usize,
        name: String,
    },
    CurrentItem {
        label: String,
        value: String,
    },
    ScriptProgress {
        completed: usize,
        total: usize,
        current_path: Option<String>,
    },
    CompatibilityNote {
        message: String,
    },
    EtaUpdated {
        seconds_remaining: Option<u64>,
    },
    Warning {
        message: String,
    },
    Finished,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObfuscationSummary {
    pub input: PathBuf,
    pub output: PathBuf,
    pub obfuscation_level: ObfuscationLevel,
    pub prometheus_preset: &'static str,
    pub scripts_found: usize,
    pub scripts_processed: usize,
    pub scripts_skipped: usize,
    pub scripts_failed: usize,
    pub dry_run: bool,
    pub backup_created: bool,
    pub duration: Duration,
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
    luau_compatibility_applied: bool,
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

#[derive(Debug)]
struct PrometheusFailure {
    error: anyhow::Error,
    luau_compatibility_applied: bool,
}

pub fn run(options: Options) -> Result<()> {
    let verbose = options.verbose;
    run_with_progress(options, |event| render_console_progress(event, verbose)).map(|_| ())
}

pub fn run_with_progress<F>(options: Options, progress: F) -> Result<ObfuscationSummary>
where
    F: FnMut(ProgressEvent),
{
    run_with_progress_controlled(options, || false, progress)
}

pub fn run_with_progress_controlled<C, F>(
    options: Options,
    mut should_cancel: C,
    mut progress: F,
) -> Result<ObfuscationSummary>
where
    C: FnMut() -> bool,
    F: FnMut(ProgressEvent),
{
    let started_at = Instant::now();
    let input_format = validate_input_format(&options.input)?;
    let output = match &options.output {
        Some(output) => output.clone(),
        None => default_output_path(&options.input, options.obfuscation_level)?,
    };
    let output_format = validate_input_format(&output)?;
    validate_options(&options, &output)?;
    let prometheus_path = resolve_or_install_prometheus(options.dry_run)?;
    let prometheus_preset = prometheus_preset_for_level(options.obfuscation_level);

    progress(ProgressEvent::StageStarted {
        stage_index: 1,
        stage_total: 3,
        name: "Extract scripts from RBXL/RBXM".to_owned(),
    });
    progress(ProgressEvent::CurrentItem {
        label: "Parsing input file".to_owned(),
        value: options.input.display().to_string(),
    });
    if options.verbose {
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
    }
    let mut dom = read_roblox_file(&options.input, input_format)?;

    let skip_paths: HashSet<String> = options.skip_paths.iter().cloned().collect();
    let scripts = collect_scripts(&dom)?;
    if options.verbose {
        eprintln!("Found {} script instance(s)", scripts.len());
    }
    progress(ProgressEvent::ScriptProgress {
        completed: 0,
        total: scripts.len(),
        current_path: None,
    });
    progress(ProgressEvent::StageCompleted {
        stage_index: 1,
        stage_total: 3,
        name: "Extract scripts from RBXL/RBXM".to_owned(),
    });

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

    progress(ProgressEvent::StageStarted {
        stage_index: 2,
        stage_total: 3,
        name: "Obfuscate with Prometheus".to_owned(),
    });
    let mut eta = EtaTracker::default();
    let total_scripts = scripts.len();
    for script in scripts {
        if should_cancel() {
            bail!("operation cancelled");
        }
        progress(ProgressEvent::CurrentItem {
            label: "Current thing".to_owned(),
            value: script.path.clone(),
        });
        if skip_paths.contains(&script.path) {
            skipped += 1;
            if options.verbose {
                eprintln!("Skipping {} ({})", script.path, script.class_name);
            }
            manifest_entries.push(ManifestEntry {
                path: script.path,
                class_name: script.class_name,
                action: ManifestAction::Skipped,
                source_bytes: script.source.len(),
                transformed_bytes: None,
                luau_compatibility_applied: false,
                backup_path: None,
                error: None,
            });
            progress(ProgressEvent::ScriptProgress {
                completed: processed + skipped,
                total: total_scripts,
                current_path: None,
            });
            continue;
        }

        if options.dry_run {
            processed += 1;
            let progress_path = script.path.clone();
            if options.verbose {
                eprintln!(
                    "Would process {} ({}) with Prometheus preset {prometheus_preset}",
                    script.path, script.class_name
                );
            }
            manifest_entries.push(ManifestEntry {
                path: script.path,
                class_name: script.class_name,
                action: ManifestAction::DryRun,
                source_bytes: script.source.len(),
                transformed_bytes: None,
                luau_compatibility_applied: options.strip_types,
                backup_path: None,
                error: None,
            });
            progress(ProgressEvent::ScriptProgress {
                completed: processed + skipped,
                total: total_scripts,
                current_path: Some(progress_path),
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

        if options.verbose {
            eprintln!("Processing {} ({})", script.path, script.class_name);
        }
        let temp_dir = prometheus_temp_dir
            .as_ref()
            .expect("Prometheus temp dir must exist outside dry-run")
            .path();
        let (transformed, luau_compatibility_applied) = match run_prometheus_with_type_fallback(
            &prometheus_path,
            &script.source,
            options.obfuscation_level,
            temp_dir,
            options.strip_types,
            &script.path,
            options.verbose,
        ) {
            Ok(result) => result,
            Err(failure) => {
                let error = format!("{:#}", failure.error);
                let summary = first_error_line(&error).to_owned();
                let progress_path = script.path.clone();
                progress(ProgressEvent::Warning {
                    message: format!(
                        "Failed {}; leaving source unobfuscated: {summary}",
                        script.path
                    ),
                });
                if options.verbose {
                    eprintln!(
                        "Failed {}; leaving source unobfuscated: {}",
                        script.path, summary
                    );
                }
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
                    luau_compatibility_applied: failure.luau_compatibility_applied,
                    backup_path,
                    error: Some(error),
                });
                progress(ProgressEvent::ScriptProgress {
                    completed: processed + skipped + failed_scripts.len(),
                    total: total_scripts,
                    current_path: Some(progress_path),
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
        let progress_path = script.path.clone();
        manifest_entries.push(ManifestEntry {
            path: script.path,
            class_name: script.class_name,
            action: ManifestAction::Processed,
            source_bytes: script.source.len(),
            transformed_bytes: Some(transformed.len()),
            luau_compatibility_applied,
            backup_path,
            error: None,
        });
        if luau_compatibility_applied {
            progress(ProgressEvent::CompatibilityNote {
                message: "Luau compatibility preprocessing applied".to_owned(),
            });
        }
        eta.record_script();
        progress(ProgressEvent::EtaUpdated {
            seconds_remaining: eta.seconds_remaining(processed, total_scripts),
        });
        progress(ProgressEvent::ScriptProgress {
            completed: processed + skipped + failed_scripts.len(),
            total: total_scripts,
            current_path: Some(progress_path),
        });
    }
    progress(ProgressEvent::StageCompleted {
        stage_index: 2,
        stage_total: 3,
        name: "Obfuscate with Prometheus".to_owned(),
    });

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
        if options.verbose {
            eprintln!("Wrote manifest: {}", manifest_path.display());
        }
    }

    progress(ProgressEvent::CurrentItem {
        label: "Summary".to_owned(),
        value: format!(
            "Processed: {processed}, skipped: {skipped}, failed: {}",
            failed_scripts.len()
        ),
    });

    if !failed_scripts.is_empty() {
        progress(ProgressEvent::Warning {
            message: format!("{} script(s) were left unobfuscated", failed_scripts.len()),
        });
        if options.verbose {
            eprintln!("Failed scripts left unobfuscated:");
        }
        for failed in &failed_scripts {
            if options.verbose {
                eprintln!(
                    "  - {} ({}): {}",
                    failed.path, failed.class_name, failed.error
                );
            }
        }
    }

    if options.dry_run {
        progress(ProgressEvent::Finished);
        return Ok(ObfuscationSummary {
            input: options.input,
            output,
            obfuscation_level: options.obfuscation_level,
            prometheus_preset,
            scripts_found: total_scripts,
            scripts_processed: processed,
            scripts_skipped: skipped,
            scripts_failed: failed_scripts.len(),
            dry_run: true,
            backup_created: options.backup_dir.is_some(),
            duration: started_at.elapsed(),
        });
    }

    progress(ProgressEvent::StageStarted {
        stage_index: 3,
        stage_total: 3,
        name: "Compile output file".to_owned(),
    });
    if should_cancel() {
        bail!("operation cancelled");
    }
    progress(ProgressEvent::CurrentItem {
        label: "Writing output file".to_owned(),
        value: output.display().to_string(),
    });
    write_roblox_file(&output, &dom, output_format)?;
    progress(ProgressEvent::StageCompleted {
        stage_index: 3,
        stage_total: 3,
        name: "Compile output file".to_owned(),
    });

    progress(ProgressEvent::Finished);
    Ok(ObfuscationSummary {
        input: options.input,
        output,
        obfuscation_level: options.obfuscation_level,
        prometheus_preset,
        scripts_found: total_scripts,
        scripts_processed: processed,
        scripts_skipped: skipped,
        scripts_failed: failed_scripts.len(),
        dry_run: false,
        backup_created: options.backup_dir.is_some(),
        duration: started_at.elapsed(),
    })
}

pub fn validate_input_format(input: &Path) -> Result<RobloxFileFormat> {
    RobloxFileFormat::from_path(input)
}

fn render_console_progress(event: ProgressEvent, verbose: bool) {
    match event {
        ProgressEvent::StageStarted {
            stage_index,
            stage_total,
            name,
        } => {
            eprintln!("Stage {stage_index}/{stage_total}: {name}");
        }
        ProgressEvent::ScriptProgress {
            completed, total, ..
        } => {
            if verbose && total > 0 {
                eprintln!("Scripts: {completed}/{total}");
            }
        }
        ProgressEvent::Warning { message } => eprintln!("warning: {message}"),
        ProgressEvent::Finished => eprintln!("Done"),
        ProgressEvent::CurrentItem { label, value } if verbose => eprintln!("{label}: {value}"),
        ProgressEvent::CompatibilityNote { message } if verbose => eprintln!("{message}"),
        ProgressEvent::EtaUpdated { .. }
        | ProgressEvent::StageCompleted { .. }
        | ProgressEvent::CurrentItem { .. }
        | ProgressEvent::CompatibilityNote { .. } => {}
    }
}

#[derive(Default)]
struct EtaTracker {
    samples: VecDeque<Duration>,
    last_tick: Option<Instant>,
}

impl EtaTracker {
    fn record_script(&mut self) {
        let now = Instant::now();
        if let Some(last_tick) = self.last_tick {
            self.samples
                .push_back(now.saturating_duration_since(last_tick));
            if self.samples.len() > 10 {
                self.samples.pop_front();
            }
        }
        self.last_tick = Some(now);
    }

    fn seconds_remaining(&self, completed: usize, total: usize) -> Option<u64> {
        if completed >= total || self.samples.len() < 3 {
            return None;
        }
        let remaining = total.saturating_sub(completed);
        if remaining == 0 {
            return Some(0);
        }
        let total_sample_seconds = self.samples.iter().map(Duration::as_secs_f64).sum::<f64>();
        let average = total_sample_seconds / self.samples.len() as f64;
        Some((average * remaining as f64).round() as u64)
    }
}

fn validate_options(options: &Options, output: &Path) -> Result<()> {
    if !options.input.exists() {
        bail!("input file does not exist: {}", options.input.display());
    }
    if !options.input.is_file() {
        bail!("input path is not a file: {}", options.input.display());
    }
    validate_output_path(&options.input, output)?;
    Ok(())
}

pub fn validate_output_path(input: &Path, output: &Path) -> Result<()> {
    if same_path(input, output)? {
        bail!("output must not overwrite input: {}", output.display());
    }
    Ok(())
}

pub fn default_output_path(input: &Path, level: ObfuscationLevel) -> Result<PathBuf> {
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
                command_output_summary(&output, false)
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
                command_output_summary(&output, false)
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
                command_output_summary(&output, false)
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
        .prefix("rbx-obfuscator-")
        .tempdir()
        .context("failed to create temporary Prometheus workspace")
}

fn prometheus_update_state_file() -> Option<PathBuf> {
    if let Some(state_home) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Some(
            PathBuf::from(state_home)
                .join("rbx-obfuscator")
                .join(PROMETHEUS_UPDATE_STATE_FILE),
        );
    }

    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("rbx-obfuscator")
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

fn run_prometheus_with_type_fallback(
    prometheus_path: &Path,
    input_source: &str,
    level: ObfuscationLevel,
    temp_dir: &Path,
    strip_types: bool,
    script_path: &str,
    verbose: bool,
) -> std::result::Result<(String, bool), PrometheusFailure> {
    if strip_types {
        let prepared = prepare_luau_for_prometheus(input_source);
        return run_prometheus(prometheus_path, &prepared, level, temp_dir, verbose)
            .map(|transformed| (transformed, true))
            .map_err(|error| PrometheusFailure {
                error,
                luau_compatibility_applied: true,
            });
    }

    match run_prometheus(prometheus_path, input_source, level, temp_dir, verbose) {
        Ok(transformed) => Ok((transformed, false)),
        Err(original_error) => {
            let prepared = prepare_luau_for_prometheus(input_source);
            if prepared == input_source {
                return Err(PrometheusFailure {
                    error: original_error,
                    luau_compatibility_applied: false,
                });
            }

            if verbose {
                eprintln!(
                    "Prometheus failed for {}; retrying after applying Luau compatibility preprocessing",
                    script_path
                );
            }
            run_prometheus(prometheus_path, &prepared, level, temp_dir, verbose)
                .map(|transformed| (transformed, true))
                .map_err(|fallback_error| PrometheusFailure {
                    error: anyhow!(
                        "Prometheus also failed after Luau compatibility preprocessing; original error: {original_error:#}; fallback error: {fallback_error:#}"
                    ),
                    luau_compatibility_applied: true,
                })
        }
    }
}

fn run_prometheus(
    prometheus_path: &Path,
    input_source: &str,
    level: ObfuscationLevel,
    temp_dir: &Path,
    verbose: bool,
) -> Result<String> {
    let input_file = TempFileBuilder::new()
        .prefix("rbx-obfuscator-input-")
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
        .prefix("rbx-obfuscator-output-")
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
            command_output_summary(&status_output, verbose)
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
        OsString::from("--saveerrors"),
        input_file.as_os_str().to_owned(),
    ]
}

fn command_output_summary(output: &Output, verbose: bool) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if verbose {
        return format!("stdout: {} stderr: {}", stdout.trim(), stderr.trim());
    }

    format!(
        "stdout: {} stderr: {}",
        compact_process_output(&stdout),
        compact_process_output(&stderr)
    )
}

fn compact_process_output(output: &str) -> String {
    const LIMIT: usize = 800;
    let trimmed = output.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_owned();
    }

    let mut compact: String = trimmed.chars().take(LIMIT).collect();
    compact.push_str(" ...");
    compact
}

fn first_error_line(error: &str) -> &str {
    error
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("unknown error")
        .trim()
}

fn prepare_luau_for_prometheus(source: &str) -> String {
    let stripped = strip_luau_type_annotations(source);
    let (without_interpolated_strings, lowered_strings) =
        lower_luau_interpolated_strings(&stripped);
    let (lowered, lowered_if_expressions) =
        lower_luau_if_expressions(&without_interpolated_strings);
    if lowered_if_expressions {
        insert_luau_if_helper(&lowered)
    } else if lowered_strings {
        without_interpolated_strings
    } else {
        lowered
    }
}

fn strip_luau_type_annotations(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = String::with_capacity(source.len());
    let mut normal_start = 0usize;
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'-' && bytes.get(index + 1) == Some(&b'-') {
            output.push_str(&strip_luau_type_annotations_from_code(
                &source[normal_start..index],
            ));
            let end = if let Some(open_len) = long_bracket_open(bytes, index + 2) {
                find_long_bracket_end(bytes, index + 2 + open_len, open_len - 2)
                    .unwrap_or(bytes.len())
            } else {
                find_line_end(bytes, index)
            };
            output.push_str(&source[index..end]);
            index = end;
            normal_start = end;
            continue;
        }

        if matches!(bytes[index], b'\'' | b'"' | b'`') {
            output.push_str(&strip_luau_type_annotations_from_code(
                &source[normal_start..index],
            ));
            let end = find_quoted_string_end(bytes, index);
            output.push_str(&source[index..end]);
            index = end;
            normal_start = end;
            continue;
        }

        if let Some(open_len) = long_bracket_open(bytes, index) {
            output.push_str(&strip_luau_type_annotations_from_code(
                &source[normal_start..index],
            ));
            let end =
                find_long_bracket_end(bytes, index + open_len, open_len - 2).unwrap_or(bytes.len());
            output.push_str(&source[index..end]);
            index = end;
            normal_start = end;
            continue;
        }

        index += 1;
    }

    output.push_str(&strip_luau_type_annotations_from_code(
        &source[normal_start..],
    ));
    output
}

fn strip_luau_type_annotations_from_code(code: &str) -> String {
    let bytes = code.as_bytes();
    let mut output = String::with_capacity(code.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if is_keyword_at(code, index, "export") && is_statement_start(code, index) {
            if let Some((replacement, next_index)) = strip_type_alias(code, index) {
                output.push_str(&replacement);
                index = next_index;
                continue;
            }
        }

        if is_keyword_at(code, index, "type") && is_statement_start(code, index) {
            if let Some((replacement, next_index)) = strip_type_alias(code, index) {
                output.push_str(&replacement);
                index = next_index;
                continue;
            }
        }

        if is_keyword_at(code, index, "local") {
            let after_local = skip_whitespace(code, index + "local".len());
            if !is_keyword_at(code, after_local, "function") {
                let (replacement, next_index) = strip_local_declaration(code, index);
                output.push_str(&replacement);
                index = next_index;
                continue;
            }
        }

        if is_keyword_at(code, index, "for") {
            if let Some((replacement, next_index)) = strip_for_statement_variable_types(code, index)
            {
                output.push_str(&replacement);
                index = next_index;
                continue;
            }
        }

        if is_keyword_at(code, index, "function") {
            if let Some((replacement, next_index)) = strip_function_signature(code, index) {
                output.push_str(&replacement);
                index = next_index;
                continue;
            }
        }

        if bytes[index] == b':' && bytes.get(index + 1) == Some(&b':') {
            index = skip_type_annotation(code, index + 2, b"\n;,)]}+-*/%^=");
            continue;
        }

        push_next_char(code, &mut output, &mut index);
    }

    output
}

fn strip_local_declaration(code: &str, start: usize) -> (String, usize) {
    let bytes = code.as_bytes();
    let line_end = bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| start + offset)
        .unwrap_or(bytes.len());
    let line = &code[start..line_end];
    let assignment = line.as_bytes().iter().position(|byte| *byte == b'=');

    let mut output = String::with_capacity(line.len());
    match assignment {
        Some(assignment) => {
            let lhs = &line[..assignment];
            let trimmed_len = lhs.trim_end_matches([' ', '\t', '\r']).len();
            output.push_str(&strip_type_suffixes(&lhs[..trimmed_len], b","));
            output.push_str(&lhs[trimmed_len..]);
            output.push_str(&strip_luau_type_annotations_from_code(&line[assignment..]));
        }
        None => output.push_str(&strip_type_suffixes(line, b",")),
    }

    if line_end < bytes.len() {
        output.push('\n');
        (output, line_end + 1)
    } else {
        (output, line_end)
    }
}

fn strip_for_statement_variable_types(code: &str, start: usize) -> Option<(String, usize)> {
    let after_for = start + "for".len();
    let in_index = find_top_level_keyword(code, after_for, "in")?;
    let variables = &code[after_for..in_index];
    let trimmed_len = variables.trim_end_matches([' ', '\t', '\r']).len();
    let mut stripped = strip_type_suffixes(&variables[..trimmed_len], b",");
    stripped.push_str(&variables[trimmed_len..]);

    if stripped == variables {
        return None;
    }

    let mut output = String::with_capacity(in_index - start);
    output.push_str(&code[start..after_for]);
    output.push_str(&stripped);
    Some((output, in_index))
}

fn strip_function_signature(code: &str, start: usize) -> Option<(String, usize)> {
    let bytes = code.as_bytes();
    let open_paren = bytes[start..]
        .iter()
        .position(|byte| *byte == b'(')
        .map(|offset| start + offset)?;
    let close_paren = find_matching_paren(bytes, open_paren)?;

    let mut output = String::new();
    output.push_str(&strip_function_generics(&code[start..open_paren]));
    output.push('(');
    output.push_str(&strip_type_suffixes(
        &code[open_paren + 1..close_paren],
        b",",
    ));
    output.push(')');

    let after_params = close_paren + 1;
    let after_whitespace = skip_inline_whitespace(code, after_params);
    if bytes.get(after_whitespace) == Some(&b':') && bytes.get(after_whitespace + 1) != Some(&b':')
    {
        output.push_str(&code[after_params..after_whitespace]);
        let next_index = skip_function_return_annotation(code, after_whitespace + 1);
        Some((output, next_index))
    } else {
        Some((output, after_params))
    }
}

fn strip_type_suffixes(segment: &str, terminators: &[u8]) -> String {
    let bytes = segment.as_bytes();
    let mut output = String::with_capacity(segment.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b':'
            && bytes.get(index + 1) != Some(&b':')
            && previous_non_whitespace_is_identifier(segment, index)
            && next_non_whitespace_starts_type(segment, index + 1)
        {
            trim_inline_whitespace_end(&mut output);
            index = skip_type_annotation(segment, index + 1, terminators);
            continue;
        }

        push_next_char(segment, &mut output, &mut index);
    }

    output
}

fn strip_type_alias(code: &str, start: usize) -> Option<(String, usize)> {
    let bytes = code.as_bytes();
    let type_keyword = if is_keyword_at(code, start, "export") {
        let after_export = skip_whitespace(code, start + "export".len());
        if !is_keyword_at(code, after_export, "type") {
            return None;
        }
        after_export
    } else if is_keyword_at(code, start, "type") {
        start
    } else {
        return None;
    };

    let name_start = skip_inline_whitespace(code, type_keyword + "type".len());
    if name_start == type_keyword + "type".len()
        || !bytes
            .get(name_start)
            .is_some_and(|byte| is_identifier_start_byte(*byte))
    {
        return None;
    }

    let mut cursor = name_start + 1;
    while bytes
        .get(cursor)
        .is_some_and(|byte| is_identifier_byte(*byte))
    {
        cursor += 1;
    }

    cursor = skip_inline_whitespace(code, cursor);
    if bytes.get(cursor) == Some(&b'<') {
        cursor = skip_balanced_angle(code, cursor)?;
        cursor = skip_inline_whitespace(code, cursor);
    }

    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }

    let mut index = start;
    let mut depth = 0usize;
    let mut saw_equals = false;
    let mut replacement = String::new();

    while index < bytes.len() {
        match bytes[index] {
            b'=' => saw_equals = true,
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth = depth.saturating_sub(1),
            b'\n' => {
                replacement.push('\n');
                index += 1;
                if !saw_equals || depth == 0 {
                    break;
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }

    Some((replacement, index))
}

fn strip_function_generics(prefix: &str) -> String {
    let bytes = prefix.as_bytes();
    let generic_end = skip_inline_whitespace_back(bytes, bytes.len());
    if generic_end == 0 || bytes.get(generic_end - 1) != Some(&b'>') {
        return prefix.to_owned();
    }

    let mut depth = 0usize;
    let mut index = generic_end;
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b'>' => depth += 1,
            b'<' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let mut output = String::with_capacity(prefix.len());
                    output.push_str(&prefix[..index]);
                    output.push_str(&prefix[generic_end..]);
                    return output;
                }
            }
            _ => {}
        }
    }

    prefix.to_owned()
}

fn skip_balanced_angle(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    let mut index = start;

    while index < bytes.len() {
        match bytes[index] {
            b'<' => depth += 1,
            b'>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            b'\n' | b';' => return None,
            _ => {}
        }
        index += 1;
    }

    None
}

fn skip_type_annotation(code: &str, start: usize, terminators: &[u8]) -> usize {
    let bytes = code.as_bytes();
    let mut index = start;
    let mut depth = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        if depth == 0
            && terminators.contains(&byte)
            && !(byte == b'-' && bytes.get(index + 1) == Some(&b'>'))
        {
            break;
        }

        match byte {
            b'{' | b'(' | b'[' | b'<' => depth += 1,
            b'}' | b')' | b']' | b'>' => depth = depth.saturating_sub(1),
            _ => {}
        }

        index += 1;
    }

    index
}

fn skip_function_return_annotation(code: &str, start: usize) -> usize {
    let bytes = code.as_bytes();
    let mut index = start;
    let mut depth = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        if depth == 0 {
            if matches!(byte, b'\n' | b';') {
                break;
            }
            if matches!(byte, b' ' | b'\t' | b'\r') {
                let after_whitespace = skip_inline_whitespace(code, index);
                if is_function_body_keyword_at(code, after_whitespace) {
                    break;
                }
            }
        }

        match byte {
            b'{' | b'(' | b'[' | b'<' => depth += 1,
            b'}' | b')' | b']' | b'>' => depth = depth.saturating_sub(1),
            _ => {}
        }

        index += 1;
    }

    index
}

fn long_bracket_open(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'[') {
        return None;
    }

    let mut index = start + 1;
    while bytes.get(index) == Some(&b'=') {
        index += 1;
    }

    if bytes.get(index) == Some(&b'[') {
        Some(index - start + 1)
    } else {
        None
    }
}

fn find_long_bracket_end(bytes: &[u8], start: usize, equals: usize) -> Option<usize> {
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == b']' {
            let mut cursor = index + 1;
            let mut seen_equals = 0usize;
            while seen_equals < equals && bytes.get(cursor) == Some(&b'=') {
                cursor += 1;
                seen_equals += 1;
            }
            if seen_equals == equals && bytes.get(cursor) == Some(&b']') {
                return Some(cursor + 1);
            }
        }
        index += 1;
    }
    None
}

fn find_quoted_string_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if bytes[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

fn find_line_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| start + offset + 1)
        .unwrap_or(bytes.len())
}

fn find_matching_paren(bytes: &[u8], open_paren: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open_paren) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_keyword_at(code: &str, index: usize, keyword: &str) -> bool {
    let bytes = code.as_bytes();
    let keyword_bytes = keyword.as_bytes();
    if bytes.get(index..index + keyword_bytes.len()) != Some(keyword_bytes) {
        return false;
    }

    let before_is_identifier = index
        .checked_sub(1)
        .and_then(|before| bytes.get(before))
        .is_some_and(|byte| is_identifier_byte(*byte));
    let after_is_identifier = bytes
        .get(index + keyword_bytes.len())
        .is_some_and(|byte| is_identifier_byte(*byte));

    !before_is_identifier && !after_is_identifier
}

fn is_statement_start(code: &str, index: usize) -> bool {
    for byte in code.as_bytes()[..index].iter().rev() {
        match byte {
            b' ' | b'\t' | b'\r' => {}
            b'\n' | b';' => return true,
            _ => return false,
        }
    }
    true
}

fn is_function_body_keyword_at(code: &str, index: usize) -> bool {
    [
        "break", "continue", "do", "for", "function", "if", "local", "repeat", "return", "while",
    ]
    .iter()
    .any(|keyword| is_keyword_at(code, index, keyword))
}

fn skip_whitespace(code: &str, mut index: usize) -> usize {
    while code
        .as_bytes()
        .get(index)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        index += 1;
    }
    index
}

fn skip_inline_whitespace(code: &str, mut index: usize) -> usize {
    while matches!(code.as_bytes().get(index), Some(b' ' | b'\t' | b'\r')) {
        index += 1;
    }
    index
}

fn skip_inline_whitespace_back(bytes: &[u8], mut index: usize) -> usize {
    while index > 0 && matches!(bytes.get(index - 1), Some(b' ' | b'\t' | b'\r')) {
        index -= 1;
    }
    index
}

fn trim_inline_whitespace_end(value: &mut String) {
    while value.ends_with([' ', '\t', '\r']) {
        value.pop();
    }
}

fn previous_non_whitespace_is_identifier(segment: &str, index: usize) -> bool {
    segment.as_bytes()[..index]
        .iter()
        .rev()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| is_identifier_byte(*byte))
}

fn next_non_whitespace_starts_type(segment: &str, index: usize) -> bool {
    segment.as_bytes()[index..]
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'{' | b'(' | b'['))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_identifier_start_byte(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn push_next_char(source: &str, output: &mut String, index: &mut usize) {
    let ch = source[*index..]
        .chars()
        .next()
        .expect("index must be at a character boundary");
    output.push(ch);
    *index += ch.len_utf8();
}

fn lower_luau_interpolated_strings(source: &str) -> (String, bool) {
    let bytes = source.as_bytes();
    let mut output = String::with_capacity(source.len());
    let mut index = 0usize;
    let mut changed = false;

    while index < bytes.len() {
        if bytes[index] == b'-' && bytes.get(index + 1) == Some(&b'-') {
            let end = if let Some(open_len) = long_bracket_open(bytes, index + 2) {
                find_long_bracket_end(bytes, index + 2 + open_len, open_len - 2)
                    .unwrap_or(bytes.len())
            } else {
                find_line_end(bytes, index)
            };
            output.push_str(&source[index..end]);
            index = end;
            continue;
        }

        if matches!(bytes[index], b'\'' | b'"') {
            let end = find_quoted_string_end(bytes, index);
            output.push_str(&source[index..end]);
            index = end;
            continue;
        }

        if bytes[index] == b'`' {
            if let Some((replacement, end)) = parse_luau_interpolated_string(source, index) {
                output.push_str(&replacement);
                index = end;
                changed = true;
                continue;
            }
        }

        if let Some(open_len) = long_bracket_open(bytes, index) {
            let end =
                find_long_bracket_end(bytes, index + open_len, open_len - 2).unwrap_or(bytes.len());
            output.push_str(&source[index..end]);
            index = end;
            continue;
        }

        push_next_char(source, &mut output, &mut index);
    }

    (output, changed)
}

fn parse_luau_interpolated_string(source: &str, start: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut index = start + 1;
    let mut literal_start = index;
    let mut parts = Vec::new();
    let mut interpolated = false;

    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index = (index + 2).min(bytes.len());
            }
            b'`' => {
                push_lua_string_part(&source[literal_start..index], &mut parts);
                let replacement = if interpolated {
                    format!("({})", parts.join(" .. "))
                } else {
                    lua_quote(&source[literal_start..index])
                };
                return Some((replacement, index + 1));
            }
            b'{' => {
                push_lua_string_part(&source[literal_start..index], &mut parts);
                let expression_end = find_interpolation_expression_end(source, index + 1)?;
                let expression = source[index + 1..expression_end].trim();
                parts.push(format!("tostring({expression})"));
                interpolated = true;
                index = expression_end + 1;
                literal_start = index;
            }
            _ => index += 1,
        }
    }

    None
}

fn push_lua_string_part(part: &str, parts: &mut Vec<String>) {
    if !part.is_empty() {
        parts.push(lua_quote(part));
    }
}

fn lua_quote(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn find_interpolation_expression_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut depth = 0usize;

    while index < bytes.len() {
        if let Some(end) = skip_luau_literal_or_comment(source, index) {
            index = end;
            continue;
        }

        match bytes[index] {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' if depth == 0 => return Some(index),
            b'}' | b')' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }

        index += 1;
    }

    None
}

#[derive(Debug)]
struct LuauIfExpression<'a> {
    condition: &'a str,
    true_expression: &'a str,
    false_expression: &'a str,
    end: usize,
}

fn lower_luau_if_expressions(source: &str) -> (String, bool) {
    let bytes = source.as_bytes();
    let mut output = String::with_capacity(source.len());
    let mut index = 0usize;
    let mut changed = false;

    while index < bytes.len() {
        if let Some(end) = skip_luau_literal_or_comment(source, index) {
            output.push_str(&source[index..end]);
            index = end;
            continue;
        }

        if is_keyword_at(source, index, "if") && is_expression_if_start(source, index) {
            if let Some(if_expression) = parse_luau_if_expression(source, index) {
                output.push_str(&format_luau_if_expression(&if_expression));
                index = if_expression.end;
                changed = true;
                continue;
            }
        }

        push_next_char(source, &mut output, &mut index);
    }

    (output, changed)
}

fn format_luau_if_expression(if_expression: &LuauIfExpression<'_>) -> String {
    let (condition, _) = lower_luau_if_expressions(if_expression.condition.trim());
    let (true_expression, _) = lower_luau_if_expressions(if_expression.true_expression.trim());
    let (false_expression, _) = lower_luau_if_expressions(if_expression.false_expression.trim());

    format!(
        "{LUAU_IF_HELPER_NAME}(({condition}), function() return {true_expression} end, function() return {false_expression} end)"
    )
}

fn parse_luau_if_expression(source: &str, start: usize) -> Option<LuauIfExpression<'_>> {
    let condition_start = skip_whitespace(source, start + "if".len());
    let then_index = find_top_level_keyword(source, condition_start, "then")?;
    let true_start = skip_whitespace(source, then_index + "then".len());
    let else_index = find_top_level_keyword(source, true_start, "else")?;
    let false_start = skip_whitespace(source, else_index + "else".len());
    let end = find_luau_if_expression_end(source, false_start);

    if condition_start == then_index || true_start == else_index || false_start == end {
        return None;
    }

    Some(LuauIfExpression {
        condition: &source[condition_start..then_index],
        true_expression: &source[true_start..else_index],
        false_expression: &source[false_start..end],
        end,
    })
}

fn find_top_level_keyword(source: &str, start: usize, keyword: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut depth = 0usize;

    while index < bytes.len() {
        if let Some(end) = skip_luau_literal_or_comment(source, index) {
            index = end;
            continue;
        }

        if depth == 0 && is_keyword_at(source, index, keyword) {
            return Some(index);
        }

        match bytes[index] {
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth = depth.saturating_sub(1),
            b'\n' | b';' if depth == 0 => return None,
            _ => {}
        }

        index += 1;
    }

    None
}

fn find_luau_if_expression_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;
    let mut depth = 0usize;

    while index < bytes.len() {
        if let Some(end) = skip_luau_literal_or_comment(source, index) {
            index = end;
            continue;
        }

        if depth == 0 && matches!(bytes[index], b'\n' | b';' | b',' | b')' | b']' | b'}') {
            break;
        }

        match bytes[index] {
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }

        index += 1;
    }

    index
}

fn skip_luau_literal_or_comment(source: &str, index: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(index) == Some(&b'-') && bytes.get(index + 1) == Some(&b'-') {
        if let Some(open_len) = long_bracket_open(bytes, index + 2) {
            return Some(
                find_long_bracket_end(bytes, index + 2 + open_len, open_len - 2)
                    .unwrap_or(bytes.len()),
            );
        }
        return Some(find_line_end(bytes, index));
    }

    if matches!(bytes.get(index), Some(b'\'' | b'"' | b'`')) {
        return Some(find_quoted_string_end(bytes, index));
    }

    long_bracket_open(bytes, index).map(|open_len| {
        find_long_bracket_end(bytes, index + open_len, open_len - 2).unwrap_or(bytes.len())
    })
}

fn is_expression_if_start(source: &str, index: usize) -> bool {
    let bytes = source.as_bytes();
    let Some(previous_index) = bytes[..index]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
    else {
        return false;
    };

    match bytes[previous_index] {
        b'=' | b'(' | b'[' | b'{' | b',' | b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'<'
        | b'>' => return true,
        b'\n' | b';' => return false,
        _ => {}
    }

    keyword_ends_at(source, previous_index + 1, "return")
        || keyword_ends_at(source, previous_index + 1, "and")
        || keyword_ends_at(source, previous_index + 1, "or")
        || keyword_ends_at(source, previous_index + 1, "not")
}

fn keyword_ends_at(source: &str, end: usize, keyword: &str) -> bool {
    end >= keyword.len() && is_keyword_at(source, end - keyword.len(), keyword)
}

fn insert_luau_if_helper(source: &str) -> String {
    let insert_at = luau_helper_insert_index(source);
    let mut output = String::with_capacity(source.len() + LUAU_IF_HELPER.len());
    output.push_str(&source[..insert_at]);
    output.push_str(LUAU_IF_HELPER);
    output.push_str(&source[insert_at..]);
    output
}

fn luau_helper_insert_index(source: &str) -> usize {
    let mut index = 0usize;

    while source[index..].starts_with("--!") {
        let line_end = source[index..]
            .find('\n')
            .map(|offset| index + offset + 1)
            .unwrap_or(source.len());
        index = line_end;
        if index >= source.len() {
            break;
        }
    }

    index
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

pub(crate) fn read_roblox_file(path: &Path, format: RobloxFileFormat) -> Result<WeakDom> {
    let input = BufReader::new(
        File::open(path)
            .with_context(|| format!("failed to open input file {}", path.display()))?,
    );

    if format.is_xml() {
        rbx_xml::from_reader_default(input)
            .with_context(|| format!("failed to read Roblox XML file {}", path.display()))
    } else {
        rbx_binary::from_reader(input)
            .with_context(|| format!("failed to read Roblox binary file {}", path.display()))
    }
}

pub(crate) fn write_roblox_file(
    path: &Path,
    dom: &WeakDom,
    format: RobloxFileFormat,
) -> Result<()> {
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
        if format.is_xml() {
            rbx_xml::to_writer_default(output, dom, &top_level_refs).with_context(|| {
                format!(
                    "failed to write temporary Roblox XML for {}",
                    path.display()
                )
            })?;
        } else {
            rbx_binary::to_writer(output, dom, &top_level_refs).with_context(|| {
                format!(
                    "failed to write temporary Roblox binary for {}",
                    path.display()
                )
            })?;
        }
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

pub(crate) fn same_path(input: &Path, output: &Path) -> Result<bool> {
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

pub(crate) fn child_path(parent_path: &str, child_name: &str) -> String {
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

pub(crate) fn sanitize_filename(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    let output = output.trim_matches('.').to_owned();
    if output.is_empty() {
        "script".to_owned()
    } else if is_windows_reserved_filename(&output) {
        format!("{output}_")
    } else {
        output
    }
}

fn is_windows_reserved_filename(value: &str) -> bool {
    let name = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    matches!(
        name.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
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
    fn roblox_file_extensions_are_supported() {
        assert_eq!(
            validate_input_format(Path::new("place.rbxl")).unwrap(),
            RobloxFileFormat::Rbxl
        );
        assert_eq!(
            validate_input_format(Path::new("model.RBXM")).unwrap(),
            RobloxFileFormat::Rbxm
        );
        assert_eq!(
            validate_input_format(Path::new("place.rbxlx")).unwrap(),
            RobloxFileFormat::Rbxlx
        );
        assert_eq!(
            validate_input_format(Path::new("model.RBXMX")).unwrap(),
            RobloxFileFormat::Rbxmx
        );
    }

    #[test]
    fn unknown_input_extension_is_rejected() {
        let error = validate_input_format(Path::new("model.txt")).unwrap_err();
        assert!(format!("{error:#}").contains(".rbxl, .rbxm, .rbxlx, or .rbxmx"));
    }

    #[test]
    fn obfuscation_progress_sequence_includes_core_stages() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.rbxl");
        let output = dir.path().join("output.rbxl");
        let dom = WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("DataModel").with_child(
                rbx_dom_weak::InstanceBuilder::new("Script")
                    .with_name("Main")
                    .with_property("Source", "print('hi')"),
            ),
        );
        write_roblox_file(&input, &dom, RobloxFileFormat::Rbxl).unwrap();
        let mut events = Vec::new();

        let summary = run_with_progress(
            Options {
                input,
                output: Some(output),
                obfuscation_level: ObfuscationLevel::Minimal,
                dry_run: true,
                strip_types: false,
                verbose: false,
                backup_dir: None,
                skip_paths: Vec::new(),
                manifest: None,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(summary.scripts_found, 1);
        assert!(events
            .iter()
            .any(|event| matches!(event, ProgressEvent::StageStarted { stage_index: 1, .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, ProgressEvent::StageStarted { stage_index: 2, .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, ProgressEvent::Finished)));
    }

    #[test]
    fn eta_calculation_handles_zero_scripts() {
        let eta = EtaTracker::default();

        assert_eq!(eta.seconds_remaining(0, 0), None);
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
                OsString::from("--saveerrors"),
                OsString::from("input.luau"),
            ]
        );
    }

    #[test]
    fn strip_luau_type_annotations_from_local_declarations() {
        let source = "\
local Bus:ObjectValue = script.Bus
local speed: number, route: string = 4, 10
";
        let expected = "\
local Bus = script.Bus
local speed, route = 4, 10
";

        assert_eq!(strip_luau_type_annotations(source), expected);
    }

    #[test]
    fn strip_luau_type_annotations_from_function_signatures() {
        let source = "\
local function getBus(bus: ObjectValue, route: string): boolean
    return bus.Name == route
end
function Controller:Start(player: Player): () return self:Run(player) end
local f: (number) -> string = function(value: number): string return tostring(value) end
function id<T>(value: T): T return value end
for _, Car : Model in pairs(Cars:GetChildren()) do print(Car) end
";
        let expected = "\
local function getBus(bus, route)
    return bus.Name == route
end
function Controller:Start(player) return self:Run(player) end
local f = function(value) return tostring(value) end
function id(value) return value end
for _, Car in pairs(Cars:GetChildren()) do print(Car) end
";

        assert_eq!(strip_luau_type_annotations(source), expected);
    }

    #[test]
    fn strip_luau_type_aliases_and_casts_without_removing_type_calls() {
        let source = "\
type Bus = { name: string }
export type Route<T> = { value: T }
type(value)
local bus = value :: Bus
local callback = handler :: (number) -> string
local runtime = type(value)
";
        let expected = concat!(
            "\n",
            "\n",
            "type(value)\n",
            "local bus = value \n",
            "local callback = handler \n",
            "local runtime = type(value)\n",
        );

        assert_eq!(strip_luau_type_annotations(source), expected);
    }

    #[test]
    fn strip_luau_type_annotations_preserves_comments_strings_and_method_calls() {
        let source = "\
-- local Bus:ObjectValue = script.Bus
local text = \"value: ObjectValue\"
local template = `value: ObjectValue`
local long = [[type Foo = string]]
object:Method(script.Bus)
";

        assert_eq!(strip_luau_type_annotations(source), source);
    }

    #[test]
    fn prepare_luau_for_prometheus_handles_ro_translink_type_patterns() {
        let source = "\
--!strict
function SheetValues.new(SpreadId: string, SheetId: string?)
    local GUID = SHA1(SpreadId .. \"||\" .. SheetId :: string)
    function SheetManager:GetValue(Name: string, Default: any?)
        return if value ~= nil then value else Default
    end
end
";

        let prepared = prepare_luau_for_prometheus(source);

        assert!(prepared.starts_with("--!strict\nlocal function __rbx_obfuscator_luau_if"));
        assert!(prepared.contains("function SheetValues.new(SpreadId, SheetId)"));
        assert!(prepared.contains("local GUID = SHA1(SpreadId .. \"||\" .. SheetId )"));
        assert!(prepared.contains("function SheetManager:GetValue(Name, Default)"));
        assert!(prepared.contains(
            "return __rbx_obfuscator_luau_if((value ~= nil), function() return value end, function() return Default end)"
        ));
        assert!(!prepared.contains("string?"));
        assert!(!prepared.contains("any?"));
        assert!(!prepared.contains(":: string"));
    }

    #[test]
    fn prepare_luau_for_prometheus_lowers_if_expressions() {
        let source = "\
local animator = if Humanoid then Humanoid:FindFirstChildOfClass(\"Animator\") else nil
local Value = ConvertTyped(if Comp.v ~= nil then Comp.v else \"\")
local heightScale = if userAnimateScaleRun then getHeightScale() else 1
";

        let prepared = prepare_luau_for_prometheus(source);

        assert!(prepared.contains(LUAU_IF_HELPER));
        assert!(prepared.contains(
            "local animator = __rbx_obfuscator_luau_if((Humanoid), function() return Humanoid:FindFirstChildOfClass(\"Animator\") end, function() return nil end)"
        ));
        assert!(prepared.contains(
            "local Value = ConvertTyped(__rbx_obfuscator_luau_if((Comp.v ~= nil), function() return Comp.v end, function() return \"\" end))"
        ));
        assert!(prepared.contains(
            "local heightScale = __rbx_obfuscator_luau_if((userAnimateScaleRun), function() return getHeightScale() end, function() return 1 end)"
        ));
    }

    #[test]
    fn prepare_luau_for_prometheus_lowers_interpolated_strings() {
        let source = "\
error(`No MainModule found in {model.Parent:GetFullName()}`)
local plain = `hello`
";

        let prepared = prepare_luau_for_prometheus(source);

        assert!(prepared.contains(
            "error((\"No MainModule found in \" .. tostring(model.Parent:GetFullName())))"
        ));
        assert!(prepared.contains("local plain = \"hello\""));
        assert!(!prepared.contains('`'));
    }

    #[test]
    fn prepare_luau_for_prometheus_preserves_statement_ifs() {
        let source = "if success then return result else return nil end\n";

        assert_eq!(prepare_luau_for_prometheus(source), source);
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
