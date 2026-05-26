use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::{
    types::{ContentType, Ref, Variant},
    ustr, WeakDom,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    child_path, read_roblox_file, same_path, sanitize_filename, validate_input_format,
    CompileMetadata, CompileScriptEntry, ProgressEvent, RobloxFileFormat,
    COMPILE_BASELINE_INSTANCES_FILE, COMPILE_METADATA_FILE, COMPILE_STATE_DIR, SCRIPT_CLASSES,
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
struct ExtractionManifest {
    input: PathBuf,
    output_folder: PathBuf,
    timestamp: u64,
    tool_version: &'static str,
    mode: &'static str,
    input_format: RobloxFileFormat,
    counts: ExtractionCounts,
    scripts: Vec<ScriptManifestEntry>,
    warnings: Vec<String>,
    unsupported_properties: Vec<String>,
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
struct InstanceNode {
    id: String,
    name: String,
    class_name: String,
    roblox_path: String,
    properties: BTreeMap<String, Value>,
    children: Vec<InstanceNode>,
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

#[derive(Default)]
struct ExtractionCollector {
    instance_roots: Vec<InstanceNode>,
    scripts: Vec<ScriptExport>,
    instance_ids: HashMap<Ref, String>,
    gui_roots: Vec<Ref>,
    content_refs: Vec<ContentReference>,
    warnings: Vec<String>,
    unsupported_properties: Vec<String>,
    total_instances: usize,
    next_instance_id: usize,
}

impl ExtractionCollector {
    fn next_id(&mut self, referent: Ref) -> String {
        self.next_instance_id += 1;
        let id = format!("inst_{:06}", self.next_instance_id);
        self.instance_ids.insert(referent, id.clone());
        id
    }

    fn id_for(&self, referent: Ref) -> Result<String> {
        self.instance_ids
            .get(&referent)
            .cloned()
            .ok_or_else(|| anyhow!("missing extraction id for instance referent {referent}"))
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

    let mut collector = collect_components(&dom)?;
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
    let guis_exported = write_guis(&options.output_folder, &dom, &gui_roots, &mut collector)?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing instances.json".to_owned(),
    });
    write_json(
        &options.output_folder.join("instances.json"),
        &collector.instance_roots,
    )?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing content_refs.json".to_owned(),
    });
    write_json(
        &options.output_folder.join("content_refs.json"),
        &collector.content_refs,
    )?;
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
        &collector.instance_roots,
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
    let warnings_count = collector.warnings.len();
    let manifest = ExtractionManifest {
        input: options.input.clone(),
        output_folder: options.output_folder.clone(),
        timestamp: timestamp_seconds(),
        tool_version: env!("CARGO_PKG_VERSION"),
        mode: "extract",
        input_format,
        counts: ExtractionCounts {
            total_instances: collector.total_instances,
            scripts_found: collector.scripts.len(),
            scripts_exported,
            guis_exported,
            content_references_found: collector.content_refs.len(),
            warnings_count,
        },
        scripts: script_manifest,
        warnings: collector.warnings.clone(),
        unsupported_properties: collector.unsupported_properties.clone(),
    };
    write_json(&options.output_folder.join("manifest.json"), &manifest)?;
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
        content_refs_found: collector.content_refs.len(),
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

fn collect_components(dom: &WeakDom) -> Result<ExtractionCollector> {
    let mut collector = ExtractionCollector::default();
    let root = dom
        .get_by_ref(dom.root_ref())
        .ok_or_else(|| anyhow!("DOM root is missing"))?;

    for child_ref in root.children().iter().copied() {
        let node = collect_instance(dom, child_ref, "game", &[], &mut collector)?;
        collector.instance_roots.push(node);
    }

    Ok(collector)
}

fn collect_instance(
    dom: &WeakDom,
    referent: Ref,
    parent_path: &str,
    parent_segments: &[String],
    collector: &mut ExtractionCollector,
) -> Result<InstanceNode> {
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("DOM contains missing child referent"))?;
    let name = instance.name.clone();
    let class_name = instance.class.to_string();
    let id = collector.next_id(referent);
    let path = child_path(parent_path, &name);
    let mut segments = parent_segments.to_vec();
    segments.push(name.clone());
    let child_refs = instance.children().to_vec();

    collector.total_instances += 1;

    if is_script_class(&class_name) {
        let (source, warning) = script_source(instance, &path);
        if let Some(warning) = &warning {
            collector.warnings.push(warning.clone());
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

    if is_gui_root(&class_name) {
        collector.gui_roots.push(referent);
    }

    collect_content_refs(instance, &path, &class_name, collector);

    let properties = serializable_properties(instance, &path, collector);
    let mut children = Vec::with_capacity(child_refs.len());
    for child_ref in child_refs {
        children.push(collect_instance(
            dom, child_ref, &path, &segments, collector,
        )?);
    }

    Ok(InstanceNode {
        id,
        name,
        class_name,
        roblox_path: path,
        properties,
        children,
    })
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
    gui_roots: &[Ref],
    collector: &mut ExtractionCollector,
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

    for gui_ref in gui_roots {
        let instance = dom
            .get_by_ref(*gui_ref)
            .ok_or_else(|| anyhow!("DOM contains missing GUI referent"))?;
        let file_name = format!(
            "{}.{}.json",
            sanitize_filename(&instance.name),
            sanitize_filename(instance.class.as_str())
        );
        let path = unique_path(&gui_root.join(file_name), &mut used_paths);
        let node = export_instance_node(dom, *gui_ref, "game", collector)?;
        write_json(&path, &node)?;
        count += 1;
    }

    Ok(count)
}

fn export_instance_node(
    dom: &WeakDom,
    referent: Ref,
    parent_path: &str,
    collector: &mut ExtractionCollector,
) -> Result<InstanceNode> {
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("DOM contains missing child referent"))?;
    let path = child_path(parent_path, &instance.name);
    let child_refs = instance.children().to_vec();
    let mut children = Vec::with_capacity(child_refs.len());
    for child_ref in child_refs {
        children.push(export_instance_node(dom, child_ref, &path, collector)?);
    }

    Ok(InstanceNode {
        id: collector.id_for(referent)?,
        name: instance.name.clone(),
        class_name: instance.class.to_string(),
        roblox_path: path.clone(),
        properties: serializable_properties(instance, &path, collector),
        children,
    })
}

fn write_compile_state(
    input: &Path,
    input_format: RobloxFileFormat,
    output_folder: &Path,
    instance_roots: &[InstanceNode],
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

    let baseline_path = PathBuf::from(COMPILE_BASELINE_INSTANCES_FILE);
    write_json(&state_dir.join(&baseline_path), &instance_roots)?;
    let recorded_input = input.canonicalize().unwrap_or_else(|_| input.to_path_buf());

    let metadata = CompileMetadata {
        version: 1,
        input: recorded_input,
        input_format,
        original_snapshot: snapshot_path,
        baseline_instances: baseline_path,
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
) -> BTreeMap<String, Value> {
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
                collector.unsupported_properties.push(note.clone());
                collector.warnings.push(note);
            }
        }
    }

    properties
}

