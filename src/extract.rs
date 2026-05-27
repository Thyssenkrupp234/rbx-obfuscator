use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::{
    types::{Ref, Variant},
    ustr, WeakDom,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    child_path, read_roblox_file, same_path, sanitize_filename, validate_input_format,
    variant_to_json_value, CompileMetadata, CompileScriptEntry, FileFingerprint, ProgressEvent,
    RobloxFileFormat, COMPILE_METADATA_FILE, COMPILE_STATE_DIR, SCRIPT_CLASSES,
};

#[derive(Debug)]
pub struct ExtractOptions {
    pub input: PathBuf,
    pub output_folder: PathBuf,
    pub verbose: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractSummary {
    pub input: PathBuf,
    pub output_folder: PathBuf,
    pub total_instances: usize,
    pub scripts_found: usize,
    pub scripts_exported: usize,
    pub guis_exported: usize,
    pub content_refs_found: usize,
    pub warnings_count: usize,
    pub duration: Duration,
}

#[derive(Debug, Serialize)]
struct ExtractionCounts {
    total_instances: usize,
    scripts_found: usize,
    scripts_exported: usize,
    guis_exported: usize,
    content_references_found: usize,
    warnings_count: usize,
}

#[derive(Debug, Serialize)]
struct ScriptManifestEntry {
    id: String,
    roblox_path: String,
    class_name: String,
    output_file: Option<PathBuf>,
    source_length: usize,
    source_present: bool,
    warning: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ContentReference {
    roblox_path: String,
    class_name: String,
    property_name: String,
    value: String,
}

#[derive(Clone, Debug)]
struct ScriptExport {
    id: String,
    roblox_path: String,
    class_name: String,
    name: String,
    parent_segments: Vec<String>,
    source: Option<String>,
    warning: Option<String>,
}

struct ExtractionCollector {
    scripts: Vec<ScriptExport>,
    gui_roots: Vec<GuiRoot>,
    gui_instance_ids: HashMap<Ref, String>,
    content_refs: Option<JsonArrayWriter>,
    warnings: Option<JsonArrayWriter>,
    unsupported_properties: Option<JsonArrayWriter>,
    content_refs_count: usize,
    warnings_count: usize,
    unsupported_properties_count: usize,
    total_instances: usize,
    next_instance_id: usize,
}

#[derive(Clone, Copy, Debug)]
struct GuiRoot {
    referent: Ref,
}

impl ExtractionCollector {
    fn new(
        content_refs: Option<JsonArrayWriter>,
        warnings: Option<JsonArrayWriter>,
        unsupported_properties: Option<JsonArrayWriter>,
    ) -> Self {
        Self {
            scripts: Vec::new(),
            gui_roots: Vec::new(),
            gui_instance_ids: HashMap::new(),
            content_refs,
            warnings,
            unsupported_properties,
            content_refs_count: 0,
            warnings_count: 0,
            unsupported_properties_count: 0,
            total_instances: 0,
            next_instance_id: 0,
        }
    }

    fn next_id(&mut self) -> String {
        self.next_instance_id += 1;
        format!("inst_{:06}", self.next_instance_id)
    }

    fn id_for(&self, referent: Ref) -> Result<String> {
        self.gui_instance_ids
            .get(&referent)
            .cloned()
            .ok_or_else(|| anyhow!("missing extraction id for instance referent {referent}"))
    }

    fn record_gui_id(&mut self, referent: Ref, id: &str) {
        self.gui_instance_ids.insert(referent, id.to_owned());
    }

    fn record_warning(&mut self, warning: String) -> Result<()> {
        self.warnings_count += 1;
        if let Some(writer) = &mut self.warnings {
            writer.push(&warning)?;
        }
        Ok(())
    }

    fn record_unsupported_property(&mut self, note: String) -> Result<()> {
        self.unsupported_properties_count += 1;
        if let Some(writer) = &mut self.unsupported_properties {
            writer.push(&note)?;
        }
        self.record_warning(note)
    }

    fn record_content_ref(&mut self, reference: ContentReference) -> Result<()> {
        self.content_refs_count += 1;
        if let Some(writer) = &mut self.content_refs {
            writer.push(&reference)?;
        }
        Ok(())
    }

