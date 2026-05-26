use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::{
    types::{Content, ContentId, ContentType, Ref, Variant},
    ustr, WeakDom,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    detect_binary_compression, read_roblox_file, same_path, validate_input_format,
    write_roblox_file_with_binary_compression, CompileMetadata, ProgressEvent,
    COMPILE_METADATA_FILE, COMPILE_STATE_DIR, SCRIPT_CLASSES,
};

#[derive(Debug)]
pub struct CompileOptions {
    pub input_folder: PathBuf,
    pub output: Option<PathBuf>,
    pub verbose: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompileSummary {
    pub input_folder: PathBuf,
    pub output: PathBuf,
    pub scripts_updated: usize,
    pub instances_changed: usize,
    pub duration: Duration,
}

#[derive(Clone, Debug, Deserialize)]
struct InstanceNode {
    id: String,
    name: String,
    class_name: String,
    #[allow(dead_code)]
    roblox_path: String,
    properties: BTreeMap<String, Value>,
    children: Vec<InstanceNode>,
}

#[derive(Clone, Debug)]
struct FlatNode {
    name: String,
    class_name: String,
    parent_id: Option<String>,
    child_ids: Vec<String>,
    properties: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
struct InstanceIndex {
    nodes: HashMap<String, FlatNode>,
    root_ids: Vec<String>,
}

pub fn run(options: CompileOptions) -> Result<CompileSummary> {
    let verbose = options.verbose;
    run_with_progress(options, |event| render_compile_progress(event, verbose))
}

pub fn run_with_progress<F>(options: CompileOptions, progress: F) -> Result<CompileSummary>
where
    F: FnMut(ProgressEvent),
{
    run_with_progress_controlled(options, || false, progress)
}

pub fn run_with_progress_controlled<C, F>(
    options: CompileOptions,
    mut should_cancel: C,
    mut progress: F,
) -> Result<CompileSummary>
where
    C: FnMut() -> bool,
    F: FnMut(ProgressEvent),
{
    let started_at = Instant::now();
    validate_compile_project(&options.input_folder)?;

    progress(ProgressEvent::StageStarted {
        stage_index: 1,
        stage_total: 3,
        name: "Load extracted project".to_owned(),
    });
    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Reading compile metadata".to_owned(),
    });
    let metadata = read_metadata(&options.input_folder)?;
    let output = match &options.output {
        Some(output) => output.clone(),
        None => default_output_path(&metadata)?,
    };
    let output_format = validate_input_format(&output)?;
    validate_compile_output(&options.input_folder, &metadata, &output)?;
    let state_dir = state_dir(&options.input_folder);
    let snapshot = state_dir.join(&metadata.original_snapshot);

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Loading original Roblox file".to_owned(),
    });
    let mut dom = read_roblox_file(&snapshot, metadata.input_format)?;
    let baseline = read_instance_tree(&state_dir.join(&metadata.baseline_instances))?;
    let current = read_instance_tree(&options.input_folder.join("instances.json"))?;
    let ref_map = build_ref_map(&dom, &baseline)?;
    let baseline_nodes = flatten_nodes(&baseline)?;
    let current_nodes = flatten_nodes(&current)?;
    validate_instance_diff(&baseline_nodes, &current_nodes)?;
    if should_cancel() {
        bail!("operation cancelled");
    }
    progress(ProgressEvent::StageCompleted {
        stage_index: 1,
        stage_total: 3,
        name: "Load extracted project".to_owned(),
    });

    progress(ProgressEvent::StageStarted {
        stage_index: 2,
        stage_total: 3,
        name: "Apply extracted edits".to_owned(),
    });
    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Applying instances.json changes".to_owned(),
    });
    let instances_changed =
        apply_instance_changes(&mut dom, &ref_map, &baseline_nodes, &current_nodes)?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Applying script sources".to_owned(),
    });
    let scripts_total = metadata
        .scripts
        .iter()
        .filter(|script| script.source_file.is_some())
        .count();
    let scripts_updated = apply_script_changes(
        &mut dom,
        &options.input_folder,
        &metadata,
        &ref_map,
        scripts_total,
        &mut progress,
    )?;
    if should_cancel() {
        bail!("operation cancelled");
    }
    progress(ProgressEvent::StageCompleted {
        stage_index: 2,
        stage_total: 3,
        name: "Apply extracted edits".to_owned(),
    });

    progress(ProgressEvent::StageStarted {
        stage_index: 3,
        stage_total: 3,
        name: "Write compiled file".to_owned(),
    });
    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: output.display().to_string(),
    });
    let binary_compression = detect_binary_compression(&snapshot, metadata.input_format)?;
    write_roblox_file_with_binary_compression(&output, &dom, output_format, binary_compression)?;
    progress(ProgressEvent::StageCompleted {
        stage_index: 3,
        stage_total: 3,
        name: "Write compiled file".to_owned(),
    });
    progress(ProgressEvent::Finished);

    Ok(CompileSummary {
        input_folder: options.input_folder,
        output,
        scripts_updated,
        instances_changed,
        duration: started_at.elapsed(),
    })
}

