use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::{
    types::{Content, ContentId, ContentType, Ref, Variant},
    ustr, WeakDom,
};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::{
    detect_binary_compression, read_roblox_file, same_path, validate_input_format,
    variant_to_json_value, write_roblox_file_with_binary_compression, CompileMetadata,
    FileFingerprint, ProgressEvent, COMPILE_BASELINE_INSTANCES_FILE, COMPILE_METADATA_FILE,
    COMPILE_STATE_DIR, SCRIPT_CLASSES,
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

    if let Some(summary) =
        crate::binary_fast::try_compile_binary_script_patch(&options, &output, &metadata)?
    {
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
        progress(ProgressEvent::StageCompleted {
            stage_index: 3,
            stage_total: 3,
            name: "Write compiled file".to_owned(),
        });
        progress(ProgressEvent::Finished);
        return Ok(summary);
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Loading original Roblox file".to_owned(),
    });
    let mut dom = read_roblox_file(&snapshot, metadata.input_format)?;
    let ref_index = build_ref_index(&dom)?;
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
        maybe_apply_instance_json_changes(&mut dom, &options.input_folder, &metadata, &ref_index)?;
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
        &ref_index,
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
    if !matches!(metadata.version, 1 | 2) {
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
    if metadata.version == 1
        && !metadata
            .baseline_instances
            .as_ref()
            .is_some_and(|baseline| state.join(baseline).is_file())
    {
        bail!(
            "baseline instance metadata is missing: {}",
            metadata
                .baseline_instances
                .as_ref()
                .map(|baseline| state.join(baseline).display().to_string())
                .unwrap_or_else(|| {
                    state
                        .join(COMPILE_BASELINE_INSTANCES_FILE)
                        .display()
                        .to_string()
                })
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

fn build_ref_index(dom: &WeakDom) -> Result<Vec<Ref>> {
    let root = dom
        .get_by_ref(dom.root_ref())
        .ok_or_else(|| anyhow!("preserved Roblox DOM root is missing"))?;
    let mut refs = Vec::new();
    for child_ref in root.children().iter().copied() {
        push_ref_index(dom, child_ref, &mut refs)?;
    }
    Ok(refs)
}

fn push_ref_index(dom: &WeakDom, referent: Ref, refs: &mut Vec<Ref>) -> Result<()> {
    refs.push(referent);
    let instance = dom
        .get_by_ref(referent)
        .ok_or_else(|| anyhow!("preserved Roblox file is missing an expected instance"))?;
    let children = instance.children().to_vec();
    for child_ref in children {
        push_ref_index(dom, child_ref, refs)?;
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn maybe_apply_instance_json_changes(
    dom: &mut WeakDom,
    input_folder: &Path,
    metadata: &CompileMetadata,
    ref_index: &[Ref],
) -> Result<usize> {
    let instances_path = input_folder.join("instances.json");
    if !instances_path.is_file() {
        return Ok(0);
    }

    if metadata.version == 2 {
        if let Some(expected) = &metadata.instances_fingerprint {
            if file_fingerprint(&instances_path).ok().as_ref() == Some(expected) {
                return Ok(0);
            }
        }
    } else if let Some(baseline) = &metadata.baseline_instances {
        let baseline_path = state_dir(input_folder).join(baseline);
        if baseline_path.is_file() && files_equal(&instances_path, &baseline_path)? {
            return Ok(0);
        }
    }

    apply_instance_json_changes(dom, &instances_path, ref_index)
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let modified = metadata.modified().ok().and_then(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
    });
    Ok(FileFingerprint {
        len: metadata.len(),
        modified_secs: modified.map(|(secs, _)| secs),
        modified_nanos: modified.map(|(_, nanos)| nanos),
    })
}

fn files_equal(left_path: &Path, right_path: &Path) -> Result<bool> {
    let left_metadata = fs::metadata(left_path)
        .with_context(|| format!("failed to read metadata for {}", left_path.display()))?;
    let right_metadata = fs::metadata(right_path)
        .with_context(|| format!("failed to read metadata for {}", right_path.display()))?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }

    let mut left = BufReader::new(
        File::open(left_path).with_context(|| format!("failed to open {}", left_path.display()))?,
    );
    let mut right = BufReader::new(
        File::open(right_path)
            .with_context(|| format!("failed to open {}", right_path.display()))?,
    );
    let mut left_buffer = vec![0u8; 1024 * 1024];
    let mut right_buffer = vec![0u8; 1024 * 1024];
    loop {
        let left_read = left
            .read(&mut left_buffer)
            .with_context(|| format!("failed to read {}", left_path.display()))?;
        let right_read = right
            .read(&mut right_buffer)
            .with_context(|| format!("failed to read {}", right_path.display()))?;
        if left_read != right_read {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
        if left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
    }
}

fn apply_instance_json_changes(
    dom: &mut WeakDom,
    instances_path: &Path,
    ref_index: &[Ref],
) -> Result<usize> {
    let file = File::open(instances_path)
        .with_context(|| format!("failed to read {}", instances_path.display()))?;
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
    let mut applier = InstanceJsonApplier::new(dom, ref_index);
    let root_children = RootInstancesSeed {
        applier: &mut applier,
    }
    .deserialize(&mut deserializer)
    .with_context(|| format!("failed to parse {}", instances_path.display()))?;
    deserializer
        .end()
        .with_context(|| format!("failed to parse {}", instances_path.display()))?;
    let root_ref = applier.dom.root_ref();
    applier.reorder_children(root_ref, &root_children)?;
    applier.finish()?;
    Ok(applier.changed)
}

struct InstanceJsonApplier<'a> {
    dom: &'a mut WeakDom,
    ref_index: &'a [Ref],
    seen: Vec<bool>,
    changed: usize,
}

impl<'a> InstanceJsonApplier<'a> {
    fn new(dom: &'a mut WeakDom, ref_index: &'a [Ref]) -> Self {
        Self {
            dom,
            ref_index,
            seen: vec![false; ref_index.len()],
            changed: 0,
        }
    }

    fn apply_node(
        &mut self,
        id: String,
        name: String,
        class_name: String,
        properties: BTreeMap<String, Value>,
        children: Vec<Ref>,
    ) -> Result<Ref> {
        let index = parse_instance_id(&id)?;
        let referent = *self.ref_index.get(index).ok_or_else(|| {
            anyhow!("instances.json cannot add or delete instances; unknown instance id {id}")
        })?;
        if std::mem::replace(&mut self.seen[index], true) {
            bail!("instances.json contains duplicate instance id {id}");
        }

        let updates = {
            let instance = self
                .dom
                .get_by_ref(referent)
                .ok_or_else(|| anyhow!("original Roblox file is missing instance id {id}"))?;
            if instance.class.as_str() != class_name {
                bail!(
                    "instances.json cannot change class for id {id}: {} -> {}",
                    instance.class,
                    class_name
                );
            }
            validate_and_collect_property_updates(instance, &id, &properties)?
        };

        let instance = self
            .dom
            .get_by_ref_mut(referent)
            .ok_or_else(|| anyhow!("original Roblox file is missing instance id {id}"))?;
        if instance.name != name {
            instance.name = name;
            self.changed += 1;
        }
        for (property_name, value) in updates {
            instance.properties.insert(ustr(&property_name), value);
            self.changed += 1;
        }

        self.reorder_children(referent, &children)?;
        Ok(referent)
    }

    fn reorder_children(&mut self, parent: Ref, desired_children: &[Ref]) -> Result<()> {
        let current_children = self
            .dom
            .get_by_ref(parent)
            .ok_or_else(|| anyhow!("original Roblox file is missing an expected parent"))?
            .children()
            .to_vec();
        if current_children == desired_children {
            return Ok(());
        }
        for child in desired_children {
            self.dom.transfer_within(*child, parent);
        }
        self.changed += 1;
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        if let Some((index, _)) = self.seen.iter().enumerate().find(|(_, seen)| !**seen) {
            bail!(
                "instances.json cannot add or delete instances; missing instance id {}",
                format_instance_id(index)
            );
        }
        Ok(())
    }
}

fn validate_and_collect_property_updates(
    instance: &rbx_dom_weak::Instance,
    id: &str,
    properties: &BTreeMap<String, Value>,
) -> Result<Vec<(String, Variant)>> {
    let mut expected_count = 0usize;
    for (name, original) in &instance.properties {
        if name.as_str() == "Source" {
            continue;
        }
        if variant_to_json_value(original).is_some() {
            expected_count += 1;
            if !properties.contains_key(name.as_str()) {
                bail!(
                    "instances.json cannot add or delete properties for id {id}; missing property {}",
                    name
                );
            }
        }
    }
    if properties.len() != expected_count {
        bail!(
            "instances.json cannot add or delete properties for id {id}; only existing exported properties can be edited"
        );
    }

    let mut updates = Vec::new();
    for (property_name, current_value) in properties {
        if property_name == "Source" {
            bail!("script Source edits must be made in scripts/, not instances.json");
        }
        let property_key = ustr(property_name);
        let original = instance.properties.get(&property_key).ok_or_else(|| {
            anyhow!("original Roblox file is missing property {property_name} for id {id}")
        })?;
        let baseline_value = variant_to_json_value(original).ok_or_else(|| {
            anyhow!("property {property_name} for id {id} is not editable from instances.json")
        })?;
        if current_value != &baseline_value {
            updates.push((
                property_name.clone(),
                json_to_variant(original, current_value, id, property_name)?,
            ));
        }
    }
    Ok(updates)
}

fn parse_instance_id(id: &str) -> Result<usize> {
    let number = id
        .strip_prefix("inst_")
        .ok_or_else(|| {
            anyhow!("instances.json cannot add or delete instances; invalid instance id {id}")
        })?
        .parse::<usize>()
        .with_context(|| {
            format!("instances.json cannot add or delete instances; invalid instance id {id}")
        })?;
    if number == 0 {
        bail!("instances.json cannot add or delete instances; invalid instance id {id}");
    }
    Ok(number - 1)
}

fn format_instance_id(index: usize) -> String {
    format!("inst_{:06}", index + 1)
}

struct RootInstancesSeed<'a, 'dom> {
    applier: &'a mut InstanceJsonApplier<'dom>,
}

impl<'de, 'a, 'dom> DeserializeSeed<'de> for RootInstancesSeed<'a, 'dom> {
    type Value = Vec<Ref>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(ChildrenVisitor {
            applier: self.applier,
        })
    }
}

struct ChildrenVisitor<'a, 'dom> {
    applier: &'a mut InstanceJsonApplier<'dom>,
}

impl<'de, 'a, 'dom> Visitor<'de> for ChildrenVisitor<'a, 'dom> {
    type Value = Vec<Ref>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of instance nodes")
    }

    fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut children = Vec::new();
        while let Some(referent) = seq.next_element_seed(InstanceNodeSeed {
            applier: &mut *self.applier,
        })? {
            children.push(referent);
        }
        Ok(children)
    }
}