    fn finish_array_writers(&mut self) -> Result<()> {
        if let Some(writer) = &mut self.content_refs {
            writer.finish()?;
        }
        if let Some(writer) = &mut self.warnings {
            writer.finish()?;
        }
        if let Some(writer) = &mut self.unsupported_properties {
            writer.finish()?;
        }
        Ok(())
    }
}

impl Default for ExtractionCollector {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

struct JsonArrayWriter {
    writer: BufWriter<File>,
    first: bool,
    finished: bool,
}

impl JsonArrayWriter {
    fn create(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create JSON output directory {}",
                parent.display()
            )
        })?;
        let mut writer = BufWriter::new(
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?,
        );
        writer
            .write_all(b"[\n")
            .with_context(|| format!("failed to initialize {}", path.display()))?;
        Ok(Self {
            writer,
            first: true,
            finished: false,
        })
    }

    fn push(&mut self, value: &impl Serialize) -> Result<()> {
        if !self.first {
            self.writer.write_all(b",\n")?;
        }
        self.first = false;
        self.writer.write_all(b"  ")?;
        serde_json::to_writer(&mut self.writer, value).context("failed to serialize JSON item")?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.writer.write_all(b"\n]\n")?;
        self.writer.flush()?;
        self.finished = true;
        Ok(())
    }
}

pub fn run(options: ExtractOptions) -> Result<ExtractSummary> {
    let verbose = options.verbose;
    run_with_progress(options, |event| render_extract_progress(event, verbose))
}

pub fn run_with_progress<F>(options: ExtractOptions, progress: F) -> Result<ExtractSummary>
where
    F: FnMut(ProgressEvent),
{
    run_with_progress_controlled(options, || false, progress)
}

pub fn default_output_folder(input: &Path) -> Result<PathBuf> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("input path has no file name: {}", input.display()))?;
    Ok(input.with_file_name(stem))
}

pub fn run_with_progress_controlled<C, F>(
    options: ExtractOptions,
    mut should_cancel: C,
    mut progress: F,
) -> Result<ExtractSummary>
where
    C: FnMut() -> bool,
    F: FnMut(ProgressEvent),
{
    let started_at = Instant::now();
    let input_format = validate_input_format(&options.input)?;
    validate_extract_options(&options)?;
    if !input_format.is_xml()
        && crate::binary_fast::binary_instance_count(&options.input)? >= 200_000
    {
        return crate::binary_fast::extract_binary_large(
            options,
            input_format,
            should_cancel,
            progress,
        );
    }

    progress(ProgressEvent::StageStarted {
        stage_index: 1,
        stage_total: 3,
        name: "Parse RBXL/RBXM".to_owned(),
    });
    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Parsing input file".to_owned(),
    });
    let dom = read_roblox_file(&options.input, input_format)?;
    if should_cancel() {
        bail!("operation cancelled");
    }
    progress(ProgressEvent::StageCompleted {
        stage_index: 1,
        stage_total: 3,
        name: "Parse RBXL/RBXM".to_owned(),
    });

    progress(ProgressEvent::StageStarted {
        stage_index: 2,
        stage_total: 3,
        name: "Export components".to_owned(),
    });
    fs::create_dir_all(&options.output_folder).with_context(|| {
        format!(
            "failed to create extraction output folder {}",
            options.output_folder.display()
        )
    })?;

    let state_dir = options.output_folder.join(COMPILE_STATE_DIR);
    fs::create_dir_all(&state_dir).with_context(|| {
        format!(
            "failed to create compile metadata folder {}",
            state_dir.display()
        )
    })?;
    let warnings_path = state_dir.join("warnings.tmp.json");
    let unsupported_path = state_dir.join("unsupported_properties.tmp.json");
    let content_refs_path = options.output_folder.join("content_refs.json");
    let mut collector = ExtractionCollector::new(
        Some(JsonArrayWriter::create(&content_refs_path)?),
        Some(JsonArrayWriter::create(&warnings_path)?),
        Some(JsonArrayWriter::create(&unsupported_path)?),
    );
    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing instances.json".to_owned(),
    });
    write_instances_json(
        &options.output_folder.join("instances.json"),
        &dom,
        &mut collector,
        &mut should_cancel,
    )?;
    if should_cancel() {
        bail!("operation cancelled");
    }
    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Exporting scripts".to_owned(),
    });
    let script_manifest = write_scripts(&options.output_folder, &collector.scripts)?;
    if should_cancel() {
        bail!("operation cancelled");
    }
    let scripts_exported = script_manifest
        .iter()
        .filter(|entry| entry.output_file.is_some())
        .count();
    progress(ProgressEvent::ScriptProgress {
        completed: scripts_exported,
        total: collector.scripts.len(),
        current_path: None,
    });

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Exporting GUI JSON".to_owned(),
    });
    let gui_roots = collector.gui_roots.clone();
    let guis_exported = write_guis(&options.output_folder, &dom, &gui_roots, &collector)?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Finalizing content_refs.json".to_owned(),
    });
    collector.finish_array_writers()?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing compile metadata".to_owned(),
    });
    write_compile_state(
        &options.input,
        input_format,
        &options.output_folder,
        &options.output_folder.join("instances.json"),
        collector.total_instances,
        &script_manifest,
    )?;
    if should_cancel() {
        bail!("operation cancelled");
    }
    progress(ProgressEvent::StageCompleted {
        stage_index: 2,
        stage_total: 3,
        name: "Export components".to_owned(),
    });

    progress(ProgressEvent::StageStarted {
        stage_index: 3,
        stage_total: 3,
        name: "Write manifest".to_owned(),
    });
    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing manifest.json".to_owned(),
    });
    let warnings_count = collector.warnings_count;
    let counts = ExtractionCounts {
        total_instances: collector.total_instances,
        scripts_found: collector.scripts.len(),
        scripts_exported,
        guis_exported,
        content_references_found: collector.content_refs_count,
        warnings_count,
    };
    write_manifest(
        &options.output_folder.join("manifest.json"),
        &options.input,
        &options.output_folder,
        input_format,
        &counts,
        &script_manifest,
        &warnings_path,
        &unsupported_path,
    )?;
    let _ = fs::remove_file(&warnings_path);
    let _ = fs::remove_file(&unsupported_path);
    progress(ProgressEvent::StageCompleted {
        stage_index: 3,
        stage_total: 3,
        name: "Write manifest".to_owned(),
    });
    progress(ProgressEvent::Finished);

    Ok(ExtractSummary {
        input: options.input,
        output_folder: options.output_folder,
        total_instances: collector.total_instances,
        scripts_found: collector.scripts.len(),
        scripts_exported,
        guis_exported,
        content_refs_found: collector.content_refs_count,
        warnings_count,
        duration: started_at.elapsed(),
    })
}