pub fn validate_compile_project(input_folder: &Path) -> Result<()> {
    if !input_folder.exists() {
        bail!(
            "extracted project folder does not exist: {}",
            input_folder.display()
        );
    }
    if !input_folder.is_dir() {
        bail!(
            "compile input must be an extracted project folder: {}",
            input_folder.display()
        );
    }
    let state = state_dir(input_folder);
    let metadata = state.join(COMPILE_METADATA_FILE);
    if !metadata.is_file() {
        bail!(
            "compile metadata is missing: {}. Re-extract the Roblox file with this version before compiling",
            metadata.display()
        );
    }
    if !input_folder.join("instances.json").is_file() {
        bail!(
            "instances.json is missing from extracted project: {}",
            input_folder.display()
        );
    }
    Ok(())
}

pub fn default_output_for_project(input_folder: &Path) -> Result<PathBuf> {
    let metadata = read_metadata(input_folder)?;
    default_output_path(&metadata)
}

pub fn validate_compile_output_for_project(input_folder: &Path, output: &Path) -> Result<()> {
    let metadata = read_metadata(input_folder)?;
    validate_compile_output(input_folder, &metadata, output)
}

fn validate_compile_output(
    input_folder: &Path,
    metadata: &CompileMetadata,
    output: &Path,
) -> Result<()> {
    validate_input_format(output)?;
    if output.exists() && output.is_dir() {
        bail!(
            "compile output path is a folder, expected a file: {}",
            output.display()
        );
    }

    let snapshot = state_dir(input_folder).join(&metadata.original_snapshot);
    if same_path(&snapshot, output)? {
        bail!(
            "compile output must not overwrite preserved original snapshot: {}",
            output.display()
        );
    }
    if metadata.input.exists() && same_path(&metadata.input, output)? {
        bail!(
            "compile output must not overwrite original input file: {}",
            output.display()
        );
    }
    Ok(())
}

fn state_dir(input_folder: &Path) -> PathBuf {
    input_folder.join(COMPILE_STATE_DIR)
}

fn read_metadata(input_folder: &Path) -> Result<CompileMetadata> {
    let path = state_dir(input_folder).join(COMPILE_METADATA_FILE);
    let metadata: CompileMetadata = read_json(&path)?;
    if metadata.version != 1 {
        bail!(
            "unsupported compile metadata version {} in {}",
            metadata.version,
            path.display()
        );
    }
    let state = state_dir(input_folder);
    if !state.join(&metadata.original_snapshot).is_file() {
        bail!(
            "preserved original Roblox file is missing: {}",
            state.join(&metadata.original_snapshot).display()
        );
    }
    if !state.join(&metadata.baseline_instances).is_file() {
        bail!(
            "baseline instance metadata is missing: {}",
            state.join(&metadata.baseline_instances).display()
        );
    }
    Ok(metadata)
}