struct InstanceNodeSeed<'a, 'dom> {
    applier: &'a mut InstanceJsonApplier<'dom>,
}

impl<'de, 'a, 'dom> DeserializeSeed<'de> for InstanceNodeSeed<'a, 'dom> {
    type Value = Ref;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(InstanceNodeVisitor {
            applier: self.applier,
        })
    }
}

struct InstanceNodeVisitor<'a, 'dom> {
    applier: &'a mut InstanceJsonApplier<'dom>,
}

impl<'de, 'a, 'dom> Visitor<'de> for InstanceNodeVisitor<'a, 'dom> {
    type Value = Ref;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an instance node object")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut id = None;
        let mut name = None;
        let mut class_name = None;
        let mut properties = None;
        let mut children = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => id = Some(map.next_value()?),
                "name" => name = Some(map.next_value()?),
                "class_name" => class_name = Some(map.next_value()?),
                "roblox_path" => {
                    let _: Value = map.next_value()?;
                }
                "properties" => properties = Some(map.next_value()?),
                "children" => {
                    children = Some(map.next_value_seed(RootInstancesSeed {
                        applier: &mut *self.applier,
                    })?);
                }
                _ => {
                    let _: Value = map.next_value()?;
                }
            }
        }

        let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
        let name = name.ok_or_else(|| de::Error::missing_field("name"))?;
        let class_name = class_name.ok_or_else(|| de::Error::missing_field("class_name"))?;
        let properties = properties.ok_or_else(|| de::Error::missing_field("properties"))?;
        let children = children.ok_or_else(|| de::Error::missing_field("children"))?;
        self.applier
            .apply_node(id, name, class_name, properties, children)
            .map_err(de::Error::custom)
    }
}

fn apply_script_changes<F>(
    dom: &mut WeakDom,
    input_folder: &Path,
    metadata: &CompileMetadata,
    ref_index: &[Ref],
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
        let referent = *ref_index
            .get(parse_instance_id(&script.id)?)
            .ok_or_else(|| {
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