pub fn validate_extract_output(input: &Path, output_folder: &Path) -> Result<()> {
    if same_path(input, output_folder)? {
        bail!(
            "extraction output folder must not overwrite input: {}",
            output_folder.display()
        );
    }
    if output_folder.exists() && output_folder.is_file() {
        bail!(
            "extraction output path is a file, expected a folder: {}",
            output_folder.display()
        );
    }
    Ok(())
}

fn validate_extract_options(options: &ExtractOptions) -> Result<()> {
    if !options.input.exists() {
        bail!("input file does not exist: {}", options.input.display());
    }
    if !options.input.is_file() {
        bail!("input path is not a file: {}", options.input.display());
    }
    validate_extract_output(&options.input, &options.output_folder)
}

#[cfg(test)]
fn collect_components(dom: &WeakDom) -> Result<ExtractionCollector> {
    let mut collector = ExtractionCollector::default();
    let root = dom
        .get_by_ref(dom.root_ref())
        .ok_or_else(|| anyhow!("DOM root is missing"))?;

    for child_ref in root.children().iter().copied() {
        collect_instance(dom, child_ref, "game", &[], false, &mut collector)?;
    }

    Ok(collector)
}

#[cfg(test)]
fn collect_instance(
    dom: &WeakDom,
    referent: Ref,
    parent_path: &str,
    parent_segments: &[String],
    inside_gui: bool,
    collector: &mut ExtractionCollector,
) -> Result<()> {
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("DOM contains missing child referent"))?;
    let name = instance.name.clone();
    let class_name = instance.class.to_string();
    let id = collector.next_id();
    let path = child_path(parent_path, &name);
    let mut segments = parent_segments.to_vec();
    segments.push(name.clone());
    let child_refs = instance.children().to_vec();
    let is_gui = is_gui_root(&class_name);
    let inside_gui = inside_gui || is_gui;

    collector.total_instances += 1;
    if inside_gui {
        collector.record_gui_id(referent, &id);
    }

    if is_script_class(&class_name) {
        let (source, warning) = script_source(instance, &path);
        if let Some(warning) = &warning {
            collector.record_warning(warning.clone())?;
        }
        collector.scripts.push(ScriptExport {
            id: id.clone(),
            roblox_path: path.clone(),
            class_name: class_name.clone(),
            name: name.clone(),
            parent_segments: parent_segments.to_vec(),
            source,
            warning,
        });
    }

    if is_gui {
        collector.gui_roots.push(GuiRoot { referent });
    }

    collect_content_refs(instance, &path, &class_name, collector)?;
    let _ = serializable_properties(instance, &path, collector)?;

    for child_ref in child_refs {
        collect_instance(dom, child_ref, &path, &segments, inside_gui, collector)?;
    }

    Ok(())
}