fn default_output_path(metadata: &CompileMetadata) -> Result<PathBuf> {
    let stem = metadata.input.file_stem().ok_or_else(|| {
        anyhow!(
            "original input path has no file name: {}",
            metadata.input.display()
        )
    })?;
    let mut file_name = stem.to_os_string();
    file_name.push("-compiled.");
    file_name.push(metadata.input_format.extension());
    Ok(metadata.input.with_file_name(file_name))
}

fn read_instance_tree(path: &Path) -> Result<Vec<InstanceNode>> {
    read_json(path)
}

fn build_ref_map(dom: &WeakDom, baseline_roots: &[InstanceNode]) -> Result<HashMap<String, Ref>> {
    let root = dom
        .get_by_ref(dom.root_ref())
        .ok_or_else(|| anyhow!("preserved Roblox DOM root is missing"))?;
    let root_children = root.children();
    if root_children.len() != baseline_roots.len() {
        bail!(
            "preserved Roblox file no longer matches extraction baseline: expected {} root instance(s), found {}",
            baseline_roots.len(),
            root_children.len()
        );
    }

    let mut refs = HashMap::new();
    for (node, referent) in baseline_roots.iter().zip(root_children.iter().copied()) {
        map_baseline_refs(dom, node, referent, &mut refs)?;
    }
    Ok(refs)
}