fn variant_to_json_value(value: &Variant) -> Option<Value> {
    match value {
        Variant::BinaryString(_) | Variant::SharedString(_) | Variant::MaterialColors(_) => None,
        Variant::Bool(value) => Some(Value::Bool(*value)),
        Variant::Float32(value) => Some(Value::from(*value)),
        Variant::Float64(value) => Some(Value::from(*value)),
        Variant::Int32(value) => Some(Value::from(*value)),
        Variant::Int64(value) => Some(Value::from(*value)),
        Variant::String(value) => Some(Value::String(value.clone())),
        Variant::ContentId(value) => Some(Value::String(value.as_str().to_owned())),
        Variant::Content(value) => match value.value() {
            ContentType::None => Some(Value::Null),
            ContentType::Uri(uri) => Some(Value::String(uri.clone())),
            ContentType::Object(referent) => Some(Value::String(format!("{referent:?}"))),
            _ => None,
        },
        _ => serde_json::to_value(value).ok(),
    }
}

fn collect_content_refs(
    instance: &rbx_dom_weak::Instance,
    roblox_path: &str,
    class_name: &str,
    collector: &mut ExtractionCollector,
) {
    for (property_name, value) in &instance.properties {
        let property_name = property_name.to_string();
        for reference in content_reference_values(&property_name, value) {
            collector.content_refs.push(ContentReference {
                roblox_path: roblox_path.to_owned(),
                class_name: class_name.to_owned(),
                property_name: property_name.clone(),
                value: reference,
            });
        }
    }
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
    let json =
        serde_json::to_string_pretty(value).context("failed to serialize extraction JSON")?;
    fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))
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
        assert_eq!(collector.unsupported_properties.len(), 1);
        assert!(collector.unsupported_properties[0].contains("Binary"));
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
        assert_eq!(collector.content_refs.len(), 1);
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
        assert!(output
            .join(COMPILE_STATE_DIR)
            .join(COMPILE_BASELINE_INSTANCES_FILE)
            .exists());
        assert!(events
            .iter()
            .any(|event| matches!(event, ProgressEvent::StageStarted { stage_index: 1, .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, ProgressEvent::StageStarted { stage_index: 3, .. })));
    }
}