fn write_instances_json<C>(
    path: &Path,
    dom: &WeakDom,
    collector: &mut ExtractionCollector,
    should_cancel: &mut C,
) -> Result<()>
where
    C: FnMut() -> bool,
{
    let mut writer = BufWriter::new(
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?,
    );
    writer.write_all(b"[\n")?;
    let root = dom
        .get_by_ref(dom.root_ref())
        .ok_or_else(|| anyhow!("DOM root is missing"))?;
    let mut first = true;
    for child_ref in root.children().iter().copied() {
        if !first {
            writer.write_all(b",\n")?;
        }
        first = false;
        write_instance_node(
            &mut writer,
            dom,
            child_ref,
            "game",
            &[],
            false,
            1,
            collector,
            should_cancel,
        )?;
    }
    writer.write_all(b"\n]\n")?;
    writer
        .flush()
        .with_context(|| format!("failed to write {}", path.display()))
}

#[allow(clippy::too_many_arguments)]
fn write_instance_node<C>(
    writer: &mut dyn Write,
    dom: &WeakDom,
    referent: Ref,
    parent_path: &str,
    parent_segments: &[String],
    inside_gui: bool,
    indent: usize,
    collector: &mut ExtractionCollector,
    should_cancel: &mut C,
) -> Result<()>
where
    C: FnMut() -> bool,
{
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("DOM contains missing child referent"))?;
    let name = instance.name.clone();
    let class_name = instance.class.to_string();
    let id = collector.next_id();
    let path = child_path(parent_path, &name);
    let mut segments = parent_segments.to_vec();
    segments.push(name.clone());
    let child_refs = instance.children().to_vec();
    let is_gui = is_gui_root(&class_name);
    let inside_gui = inside_gui || is_gui;

    collector.total_instances += 1;
    if collector.total_instances.is_multiple_of(1024) && should_cancel() {
        bail!("operation cancelled");
    }
    if inside_gui {
        collector.record_gui_id(referent, &id);
    }

    if is_script_class(&class_name) {
        let (source, warning) = script_source(instance, &path);
        if let Some(warning) = &warning {
            collector.record_warning(warning.clone())?;
        }
        collector.scripts.push(ScriptExport {
            id: id.clone(),
            roblox_path: path.clone(),
            class_name: class_name.clone(),
            name: name.clone(),
            parent_segments: parent_segments.to_vec(),
            source,
            warning,
        });
    }

    if is_gui {
        collector.gui_roots.push(GuiRoot { referent });
    }

    collect_content_refs(instance, &path, &class_name, collector)?;
    let properties = serializable_properties(instance, &path, collector)?;

    write_indent(writer, indent)?;
    writer.write_all(b"{\"id\":")?;
    serde_json::to_writer(&mut *writer, &id)?;
    writer.write_all(b",\"name\":")?;
    serde_json::to_writer(&mut *writer, &name)?;
    writer.write_all(b",\"class_name\":")?;
    serde_json::to_writer(&mut *writer, &class_name)?;
    writer.write_all(b",\"roblox_path\":")?;
    serde_json::to_writer(&mut *writer, &path)?;
    writer.write_all(b",\"properties\":")?;
    serde_json::to_writer(&mut *writer, &properties)?;
    writer.write_all(b",\"children\":[")?;
    if !child_refs.is_empty() {
        writer.write_all(b"\n")?;
    }
    let mut first = true;
    for child_ref in child_refs {
        if !first {
            writer.write_all(b",\n")?;
        }
        first = false;
        write_instance_node(
            writer,
            dom,
            child_ref,
            &path,
            &segments,
            inside_gui,
            indent + 1,
            collector,
            should_cancel,
        )?;
    }
    if !first {
        writer.write_all(b"\n")?;
        write_indent(writer, indent)?;
    }
    writer.write_all(b"]}")?;
    Ok(())
}