fn map_baseline_refs(
    dom: &WeakDom,
    node: &InstanceNode,
    referent: Ref,
    refs: &mut HashMap<String, Ref>,
) -> Result<()> {
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("preserved Roblox file is missing an expected instance"))?;
    if instance.name != node.name || instance.class.as_str() != node.class_name {
        bail!(
            "preserved Roblox file no longer matches extraction baseline at id {}: expected {} {}, found {} {}",
            node.id,
            node.class_name,
            node.name,
            instance.class,
            instance.name
        );
    }
    if refs.insert(node.id.clone(), referent).is_some() {
        bail!(
            "baseline instance metadata contains duplicate id {}",
            node.id
        );
    }

    let children = instance.children();
    if children.len() != node.children.len() {
        bail!(
            "preserved Roblox file no longer matches extraction baseline at id {}: expected {} child instance(s), found {}",
            node.id,
            node.children.len(),
            children.len()
        );
    }
    for (child_node, child_ref) in node.children.iter().zip(children.iter().copied()) {
        map_baseline_refs(dom, child_node, child_ref, refs)?;
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn flatten_nodes(nodes: &[InstanceNode]) -> Result<InstanceIndex> {
    let mut flattened = HashMap::new();
    let root_ids = nodes.iter().map(|node| node.id.clone()).collect();
    for node in nodes {
        flatten_node(node, None, &mut flattened)?;
    }
    Ok(InstanceIndex {
        nodes: flattened,
        root_ids,
    })
}

fn flatten_node(
    node: &InstanceNode,
    parent_id: Option<&str>,
    flattened: &mut HashMap<String, FlatNode>,
) -> Result<()> {
    if node.id.trim().is_empty() {
        bail!("instances.json contains an instance with an empty id");
    }
    if flattened.contains_key(&node.id) {
        bail!("instances.json contains duplicate instance id {}", node.id);
    }

    let child_ids = node
        .children
        .iter()
        .map(|child| child.id.clone())
        .collect::<Vec<_>>();
    flattened.insert(
        node.id.clone(),
        FlatNode {
            name: node.name.clone(),
            class_name: node.class_name.clone(),
            parent_id: parent_id.map(str::to_owned),
            child_ids,
            properties: node.properties.clone(),
        },
    );

    for child in &node.children {
        flatten_node(child, Some(&node.id), flattened)?;
    }
    Ok(())
}

fn validate_instance_diff(baseline: &InstanceIndex, current: &InstanceIndex) -> Result<()> {
    let baseline_ids = baseline.nodes.keys().collect::<BTreeSet<_>>();
    let current_ids = current.nodes.keys().collect::<BTreeSet<_>>();
    if baseline_ids != current_ids {
        let added = current_ids
            .difference(&baseline_ids)
            .map(|id| id.as_str())
            .collect::<Vec<_>>();
        let deleted = baseline_ids
            .difference(&current_ids)
            .map(|id| id.as_str())
            .collect::<Vec<_>>();
        bail!(
            "instances.json cannot add or delete instances in this version; added: [{}], deleted: [{}]",
            added.join(", "),
            deleted.join(", ")
        );
    }

    for (id, baseline_node) in &baseline.nodes {
        let current_node = current
            .nodes
            .get(id)
            .ok_or_else(|| anyhow!("missing instance id {id}"))?;
        if baseline_node.class_name != current_node.class_name {
            bail!(
                "instances.json cannot change class for id {id}: {} -> {}",
                baseline_node.class_name,
                current_node.class_name
            );
        }

        let baseline_properties = baseline_node.properties.keys().collect::<BTreeSet<_>>();
        let current_properties = current_node.properties.keys().collect::<BTreeSet<_>>();
        if baseline_properties != current_properties {
            bail!(
                "instances.json cannot add or delete properties for id {id}; only existing exported properties can be edited"
            );
        }
    }

    Ok(())
}

fn apply_instance_changes(
    dom: &mut WeakDom,
    ref_map: &HashMap<String, Ref>,
    baseline: &InstanceIndex,
    current: &InstanceIndex,
) -> Result<usize> {
    let mut changed = 0usize;

    for (id, current_node) in &current.nodes {
        let baseline_node = baseline
            .nodes
            .get(id)
            .ok_or_else(|| anyhow!("missing baseline instance id {id}"))?;
        let referent = *ref_map
            .get(id)
            .ok_or_else(|| anyhow!("missing preserved Roblox referent for instance id {id}"))?;
        let instance = dom
            .get_by_ref_mut(referent)
            .ok_or_else(|| anyhow!("original Roblox file is missing instance id {id}"))?;

        if instance.class.as_str() != baseline_node.class_name {
            bail!(
                "original Roblox file no longer matches baseline for id {id}: expected class {}, found {}",
                baseline_node.class_name,
                instance.class
            );
        }

        if current_node.name != baseline_node.name {
            instance.name = current_node.name.clone();
            changed += 1;
        }

        for (property_name, current_value) in &current_node.properties {
            let baseline_value = baseline_node
                .properties
                .get(property_name)
                .ok_or_else(|| anyhow!("missing baseline property {property_name} for id {id}"))?;
            if current_value == baseline_value {
                continue;
            }
            if property_name == "Source" {
                bail!("script Source edits must be made in scripts/, not instances.json");
            }

            let property_key = ustr(property_name);
            let original = instance.properties.get(&property_key).ok_or_else(|| {
                anyhow!("original Roblox file is missing property {property_name} for id {id}")
            })?;
            let updated = json_to_variant(original, current_value, id, property_name)?;
            instance.properties.insert(property_key, updated);
            changed += 1;
        }
    }

    for (id, current_node) in &current.nodes {
        let baseline_node = baseline
            .nodes
            .get(id)
            .ok_or_else(|| anyhow!("missing baseline instance id {id}"))?;
        if current_node.parent_id == baseline_node.parent_id {
            continue;
        }

        let referent = *ref_map
            .get(id)
            .ok_or_else(|| anyhow!("missing preserved Roblox referent for instance id {id}"))?;
        let parent = current_node
            .parent_id
            .as_deref()
            .map(|parent_id| {
                ref_map.get(parent_id).copied().ok_or_else(|| {
                    anyhow!("missing preserved Roblox referent for parent id {parent_id}")
                })
            })
            .transpose()?
            .unwrap_or_else(|| dom.root_ref());
        dom.transfer_within(referent, parent);
        changed += 1;
    }

    reorder_children(dom, ref_map, current)?;
    Ok(changed + order_change_count(baseline, current))
}

fn reorder_children(
    dom: &mut WeakDom,
    ref_map: &HashMap<String, Ref>,
    current: &InstanceIndex,
) -> Result<()> {
    for child_id in &current.root_ids {
        let child_ref = *ref_map.get(child_id).ok_or_else(|| {
            anyhow!("missing preserved Roblox referent for instance id {child_id}")
        })?;
        dom.transfer_within(child_ref, dom.root_ref());
    }

    for (parent_id, parent_node) in &current.nodes {
        let parent = *ref_map.get(parent_id).ok_or_else(|| {
            anyhow!("missing preserved Roblox referent for parent id {parent_id}")
        })?;
        for child_id in &parent_node.child_ids {
            let child_ref = *ref_map.get(child_id).ok_or_else(|| {
                anyhow!("missing preserved Roblox referent for instance id {child_id}")
            })?;
            dom.transfer_within(child_ref, parent);
        }
    }
    Ok(())
}

fn order_change_count(baseline: &InstanceIndex, current: &InstanceIndex) -> usize {
    let mut count = 0usize;
    if baseline.root_ids != current.root_ids {
        count += 1;
    }
    for (id, current_node) in &current.nodes {
        if baseline
            .nodes
            .get(id)
            .map(|node| node.child_ids.as_slice() != current_node.child_ids.as_slice())
            .unwrap_or(false)
        {
            count += 1;
        }
    }
    count
}

fn apply_script_changes<F>(
    dom: &mut WeakDom,
    input_folder: &Path,
    metadata: &CompileMetadata,
    ref_map: &HashMap<String, Ref>,
    scripts_total: usize,
    progress: &mut F,
) -> Result<usize>
where
    F: FnMut(ProgressEvent),
{
    let mut updated = 0usize;
    for script in &metadata.scripts {
        let Some(source_file) = &script.source_file else {
            continue;
        };
        let path = input_folder.join(source_file);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read script source {}", path.display()))?;
        let referent = *ref_map.get(&script.id).ok_or_else(|| {
            anyhow!(
                "missing preserved Roblox referent for script id {}",
                script.id
            )
        })?;
        let instance = dom
            .get_by_ref_mut(referent)
            .ok_or_else(|| anyhow!("original Roblox file is missing script id {}", script.id))?;
        if !SCRIPT_CLASSES.contains(&instance.class.as_str()) {
            bail!(
                "metadata script id {} points to non-script class {}",
                script.id,
                instance.class
            );
        }
        instance
            .properties
            .insert(ustr("Source"), Variant::String(source));
        updated += 1;
        progress(ProgressEvent::ScriptProgress {
            completed: updated,
            total: scripts_total,
            current_path: Some(script.roblox_path.clone()),
        });
    }
    if scripts_total == 0 {
        progress(ProgressEvent::ScriptProgress {
            completed: 0,
            total: 0,
            current_path: None,
        });
    }
    Ok(updated)
}

fn json_to_variant(
    original: &Variant,
    value: &Value,
    id: &str,
    property_name: &str,
) -> Result<Variant> {
    match original {
        Variant::Bool(_) => value
            .as_bool()
            .map(Variant::Bool)
            .ok_or_else(|| type_error(id, property_name, "boolean")),
        Variant::Float32(_) => number(value, id, property_name).map(|number| Variant::Float32(number as f32)),
        Variant::Float64(_) => number(value, id, property_name).map(Variant::Float64),
        Variant::Int32(_) => integer(value, id, property_name)
            .and_then(|number| {
                i32::try_from(number).with_context(|| {
                    format!("property {property_name} for id {id} is outside Int32 range")
                })
            })
            .map(Variant::Int32),
        Variant::Int64(_) => integer(value, id, property_name).map(Variant::Int64),
        Variant::String(_) => value
            .as_str()
            .map(|text| Variant::String(text.to_owned()))
            .ok_or_else(|| type_error(id, property_name, "string")),
        Variant::ContentId(_) => value
            .as_str()
            .map(|text| Variant::ContentId(ContentId::from(text)))
            .ok_or_else(|| type_error(id, property_name, "string content id")),
        Variant::Content(original_content) => match original_content.value() {
            ContentType::None => {
                if value.is_null() {
                    Ok(Variant::Content(Content::none()))
                } else if let Some(uri) = value.as_str() {
                    Ok(Variant::Content(Content::from_uri(uri)))
                } else {
                    Err(type_error(id, property_name, "null or string content uri"))
                }
            }
            ContentType::Uri(_) => value
                .as_str()
                .map(|uri| Variant::Content(Content::from_uri(uri)))
                .ok_or_else(|| type_error(id, property_name, "string content uri")),
            ContentType::Object(_) => {
                bail!(
                    "property {property_name} for id {id} is an object Content reference and cannot be edited from JSON"
                )
            }
            _ => {
                bail!(
                    "property {property_name} for id {id} has unsupported Content data and cannot be edited from JSON"
                )
            }
        },
        _ => bail!(
            "property {property_name} for id {id} has unsupported Roblox type {:?} and cannot be edited from JSON",
            original.ty()
        ),
    }
}

fn number(value: &Value, id: &str, property_name: &str) -> Result<f64> {
    let number = value
        .as_f64()
        .ok_or_else(|| type_error(id, property_name, "number"))?;
    if !number.is_finite() {
        bail!("property {property_name} for id {id} must be finite");
    }
    Ok(number)
}

fn integer(value: &Value, id: &str, property_name: &str) -> Result<i64> {
    value
        .as_i64()
        .ok_or_else(|| type_error(id, property_name, "integer"))
}

fn type_error(id: &str, property_name: &str, expected: &str) -> anyhow::Error {
    anyhow!("property {property_name} for id {id} must be a {expected}")
}

fn render_compile_progress(event: ProgressEvent, verbose: bool) {
    match event {
        ProgressEvent::StageStarted {
            stage_index,
            stage_total,
            name,
        } => eprintln!("Stage {stage_index}/{stage_total}: {name}"),
        ProgressEvent::CurrentItem { label, value } if verbose => eprintln!("{label}: {value}"),
        ProgressEvent::ScriptProgress {
            completed, total, ..
        } if verbose && total > 0 => eprintln!("Scripts: {completed}/{total}"),
        ProgressEvent::Warning { message } => eprintln!("warning: {message}"),
        ProgressEvent::Finished => eprintln!("Compile complete"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RobloxBinaryCompression, RobloxFileFormat};
    use rbx_dom_weak::InstanceBuilder;

    #[test]
    fn compile_updates_script_sources_and_instance_names() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("game.rbxl");
        let extracted = dir.path().join("game");
        let output = dir.path().join("game-compiled.rbxl");
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("ReplicatedStorage")
                    .with_child(InstanceBuilder::new("Folder").with_name("Config"))
                    .with_child(
                        InstanceBuilder::new("ModuleScript")
                            .with_name("Settings")
                            .with_property("Source", "return { value = 1 }"),
                    ),
            ),
        );
        crate::write_roblox_file(&input, &dom, RobloxFileFormat::Rbxl).unwrap();

        crate::extract::run(crate::extract::ExtractOptions {
            input,
            output_folder: extracted.clone(),
            verbose: false,
        })
        .unwrap();

        let mut instances: Value =
            read_json(&extracted.join("instances.json")).expect("instances.json should parse");
        rename_first_class(&mut instances, "Folder", "RenamedConfig");
        fs::write(
            extracted.join("instances.json"),
            serde_json::to_string_pretty(&instances).unwrap(),
        )
        .unwrap();

        let metadata = read_metadata(&extracted).unwrap();
        let script_path = metadata
            .scripts
            .iter()
            .find_map(|script| script.source_file.as_ref())
            .map(|path| extracted.join(path))
            .unwrap();
        fs::write(script_path, "return { value = 2 }").unwrap();

        let summary = run(CompileOptions {
            input_folder: extracted,
            output: Some(output.clone()),
            verbose: false,
        })
        .unwrap();

        assert_eq!(summary.scripts_updated, 1);
        assert!(summary.instances_changed >= 1);

        let output_dom = crate::read_roblox_file(&output, RobloxFileFormat::Rbxl).unwrap();
        let mut found_renamed = false;
        let mut found_source = false;
        for instance in output_dom.descendants() {
            if instance.name == "RenamedConfig" {
                found_renamed = true;
            }
            if instance.name == "Settings" {
                found_source = matches!(
                    instance.properties.get(&ustr("Source")),
                    Some(Variant::String(source)) if source == "return { value = 2 }"
                );
            }
        }
        assert!(found_renamed);
        assert!(found_source);
    }

    #[test]
    fn compile_rejects_added_instances() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("game.rbxl");
        let extracted = dir.path().join("game");
        let output = dir.path().join("game-compiled.rbxl");
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new("ReplicatedStorage")),
        );
        crate::write_roblox_file(&input, &dom, RobloxFileFormat::Rbxl).unwrap();
        crate::extract::run(crate::extract::ExtractOptions {
            input,
            output_folder: extracted.clone(),
            verbose: false,
        })
        .unwrap();

        let mut instances: Value = read_json(&extracted.join("instances.json")).unwrap();
        let children = instances.as_array_mut().unwrap();
        children.push(serde_json::json!({
            "id": "00000000000000000000000000000001",
            "name": "New",
            "class_name": "Folder",
            "roblox_path": "game.New",
            "properties": {},
            "children": []
        }));
        fs::write(
            extracted.join("instances.json"),
            serde_json::to_string_pretty(&instances).unwrap(),
        )
        .unwrap();

        let error = run(CompileOptions {
            input_folder: extracted,
            output: Some(output),
            verbose: false,
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("cannot add or delete instances"));
    }

    #[test]
    fn compile_preserves_zstd_binary_compression() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("game.rbxl");
        let extracted = dir.path().join("game");
        let output = dir.path().join("game-compiled.rbxl");
        let dom = WeakDom::new(
            InstanceBuilder::new("DataModel").with_child(
                InstanceBuilder::new("Script")
                    .with_name("Main")
                    .with_property("Source", "print('hi')"),
            ),
        );
        crate::write_roblox_file_with_binary_compression(
            &input,
            &dom,
            RobloxFileFormat::Rbxl,
            Some(RobloxBinaryCompression::Zstd),
        )
        .unwrap();

        crate::extract::run(crate::extract::ExtractOptions {
            input: input.clone(),
            output_folder: extracted.clone(),
            verbose: false,
        })
        .unwrap();
        run(CompileOptions {
            input_folder: extracted,
            output: Some(output.clone()),
            verbose: false,
        })
        .unwrap();

        assert_eq!(
            crate::detect_binary_compression(&input, RobloxFileFormat::Rbxl).unwrap(),
            Some(RobloxBinaryCompression::Zstd)
        );
        assert_eq!(
            crate::detect_binary_compression(&output, RobloxFileFormat::Rbxl).unwrap(),
            Some(RobloxBinaryCompression::Zstd)
        );
        crate::read_roblox_file(&output, RobloxFileFormat::Rbxl).unwrap();
    }

    fn rename_first_class(value: &mut Value, class_name: &str, new_name: &str) -> bool {
        let Some(nodes) = value.as_array_mut() else {
            return false;
        };
        for node in nodes {
            if node
                .get("class_name")
                .and_then(Value::as_str)
                .is_some_and(|value| value == class_name)
            {
                node["name"] = Value::String(new_name.to_owned());
                return true;
            }
            if rename_first_class(&mut node["children"], class_name, new_name) {
                return true;
            }
        }
        false
    }
}