fn write_indent(writer: &mut dyn Write, indent: usize) -> io::Result<()> {
    for _ in 0..indent {
        writer.write_all(b"  ")?;
    }
    Ok(())
}

fn write_scripts(
    output_folder: &Path,
    scripts: &[ScriptExport],
) -> Result<Vec<ScriptManifestEntry>> {
    let script_root = output_folder.join("scripts");
    let mut used_paths = HashSet::new();
    let mut manifest = Vec::with_capacity(scripts.len());

    for script in scripts {
        let Some(source) = &script.source else {
            manifest.push(ScriptManifestEntry {
                id: script.id.clone(),
                roblox_path: script.roblox_path.clone(),
                class_name: script.class_name.clone(),
                output_file: None,
                source_length: 0,
                source_present: false,
                warning: script.warning.clone(),
            });
            continue;
        };

        let mut directory = script_root.clone();
        for segment in &script.parent_segments {
            directory.push(sanitize_filename(segment));
        }
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "failed to create script extraction directory {}",
                directory.display()
            )
        })?;

        let extension = script_extension(&script.class_name);
        let file_name = format!("{}{}", sanitize_filename(&script.name), extension);
        let path = unique_path(&directory.join(file_name), &mut used_paths);
        fs::write(&path, source)
            .with_context(|| format!("failed to write script {}", path.display()))?;

        manifest.push(ScriptManifestEntry {
            id: script.id.clone(),
            roblox_path: script.roblox_path.clone(),
            class_name: script.class_name.clone(),
            output_file: Some(path),
            source_length: source.len(),
            source_present: true,
            warning: script.warning.clone(),
        });
    }

    Ok(manifest)
}

fn write_guis(
    output_folder: &Path,
    dom: &WeakDom,
    gui_roots: &[GuiRoot],
    collector: &ExtractionCollector,
) -> Result<usize> {
    let gui_root = output_folder.join("guis");
    fs::create_dir_all(&gui_root).with_context(|| {
        format!(
            "failed to create GUI extraction directory {}",
            gui_root.display()
        )
    })?;
    let mut used_paths = HashSet::new();
    let mut count = 0usize;

    for gui in gui_roots {
        let instance = dom
            .get_by_ref(gui.referent)
            .ok_or_else(|| anyhow!("DOM contains missing GUI referent"))?;
        let file_name = format!(
            "{}.{}.json",
            sanitize_filename(&instance.name),
            sanitize_filename(instance.class.as_str())
        );
        let path = unique_path(&gui_root.join(file_name), &mut used_paths);
        let mut writer = BufWriter::new(
            File::create(&path).with_context(|| format!("failed to create {}", path.display()))?,
        );
        write_gui_instance_node(&mut writer, dom, gui.referent, "game", 0, collector)?;
        writer
            .write_all(b"\n")
            .with_context(|| format!("failed to write {}", path.display()))?;
        writer
            .flush()
            .with_context(|| format!("failed to write {}", path.display()))?;
        count += 1;
    }

    Ok(count)
}

fn write_gui_instance_node(
    writer: &mut dyn Write,
    dom: &WeakDom,
    referent: Ref,
    parent_path: &str,
    indent: usize,
    collector: &ExtractionCollector,
) -> Result<()> {
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("DOM contains missing child referent"))?;
    let path = child_path(parent_path, &instance.name);
    let child_refs = instance.children().to_vec();
    let properties = serializable_properties_without_notes(instance);

    write_indent(writer, indent)?;
    writer.write_all(b"{\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"id\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &collector.id_for(referent)?)?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"name\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &instance.name)?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"class_name\": ")?;
    serde_json::to_writer_pretty(&mut *writer, instance.class.as_str())?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"roblox_path\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &path)?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"properties\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &properties)?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"children\": [")?;
    if !child_refs.is_empty() {
        writer.write_all(b"\n")?;
    }
    let mut first = true;
    for child_ref in child_refs {
        if !first {
            writer.write_all(b",\n")?;
        }
        first = false;
        write_gui_instance_node(writer, dom, child_ref, &path, indent + 2, collector)?;
    }
    if !first {
        writer.write_all(b"\n")?;
        write_indent(writer, indent + 1)?;
    }
    writer.write_all(b"]\n")?;
    write_indent(writer, indent)?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_compile_state(
    input: &Path,
    input_format: RobloxFileFormat,
    output_folder: &Path,
    instances_path: &Path,
    instance_count: usize,
    script_manifest: &[ScriptManifestEntry],
) -> Result<()> {
    let state_dir = output_folder.join(COMPILE_STATE_DIR);
    fs::create_dir_all(&state_dir).with_context(|| {
        format!(
            "failed to create compile metadata folder {}",
            state_dir.display()
        )
    })?;

    let snapshot_path = PathBuf::from(format!("original.{}", input_format.extension()));
    fs::copy(input, state_dir.join(&snapshot_path)).with_context(|| {
        format!(
            "failed to preserve original Roblox file in {}",
            state_dir.display()
        )
    })?;

    let recorded_input = input.canonicalize().unwrap_or_else(|_| input.to_path_buf());

    let metadata = CompileMetadata {
        version: 2,
        input: recorded_input,
        input_format,
        original_snapshot: snapshot_path,
        baseline_instances: None,
        instances_fingerprint: Some(file_fingerprint(instances_path)?),
        instance_count: Some(instance_count),
        scripts: script_manifest
            .iter()
            .map(|entry| CompileScriptEntry {
                id: entry.id.clone(),
                roblox_path: entry.roblox_path.clone(),
                class_name: entry.class_name.clone(),
                source_file: entry
                    .output_file
                    .as_ref()
                    .map(|path| relative_output_path(output_folder, path)),
            })
            .collect(),
    };
    write_json(&state_dir.join(COMPILE_METADATA_FILE), &metadata)
}

fn relative_output_path(output_folder: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(output_folder)
        .unwrap_or(path)
        .to_path_buf()
}

fn script_source(
    instance: &rbx_dom_weak::Instance,
    roblox_path: &str,
) -> (Option<String>, Option<String>) {
    match instance.properties.get(&ustr("Source")) {
        Some(Variant::String(source)) => (Some(source.clone()), None),
        Some(other) => (
            None,
            Some(format!(
                "{} has Source property with unsupported type {:?}",
                roblox_path,
                other.ty()
            )),
        ),
        None => (
            None,
            Some(format!("{roblox_path} is missing Source property")),
        ),
    }
}

fn serializable_properties(
    instance: &rbx_dom_weak::Instance,
    roblox_path: &str,
    collector: &mut ExtractionCollector,
) -> Result<BTreeMap<String, Value>> {
    let mut properties = BTreeMap::new();

    for (name, value) in &instance.properties {
        let name = name.to_string();
        if name == "Source" {
            continue;
        }

        match variant_to_json_value(value) {
            Some(value) => {
                properties.insert(name, value);
            }
            None => {
                let note = format!(
                    "{}.{} skipped unsupported property type {:?}",
                    roblox_path,
                    name,
                    value.ty()
                );
                collector.record_unsupported_property(note)?;
            }
        }
    }

    Ok(properties)
}

fn serializable_properties_without_notes(
    instance: &rbx_dom_weak::Instance,
) -> BTreeMap<String, Value> {
    let mut properties = BTreeMap::new();
    for (name, value) in &instance.properties {
        let name = name.to_string();
        if name == "Source" {
            continue;
        }
        if let Some(value) = variant_to_json_value(value) {
            properties.insert(name, value);
        }
    }
    properties
}

fn collect_content_refs(
    instance: &rbx_dom_weak::Instance,
    roblox_path: &str,
    class_name: &str,
    collector: &mut ExtractionCollector,
) -> Result<()> {
    for (property_name, value) in &instance.properties {
        let property_name = property_name.to_string();
        for reference in content_reference_values(&property_name, value) {
            collector.record_content_ref(ContentReference {
                roblox_path: roblox_path.to_owned(),
                class_name: class_name.to_owned(),
                property_name: property_name.clone(),
                value: reference,
            })?;
        }
    }
    Ok(())
}

fn content_reference_values(property_name: &str, value: &Variant) -> Vec<String> {
    match value {
        Variant::ContentId(value) => string_content_refs(property_name, value.as_str()),
        Variant::Content(value) => value
            .as_uri()
            .map(|uri| string_content_refs(property_name, uri))
            .unwrap_or_default(),
        Variant::String(value) => string_content_refs(property_name, value),
        _ => Vec::new(),
    }
}

fn string_content_refs(property_name: &str, value: &str) -> Vec<String> {
    if value.trim().is_empty() {
        return Vec::new();
    }
    if contains_asset_reference(value) || property_looks_like_asset_reference(property_name) {
        vec![value.to_owned()]
    } else {
        Vec::new()
    }
}

fn contains_asset_reference(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("rbxassetid://")
        || value.contains("rbxasset://")
        || value.contains("roblox.com/asset")
        || value.contains("assetdelivery.roblox.com")
        || value.contains("rbxthumb://")
}

fn property_looks_like_asset_reference(property_name: &str) -> bool {
    let property = property_name.to_ascii_lowercase();
    [
        "asset",
        "animation",
        "font",
        "icon",
        "image",
        "mesh",
        "sound",
        "texture",
        "video",
    ]
    .iter()
    .any(|needle| property.contains(needle))
}

fn is_script_class(class_name: &str) -> bool {
    SCRIPT_CLASSES.contains(&class_name)
}

fn is_gui_root(class_name: &str) -> bool {
    matches!(class_name, "ScreenGui" | "BillboardGui" | "SurfaceGui")
}

fn script_extension(class_name: &str) -> &'static str {
    match class_name {
        "LocalScript" => ".client.luau",
        "ModuleScript" => ".module.luau",
        _ => ".server.luau",
    }
}

fn unique_path(path: &Path, used_paths: &mut HashSet<PathBuf>) -> PathBuf {
    if used_paths.insert(path.to_path_buf()) && !path.exists() {
        return path.to_path_buf();
    }

    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("item");
    let extension = path.extension().and_then(|extension| extension.to_str());

    for index in 2usize.. {
        let file_name = match extension {
            Some(extension) => format!("{stem}_{index}.{extension}"),
            None => format!("{stem}_{index}"),
        };
        let candidate = parent.join(file_name);
        if used_paths.insert(candidate.clone()) && !candidate.exists() {
            return candidate;
        }
    }

    unreachable!("infinite path suffix search must return")
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create JSON output directory {}",
            parent.display()
        )
    })?;
    let mut writer = BufWriter::new(
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?,
    );
    serde_json::to_writer_pretty(&mut writer, value)
        .context("failed to serialize extraction JSON")?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("failed to write {}", path.display()))?;
    writer
        .flush()
        .with_context(|| format!("failed to write {}", path.display()))
}

#[allow(clippy::too_many_arguments)]
fn write_manifest(
    path: &Path,
    input: &Path,
    output_folder: &Path,
    input_format: RobloxFileFormat,
    counts: &ExtractionCounts,
    scripts: &[ScriptManifestEntry],
    warnings_path: &Path,
    unsupported_path: &Path,
) -> Result<()> {
    let mut writer = BufWriter::new(
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?,
    );
    writer.write_all(b"{\n  \"input\": ")?;
    serde_json::to_writer(&mut writer, input)?;
    writer.write_all(b",\n  \"output_folder\": ")?;
    serde_json::to_writer(&mut writer, output_folder)?;
    writer.write_all(b",\n  \"timestamp\": ")?;
    serde_json::to_writer(&mut writer, &timestamp_seconds())?;
    writer.write_all(b",\n  \"tool_version\": ")?;
    serde_json::to_writer(&mut writer, env!("CARGO_PKG_VERSION"))?;
    writer.write_all(b",\n  \"mode\": \"extract\",\n  \"input_format\": ")?;
    serde_json::to_writer(&mut writer, &input_format)?;
    writer.write_all(b",\n  \"counts\": ")?;
    serde_json::to_writer(&mut writer, counts)?;
    writer.write_all(b",\n  \"scripts\": ")?;
    serde_json::to_writer(&mut writer, scripts)?;
    writer.write_all(b",\n  \"warnings\": ")?;
    copy_json_file(&mut writer, warnings_path)?;
    writer.write_all(b",\n  \"unsupported_properties\": ")?;
    copy_json_file(&mut writer, unsupported_path)?;
    writer.write_all(b"\n}\n")?;
    writer
        .flush()
        .with_context(|| format!("failed to write {}", path.display()))
}

fn copy_json_file(writer: &mut dyn Write, path: &Path) -> Result<()> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    io::copy(&mut reader, writer).with_context(|| format!("failed to copy {}", path.display()))?;
    Ok(())
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let modified = metadata.modified().ok().and_then(|time| {
        time.duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
    });
    Ok(FileFingerprint {
        len: metadata.len(),
        modified_secs: modified.map(|(secs, _)| secs),
        modified_nanos: modified.map(|(_, nanos)| nanos),
    })
}

fn timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn render_extract_progress(event: ProgressEvent, verbose: bool) {
    match event {
        ProgressEvent::StageStarted {
            stage_index,
            stage_total,
            name,
        } => eprintln!("Stage {stage_index}/{stage_total}: {name}"),
        ProgressEvent::CurrentItem { label, value } if verbose => eprintln!("{label}: {value}"),
        ProgressEvent::Warning { message } => eprintln!("warning: {message}"),
        ProgressEvent::Finished => eprintln!("Extraction complete"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::{types::ContentId, InstanceBuilder};

    #[test]
    fn content_reference_detection_finds_rbxassetid_values() {
        let refs = string_content_refs("TextureId", "rbxassetid://123");

        assert_eq!(refs, vec!["rbxassetid://123"]);
    }

    #[test]
    fn duplicate_paths_receive_unique_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let mut used = HashSet::new();
        let first = unique_path(&dir.path().join("Main.server.luau"), &mut used);
        let second = unique_path(&dir.path().join("Main.server.luau"), &mut used);

        assert_eq!(first.file_name().unwrap(), "Main.server.luau");
        assert_eq!(second.file_name().unwrap(), "Main.server_2.luau");
    }

    #[test]
    fn gui_json_export_skips_unsupported_properties() {
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ScreenGui")
                    .with_name("Main")
                    .with_property(
                        "Binary",
                        rbx_dom_weak::types::BinaryString::from(vec![1, 2]),
                    ),
            ),
        );

        let collector = collect_components(&dom).unwrap();

        assert_eq!(collector.gui_roots.len(), 1);
        assert_eq!(collector.unsupported_properties_count, 1);
        assert_eq!(collector.warnings_count, 1);
    }

    #[test]
    fn manifest_counts_match_collected_components() {
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ServerScriptService").with_child(
                    InstanceBuilder::new("Script")
                        .with_name("Main")
                        .with_property("Source", "print('hi')")
                        .with_property("SoundId", ContentId::from("rbxassetid://42")),
                ),
            ),
        );

        let collector = collect_components(&dom).unwrap();

        assert_eq!(collector.total_instances, 2);
        assert_eq!(collector.scripts.len(), 1);
        assert_eq!(collector.content_refs_count, 1);
    }

    #[test]
    fn extraction_progress_sequence_includes_three_stages() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.rbxl");
        let output = dir.path().join("out");
        fs::write(&input, "").unwrap();
        let error = validate_extract_output(&input, &input).unwrap_err();

        assert!(format!("{error:#}").contains("must not overwrite input"));
        assert!(validate_extract_output(&input, &output).is_ok());
    }

    #[test]
    fn default_output_folder_uses_input_stem_next_to_input() {
        assert_eq!(
            default_output_folder(Path::new("/tmp/train game.rbxl")).unwrap(),
            PathBuf::from("/tmp/train game")
        );
        assert_eq!(
            default_output_folder(Path::new("Model.rbxm")).unwrap(),
            PathBuf::from("Model")
        );
    }

    #[test]
    fn extraction_run_writes_manifest_and_progress_events() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.rbxl");
        let output = dir.path().join("extracted");
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("StarterGui")
                    .with_child(InstanceBuilder::new("ScreenGui").with_name("MainMenu"))
                    .with_child(
                        InstanceBuilder::new("LocalScript")
                            .with_name("Controller")
                            .with_property("Source", "print('hi')"),
                    ),
            ),
        );
        crate::write_roblox_file(&input, &dom, RobloxFileFormat::Rbxl).unwrap();
        let mut events = Vec::new();

        let summary = run_with_progress(
            ExtractOptions {
                input,
                output_folder: output.clone(),
                verbose: false,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(summary.scripts_found, 1);
        assert_eq!(summary.scripts_exported, 1);
        assert_eq!(summary.guis_exported, 1);
        assert!(output.join("manifest.json").exists());
        assert!(output
            .join(COMPILE_STATE_DIR)
            .join(COMPILE_METADATA_FILE)
            .exists());
        assert!(!output
            .join(COMPILE_STATE_DIR)
            .join("baseline_instances.json")
            .exists());
        assert!(events
            .iter()
            .any(|event| matches!(event, ProgressEvent::StageStarted { stage_index: 1, .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, ProgressEvent::StageStarted { stage_index: 3, .. })));
    }
}
