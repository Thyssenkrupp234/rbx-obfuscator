use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Cursor, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use serde::Serialize;
use serde_json::json;
use tempfile::NamedTempFile;

use crate::{
    child_path,
    compile::{CompileOptions, CompileSummary},
    extract::{ExtractOptions, ExtractSummary},
    prometheus_preset_for_level, sanitize_filename, CompileMetadata, CompileScriptEntry,
    LongScriptAction, LongScriptContext, Manifest, ManifestAction, ManifestEntry,
    ObfuscationSummary, Options, ProgressEvent, RobloxFileFormat, ScriptTransformOutcome,
    COMPILE_METADATA_FILE, COMPILE_STATE_DIR, SCRIPT_CLASSES,
};

const FILE_MAGIC_HEADER: &[u8] = b"<roblox!";
const FILE_SIGNATURE: &[u8] = b"\x89\xff\x0d\x0a\x1a\x0a";
const FILE_VERSION: u16 = 0;
const ZSTD_MAGIC_NUMBER: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
const TYPE_STRING: u8 = 0x01;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChunkCompression {
    None,
    Lz4,
    Zstd,
}

#[derive(Clone, Copy, Debug)]
struct ParseOptions {
    read_sources: bool,
    collect_content_refs: bool,
}

#[derive(Debug)]
struct ChunkData {
    name: [u8; 4],
    data: Vec<u8>,
    raw: Vec<u8>,
    compression: ChunkCompression,
}

#[derive(Clone, Debug)]
struct TypeInfo {
    class_name: String,
    referents: Vec<i32>,
}

#[derive(Clone, Debug)]
struct FastNode {
    referent: i32,
    class_name: String,
    name: String,
    parent: Option<usize>,
    children: Vec<usize>,
    source: Option<String>,
}

#[derive(Default)]
struct FastModel {
    nodes: Vec<FastNode>,
    ref_to_index: HashMap<i32, usize>,
    types: HashMap<u32, TypeInfo>,
    roots: Vec<usize>,
    content_refs: Vec<FastContentReference>,
}

#[derive(Clone, Debug)]
struct FastContentReference {
    referent: i32,
    property_name: String,
    value: String,
}

#[derive(Clone, Debug, Serialize)]
struct ContentReferenceOut {
    roblox_path: String,
    class_name: String,
    property_name: String,
    value: String,
}

#[derive(Clone, Debug)]
struct FastScript {
    referent: i32,
    id: String,
    roblox_path: String,
    class_name: String,
    name: String,
    parent_segments: Vec<String>,
    source: String,
    output_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
struct Header {
    num_instances: u32,
}

pub(crate) fn binary_instance_count(path: &Path) -> Result<u32> {
    let mut input = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    read_header(&mut input).map(|header| header.num_instances)
}

pub(crate) fn extract_binary_large<C, F>(
    options: ExtractOptions,
    input_format: RobloxFileFormat,
    mut should_cancel: C,
    mut progress: F,
) -> Result<ExtractSummary>
where
    C: FnMut() -> bool,
    F: FnMut(ProgressEvent),
{
    let started_at = Instant::now();
    let state_dir = options.output_folder.join(COMPILE_STATE_DIR);
    fs::create_dir_all(&state_dir).with_context(|| {
        format!(
            "failed to create compile metadata folder {}",
            state_dir.display()
        )
    })?;

    progress(ProgressEvent::StageStarted {
        stage_index: 1,
        stage_total: 3,
        name: "Parse RBXL/RBXM".to_owned(),
    });
    let mut model = parse_model(
        &options.input,
        ParseOptions {
            read_sources: true,
            collect_content_refs: true,
        },
    )?;
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
    let paths = compute_paths(&model);
    let ids = assign_ids(&model);

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing instances.json".to_owned(),
    });
    write_instances_json(
        &options.output_folder.join("instances.json"),
        &model,
        &paths,
        &ids,
    )?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Exporting scripts".to_owned(),
    });
    let mut scripts = collect_scripts(&mut model, &paths, &ids);
    write_scripts(&options.output_folder, &mut scripts)?;
    let scripts_exported = scripts.len();
    progress(ProgressEvent::ScriptProgress {
        completed: scripts_exported,
        total: scripts.len(),
        current_path: None,
    });

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Exporting GUI JSON".to_owned(),
    });
    let guis_exported = write_guis(&options.output_folder, &model, &paths, &ids)?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing content_refs.json".to_owned(),
    });
    let content_refs_found = write_content_refs(
        &options.output_folder.join("content_refs.json"),
        &model,
        &paths,
    )?;
    if should_cancel() {
        bail!("operation cancelled");
    }

    progress(ProgressEvent::CurrentItem {
        label: "Current thing".to_owned(),
        value: "Writing compile metadata".to_owned(),
    });
    let snapshot_path = PathBuf::from(format!("original.{}", input_format.extension()));
    fs::copy(&options.input, state_dir.join(&snapshot_path)).with_context(|| {
        format!(
            "failed to preserve original Roblox file in {}",
            state_dir.display()
        )
    })?;
    let instances_path = options.output_folder.join("instances.json");
    let metadata = CompileMetadata {
        version: 2,
        input: options
            .input
            .canonicalize()
            .unwrap_or_else(|_| options.input.clone()),
        input_format,
        original_snapshot: snapshot_path,
        baseline_instances: None,
        instances_fingerprint: Some(file_fingerprint(&instances_path)?),
        instance_count: Some(model.nodes.len()),
        scripts: scripts
            .iter()
            .map(|script| CompileScriptEntry {
                id: script.id.clone(),
                roblox_path: script.roblox_path.clone(),
                class_name: script.class_name.clone(),
                source_file: script.output_file.as_ref().map(|path| {
                    path.strip_prefix(&options.output_folder)
                        .unwrap_or(path)
                        .to_path_buf()
                }),
            })
            .collect(),
    };
    write_json(&state_dir.join(COMPILE_METADATA_FILE), &metadata)?;
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
    let warnings = vec![
        "Large binary fast path exported hierarchy, scripts, GUI structure, and content references without full per-instance property JSON to keep memory bounded.".to_owned(),
    ];
    let manifest = json!({
        "input": options.input,
        "output_folder": options.output_folder,
        "timestamp": timestamp_seconds(),
        "tool_version": env!("CARGO_PKG_VERSION"),
        "mode": "extract",
        "input_format": input_format,
        "counts": {
            "total_instances": model.nodes.len(),
            "scripts_found": scripts.len(),
            "scripts_exported": scripts_exported,
            "guis_exported": guis_exported,
            "content_references_found": content_refs_found,
            "warnings_count": warnings.len(),
        },
        "scripts": scripts.iter().map(|script| json!({
            "id": script.id,
            "roblox_path": script.roblox_path,
            "class_name": script.class_name,
            "output_file": script.output_file,
            "source_length": script.source.len(),
            "source_present": true,
            "warning": null,
        })).collect::<Vec<_>>(),
        "warnings": warnings,
        "unsupported_properties": [],
    });
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
        total_instances: model.nodes.len(),
        scripts_found: scripts.len(),
        scripts_exported,
        guis_exported,
        content_refs_found,
        warnings_count: 1,
        duration: started_at.elapsed(),
    })
}

pub(crate) fn try_compile_binary_script_patch(
    options: &CompileOptions,
    output: &Path,
    metadata: &CompileMetadata,
) -> Result<Option<CompileSummary>> {
    if metadata.version != 2 || metadata.input_format.is_xml() {
        return Ok(None);
    }
    if crate::validate_input_format(output)? != metadata.input_format {
        return Ok(None);
    }

    let instances_path = options.input_folder.join("instances.json");
    if let Some(expected) = &metadata.instances_fingerprint {
        if file_fingerprint(&instances_path).ok().as_ref() != Some(expected) {
            return Ok(None);
        }
    }

    let started_at = Instant::now();
    let snapshot = options
        .input_folder
        .join(COMPILE_STATE_DIR)
        .join(&metadata.original_snapshot);
    let model = parse_model(
        &snapshot,
        ParseOptions {
            read_sources: false,
            collect_content_refs: false,
        },
    )?;
    let mut sources_by_ref = HashMap::new();
    for script in &metadata.scripts {
        let Some(source_file) = &script.source_file else {
            continue;
        };
        let index = parse_instance_id(&script.id)?;
        let referent = model
            .nodes
            .get(index)
            .ok_or_else(|| anyhow!("missing instance id {}", script.id))?
            .referent;
        let source_path = options.input_folder.join(source_file);
        let source = fs::read_to_string(&source_path)
            .with_context(|| format!("failed to read script source {}", source_path.display()))?;
        sources_by_ref.insert(referent, source);
    }

    patch_sources(&snapshot, output, &model.types, &sources_by_ref)?;

    Ok(Some(CompileSummary {
        input_folder: options.input_folder.clone(),
        output: output.to_path_buf(),
        scripts_updated: sources_by_ref.len(),
        instances_changed: 0,
        duration: started_at.elapsed(),
    }))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn obfuscate_binary_large<C, S, F>(
    options: &Options,
    output: PathBuf,
    input_format: RobloxFileFormat,
    prometheus_path: PathBuf,
    should_cancel: &mut C,
    script_action: &mut S,
    progress: &mut F,
) -> Result<ObfuscationSummary>
where
    C: FnMut() -> bool,
    S: FnMut(LongScriptContext) -> Option<LongScriptAction>,
    F: FnMut(ProgressEvent),
{
    let started_at = Instant::now();
    let prometheus_preset = prometheus_preset_for_level(options.obfuscation_level);

    progress(ProgressEvent::StageStarted {
        stage_index: 1,
        stage_total: 3,
        name: "Extract scripts from RBXL/RBXM".to_owned(),
    });
    let mut model = parse_model(
        &options.input,
        ParseOptions {
            read_sources: true,
            collect_content_refs: false,
        },
    )?;
    let paths = compute_paths(&model);
    let ids = assign_ids(&model);
    let scripts = collect_scripts(&mut model, &paths, &ids);
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

    let skip_paths: std::collections::HashSet<String> =
        options.skip_paths.iter().cloned().collect();
    let total_scripts = scripts.len();
    let mut manifest_entries = Vec::with_capacity(scripts.len());
    let mut processed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut transformed_sources = HashMap::new();
    let prometheus_temp_dir = if options.dry_run {
        None
    } else {
        Some(crate::create_prometheus_temp_dir()?)
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
    for script in scripts {
        if should_cancel() {
            bail!("operation cancelled");
        }
        progress(ProgressEvent::CurrentItem {
            label: "Current thing".to_owned(),
            value: script.roblox_path.clone(),
        });

        if skip_paths.contains(&script.roblox_path) {
            skipped += 1;
            manifest_entries.push(ManifestEntry {
                path: script.roblox_path,
                class_name: script.class_name,
                action: ManifestAction::Skipped,
                source_bytes: script.source.len(),
                transformed_bytes: None,
                luau_compatibility_applied: false,
                backup_path: None,
                error: None,
            });
            progress(ProgressEvent::ScriptProgress {
                completed: processed + skipped + failed,
                total: total_scripts,
                current_path: None,
            });
            continue;
        }

        if options.dry_run {
            processed += 1;
            manifest_entries.push(ManifestEntry {
                path: script.roblox_path,
                class_name: script.class_name,
                action: ManifestAction::DryRun,
                source_bytes: script.source.len(),
                transformed_bytes: None,
                luau_compatibility_applied: options.strip_types,
                backup_path: None,
                error: None,
            });
            progress(ProgressEvent::ScriptProgress {
                completed: processed + skipped + failed,
                total: total_scripts,
                current_path: None,
            });
            continue;
        }

        let backup_path = if let Some(backup_dir) = &options.backup_dir {
            let backup_path = crate::backup_path_for(backup_dir, &script.roblox_path);
            fs::write(&backup_path, &script.source)
                .with_context(|| format!("failed to write backup {}", backup_path.display()))?;
            Some(backup_path)
        } else {
            None
        };

        let temp_dir = prometheus_temp_dir
            .as_ref()
            .expect("Prometheus temp dir must exist outside dry-run")
            .path();
        let transform = crate::run_prometheus_with_type_fallback(
            &prometheus_path,
            &script.source,
            options.obfuscation_level,
            temp_dir,
            options.strip_types,
            &script.roblox_path,
            options.verbose,
            progress,
            script_action,
        );

        match transform {
            Ok(ScriptTransformOutcome::Transformed {
                source,
                luau_compatibility_applied,
            }) => {
                processed += 1;
                transformed_sources.insert(script.referent, source.clone());
                manifest_entries.push(ManifestEntry {
                    path: script.roblox_path.clone(),
                    class_name: script.class_name,
                    action: ManifestAction::Processed,
                    source_bytes: script.source.len(),
                    transformed_bytes: Some(source.len()),
                    luau_compatibility_applied,
                    backup_path,
                    error: None,
                });
                if luau_compatibility_applied {
                    progress(ProgressEvent::CompatibilityNote {
                        message: "Luau compatibility preprocessing applied".to_owned(),
                    });
                }
            }
            Ok(ScriptTransformOutcome::Skipped { reason }) => {
                skipped += 1;
                manifest_entries.push(ManifestEntry {
                    path: script.roblox_path,
                    class_name: script.class_name,
                    action: ManifestAction::Skipped,
                    source_bytes: script.source.len(),
                    transformed_bytes: None,
                    luau_compatibility_applied: false,
                    backup_path,
                    error: Some(reason),
                });
            }
            Err(failure) => {
                failed += 1;
                let error = format!("{:#}", failure.error);
                let summary = crate::first_error_line(&error).to_owned();
                progress(ProgressEvent::Warning {
                    message: format!(
                        "Failed {}; leaving source unobfuscated: {summary}",
                        script.roblox_path
                    ),
                });
                manifest_entries.push(ManifestEntry {
                    path: script.roblox_path,
                    class_name: script.class_name,
                    action: ManifestAction::Failed,
                    source_bytes: script.source.len(),
                    transformed_bytes: None,
                    luau_compatibility_applied: failure.luau_compatibility_applied,
                    backup_path,
                    error: Some(error),
                });
            }
        }
        progress(ProgressEvent::ScriptProgress {
            completed: processed + skipped + failed,
            total: total_scripts,
            current_path: None,
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
        crate::write_manifest(manifest_path, &manifest)?;
    }

    if !options.dry_run {
        progress(ProgressEvent::StageStarted {
            stage_index: 3,
            stage_total: 3,
            name: "Compile output file".to_owned(),
        });
        patch_sources(&options.input, &output, &model.types, &transformed_sources)?;
        progress(ProgressEvent::StageCompleted {
            stage_index: 3,
            stage_total: 3,
            name: "Compile output file".to_owned(),
        });
    }
    progress(ProgressEvent::Finished);

    Ok(ObfuscationSummary {
        input: options.input.clone(),
        output,
        obfuscation_level: options.obfuscation_level,
        prometheus_preset,
        scripts_found: processed + skipped + failed,
        scripts_processed: processed,
        scripts_skipped: skipped,
        scripts_failed: failed,
        dry_run: options.dry_run,
        backup_created: options.backup_dir.is_some(),
        duration: started_at.elapsed(),
    })
}

fn parse_model(path: &Path, options: ParseOptions) -> Result<FastModel> {
    let mut input = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    read_header(&mut input)?;
    let mut model = FastModel::default();

    loop {
        let Some(chunk) = read_chunk(&mut input)? else {
            break;
        };
        match &chunk.name {
            b"INST" => decode_inst_chunk(&chunk.data, &mut model)?,
            b"PROP" => decode_prop_chunk(&chunk.data, &mut model, options)?,
            b"PRNT" => decode_prnt_chunk(&chunk.data, &mut model)?,
            b"END\0" => break,
            _ => {}
        }
    }

    Ok(model)
}

fn read_header(reader: &mut dyn Read) -> Result<Header> {
    let mut magic = [0; 8];
    reader.read_exact(&mut magic)?;
    if magic != FILE_MAGIC_HEADER {
        bail!("bad Roblox binary header");
    }
    let mut signature = [0; 6];
    reader.read_exact(&mut signature)?;
    if signature != FILE_SIGNATURE {
        bail!("bad Roblox binary signature");
    }
    let version = reader.read_u16::<LittleEndian>()?;
    if version != FILE_VERSION {
        bail!("unsupported Roblox binary version {version}");
    }
    let _num_types = reader.read_u32::<LittleEndian>()?;
    let num_instances = reader.read_u32::<LittleEndian>()?;
    let mut reserved = [0; 8];
    reader.read_exact(&mut reserved)?;
    if reserved != [0; 8] {
        bail!("bad Roblox binary reserved header");
    }
    Ok(Header { num_instances })
}

fn read_chunk(reader: &mut dyn Read) -> Result<Option<ChunkData>> {
    let mut name = [0u8; 4];
    match reader.read_exact(&mut name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("failed to read chunk name"),
    }
    let compressed_len = reader.read_u32::<LittleEndian>()?;
    let uncompressed_len = reader.read_u32::<LittleEndian>()?;
    let reserved = reader.read_u32::<LittleEndian>()?;
    if reserved != 0 {
        bail!("Roblox binary chunk has non-zero reserved field");
    }
    let payload_len = if compressed_len == 0 {
        uncompressed_len
    } else {
        compressed_len
    } as usize;
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload)?;

    let mut raw = Vec::with_capacity(16 + payload.len());
    raw.extend_from_slice(&name);
    raw.write_u32::<LittleEndian>(compressed_len)?;
    raw.write_u32::<LittleEndian>(uncompressed_len)?;
    raw.write_u32::<LittleEndian>(reserved)?;
    raw.extend_from_slice(&payload);

    let (data, compression) = if compressed_len == 0 {
        (payload, ChunkCompression::None)
    } else if payload.starts_with(ZSTD_MAGIC_NUMBER) {
        (
            zstd::bulk::decompress(&payload, uncompressed_len as usize)
                .context("failed to decompress ZSTD chunk")?,
            ChunkCompression::Zstd,
        )
    } else {
        (
            lz4_flex::block::decompress(&payload, uncompressed_len as usize)
                .map_err(anyhow::Error::msg)
                .context("failed to decompress LZ4 chunk")?,
            ChunkCompression::Lz4,
        )
    };

    Ok(Some(ChunkData {
        name,
        data,
        raw,
        compression,
    }))
}

fn decode_inst_chunk(data: &[u8], model: &mut FastModel) -> Result<()> {
    let mut reader = Cursor::new(data);
    let type_id = reader.read_u32::<LittleEndian>()?;
    let class_name = read_string(&mut reader)?;
    let _object_format = reader.read_u8()?;
    let count = reader.read_u32::<LittleEndian>()? as usize;
    let referents = read_referent_array(&mut reader, count)?;
    for referent in &referents {
        let index = model.nodes.len();
        model.ref_to_index.insert(*referent, index);
        model.nodes.push(FastNode {
            referent: *referent,
            class_name: class_name.clone(),
            name: class_name.clone(),
            parent: None,
            children: Vec::new(),
            source: None,
        });
    }
    model.types.insert(
        type_id,
        TypeInfo {
            class_name,
            referents,
        },
    );
    Ok(())
}

fn decode_prop_chunk(data: &[u8], model: &mut FastModel, options: ParseOptions) -> Result<()> {
    let mut reader = Cursor::new(data);
    let type_id = reader.read_u32::<LittleEndian>()?;
    let prop_name = read_string(&mut reader)?;
    let Some(type_info) = model.types.get(&type_id).cloned() else {
        return Ok(());
    };
    let Ok(prop_type) = reader.read_u8() else {
        return Ok(());
    };
    if prop_type != TYPE_STRING {
        return Ok(());
    }

    let is_name = prop_name == "Name";
    let is_script_source =
        options.read_sources && prop_name == "Source" && is_script_class(&type_info.class_name);
    let property_may_be_asset_ref =
        options.collect_content_refs && property_looks_like_asset_reference(&prop_name);
    let needs_value =
        is_name || is_script_source || property_may_be_asset_ref || options.collect_content_refs;

    for referent in &type_info.referents {
        if !needs_value {
            skip_binary_string(&mut reader)?;
            continue;
        }
        let bytes = read_binary_string(&mut reader)?;
        let value = String::from_utf8_lossy(&bytes).into_owned();
        let index = model.ref_to_index[referent];
        if is_name {
            model.nodes[index].name = value;
        } else if is_script_source {
            model.nodes[index].source = Some(value);
        } else if options.collect_content_refs
            && (contains_asset_reference(&value) || property_may_be_asset_ref)
        {
            model.content_refs.push(FastContentReference {
                referent: *referent,
                property_name: prop_name.clone(),
                value,
            });
        }
    }
    Ok(())
}

fn decode_prnt_chunk(data: &[u8], model: &mut FastModel) -> Result<()> {
    let mut reader = Cursor::new(data);
    let version = reader.read_u8()?;
    if version != 0 {
        bail!("unsupported PRNT chunk version {version}");
    }
    let count = reader.read_u32::<LittleEndian>()? as usize;
    let subjects = read_referent_array(&mut reader, count)?;
    let parents = read_referent_array(&mut reader, count)?;
    for (subject, parent_ref) in subjects.into_iter().zip(parents) {
        let child_index = model.ref_to_index[&subject];
        if parent_ref == -1 {
            model.roots.push(child_index);
        } else if let Some(&parent_index) = model.ref_to_index.get(&parent_ref) {
            model.nodes[child_index].parent = Some(parent_index);
            model.nodes[parent_index].children.push(child_index);
        }
    }
    Ok(())
}

fn compute_paths(model: &FastModel) -> Vec<String> {
    let mut paths = vec![String::new(); model.nodes.len()];
    for &root in &model.roots {
        fill_paths(model, root, "game", &mut paths);
    }
    paths
}

fn fill_paths(model: &FastModel, index: usize, parent_path: &str, paths: &mut [String]) {
    let path = child_path(parent_path, &model.nodes[index].name);
    paths[index] = path.clone();
    for &child in &model.nodes[index].children {
        fill_paths(model, child, &path, paths);
    }
}

fn assign_ids(model: &FastModel) -> Vec<String> {
    let mut ids = vec![String::new(); model.nodes.len()];
    let mut next = 0usize;
    for &root in &model.roots {
        assign_id_for_node(model, root, &mut next, &mut ids);
    }
    ids
}

fn assign_id_for_node(model: &FastModel, index: usize, next: &mut usize, ids: &mut [String]) {
    *next += 1;
    ids[index] = format!("inst_{:06}", *next);
    for &child in &model.nodes[index].children {
        assign_id_for_node(model, child, next, ids);
    }
}

fn write_instances_json(
    path: &Path,
    model: &FastModel,
    paths: &[String],
    ids: &[String],
) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    writer.write_all(b"[\n")?;
    for (position, &root) in model.roots.iter().enumerate() {
        if position > 0 {
            writer.write_all(b",\n")?;
        }
        write_instance_node(&mut writer, model, root, paths, ids, 1)?;
    }
    writer.write_all(b"\n]\n")?;
    writer.flush()?;
    Ok(())
}

fn write_instance_node(
    writer: &mut dyn Write,
    model: &FastModel,
    index: usize,
    paths: &[String],
    ids: &[String],
    indent: usize,
) -> Result<()> {
    write_indent(writer, indent)?;
    let node = &model.nodes[index];
    writer.write_all(b"{\"id\":")?;
    serde_json::to_writer(&mut *writer, &ids[index])?;
    writer.write_all(b",\"name\":")?;
    serde_json::to_writer(&mut *writer, &node.name)?;
    writer.write_all(b",\"class_name\":")?;
    serde_json::to_writer(&mut *writer, &node.class_name)?;
    writer.write_all(b",\"roblox_path\":")?;
    serde_json::to_writer(&mut *writer, &paths[index])?;
    writer.write_all(b",\"properties\":{},\"children\":[")?;
    if !node.children.is_empty() {
        writer.write_all(b"\n")?;
    }
    for (position, &child) in node.children.iter().enumerate() {
        if position > 0 {
            writer.write_all(b",\n")?;
        }
        write_instance_node(writer, model, child, paths, ids, indent + 1)?;
    }
    if !node.children.is_empty() {
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

fn collect_scripts(model: &mut FastModel, paths: &[String], ids: &[String]) -> Vec<FastScript> {
    let mut scripts = Vec::new();
    for index in 0..model.nodes.len() {
        let node = &model.nodes[index];
        if !is_script_class(&node.class_name) {
            continue;
        }
        let Some(source) = node.source.clone() else {
            continue;
        };
        scripts.push(FastScript {
            referent: node.referent,
            id: ids[index].clone(),
            roblox_path: paths[index].clone(),
            class_name: node.class_name.clone(),
            name: node.name.clone(),
            parent_segments: parent_segments(model, index),
            source,
            output_file: None,
        });
    }
    scripts
}

fn parent_segments(model: &FastModel, index: usize) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = model.nodes[index].parent;
    while let Some(parent) = current {
        segments.push(model.nodes[parent].name.clone());
        current = model.nodes[parent].parent;
    }
    segments.reverse();
    segments
}

fn write_scripts(output_folder: &Path, scripts: &mut [FastScript]) -> Result<()> {
    let script_root = output_folder.join("scripts");
    let mut used_paths = std::collections::HashSet::new();
    for script in scripts {
        let mut directory = script_root.clone();
        for segment in &script.parent_segments {
            directory.push(sanitize_filename(segment));
        }
        fs::create_dir_all(&directory)?;
        let path = unique_path(
            &directory.join(format!(
                "{}{}",
                sanitize_filename(&script.name),
                script_extension(&script.class_name)
            )),
            &mut used_paths,
        );
        fs::write(&path, &script.source)?;
        script.output_file = Some(path);
    }
    Ok(())
}

fn write_guis(
    output_folder: &Path,
    model: &FastModel,
    paths: &[String],
    ids: &[String],
) -> Result<usize> {
    let gui_root = output_folder.join("guis");
    fs::create_dir_all(&gui_root)?;
    let mut used_paths = std::collections::HashSet::new();
    let mut count = 0usize;
    for (index, node) in model.nodes.iter().enumerate() {
        if !matches!(
            node.class_name.as_str(),
            "ScreenGui" | "BillboardGui" | "SurfaceGui"
        ) {
            continue;
        }
        let path = unique_path(
            &gui_root.join(format!(
                "{}.{}.json",
                sanitize_filename(&node.name),
                sanitize_filename(&node.class_name)
            )),
            &mut used_paths,
        );
        let mut writer = BufWriter::new(File::create(path)?);
        write_gui_node(&mut writer, model, index, paths, ids, 0)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        count += 1;
    }
    Ok(count)
}

fn write_gui_node(
    writer: &mut dyn Write,
    model: &FastModel,
    index: usize,
    paths: &[String],
    ids: &[String],
    indent: usize,
) -> Result<()> {
    let node = &model.nodes[index];
    write_indent(writer, indent)?;
    writer.write_all(b"{\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"id\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &ids[index])?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"name\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &node.name)?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"class_name\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &node.class_name)?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"roblox_path\": ")?;
    serde_json::to_writer_pretty(&mut *writer, &paths[index])?;
    writer.write_all(b",\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"properties\": {},\n")?;
    write_indent(writer, indent + 1)?;
    writer.write_all(b"\"children\": [")?;
    if !node.children.is_empty() {
        writer.write_all(b"\n")?;
    }
    for (position, &child) in node.children.iter().enumerate() {
        if position > 0 {
            writer.write_all(b",\n")?;
        }
        write_gui_node(writer, model, child, paths, ids, indent + 2)?;
    }
    if !node.children.is_empty() {
        writer.write_all(b"\n")?;
        write_indent(writer, indent + 1)?;
    }
    writer.write_all(b"]\n")?;
    write_indent(writer, indent)?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_content_refs(path: &Path, model: &FastModel, paths: &[String]) -> Result<usize> {
    let mut writer = BufWriter::new(File::create(path)?);
    writer.write_all(b"[\n")?;
    let mut count = 0usize;
    for reference in &model.content_refs {
        let Some(&index) = model.ref_to_index.get(&reference.referent) else {
            continue;
        };
        if count > 0 {
            writer.write_all(b",\n")?;
        }
        let out = ContentReferenceOut {
            roblox_path: paths[index].clone(),
            class_name: model.nodes[index].class_name.clone(),
            property_name: reference.property_name.clone(),
            value: reference.value.clone(),
        };
        writer.write_all(b"  ")?;
        serde_json::to_writer(&mut writer, &out)?;
        count += 1;
    }
    writer.write_all(b"\n]\n")?;
    writer.flush()?;
    Ok(count)
}

fn patch_sources(
    snapshot: &Path,
    output: &Path,
    types: &HashMap<u32, TypeInfo>,
    sources_by_ref: &HashMap<i32, String>,
) -> Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    let mut input = BufReader::new(File::open(snapshot)?);
    let mut header = [0u8; 32];
    input.read_exact(&mut header)?;
    temp.write_all(&header)?;

    loop {
        let Some(chunk) = read_chunk(&mut input)? else {
            break;
        };
        if chunk.name == *b"PROP" {
            if let Some(data) = rewrite_source_chunk(&chunk.data, types, sources_by_ref)? {
                write_chunk(temp.as_file_mut(), &chunk.name, &data, chunk.compression)?;
            } else {
                temp.write_all(&chunk.raw)?;
            }
        } else {
            temp.write_all(&chunk.raw)?;
        }
        if chunk.name == *b"END\0" {
            break;
        }
    }
    temp.flush()?;
    temp.persist(output)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to move compiled file into {}", output.display()))?;
    Ok(())
}

fn rewrite_source_chunk(
    data: &[u8],
    types: &HashMap<u32, TypeInfo>,
    sources_by_ref: &HashMap<i32, String>,
) -> Result<Option<Vec<u8>>> {
    let mut reader = Cursor::new(data);
    let type_id = reader.read_u32::<LittleEndian>()?;
    let prop_name = read_string(&mut reader)?;
    if prop_name != "Source" {
        return Ok(None);
    }
    let prop_type = reader.read_u8()?;
    if prop_type != TYPE_STRING {
        return Ok(None);
    }
    let Some(type_info) = types.get(&type_id) else {
        return Ok(None);
    };
    if !type_info
        .referents
        .iter()
        .any(|referent| sources_by_ref.contains_key(referent))
    {
        return Ok(None);
    }

    let mut output = Vec::new();
    output.write_u32::<LittleEndian>(type_id)?;
    write_string(&mut output, &prop_name)?;
    output.write_u8(TYPE_STRING)?;
    let mut changed = false;
    for referent in &type_info.referents {
        let original = read_binary_string(&mut reader)?;
        if let Some(source) = sources_by_ref.get(referent) {
            changed |= source.as_bytes() != original.as_slice();
            write_binary_string(&mut output, source.as_bytes())?;
        } else {
            write_binary_string(&mut output, &original)?;
        }
    }
    Ok(changed.then_some(output))
}

fn write_chunk(
    writer: &mut dyn Write,
    name: &[u8; 4],
    data: &[u8],
    compression: ChunkCompression,
) -> Result<()> {
    writer.write_all(name)?;
    match compression {
        ChunkCompression::None => {
            writer.write_u32::<LittleEndian>(0)?;
            writer.write_u32::<LittleEndian>(data.len() as u32)?;
            writer.write_u32::<LittleEndian>(0)?;
            writer.write_all(data)?;
        }
        ChunkCompression::Lz4 => {
            let compressed = lz4_flex::block::compress(data);
            writer.write_u32::<LittleEndian>(compressed.len() as u32)?;
            writer.write_u32::<LittleEndian>(data.len() as u32)?;
            writer.write_u32::<LittleEndian>(0)?;
            writer.write_all(&compressed)?;
        }
        ChunkCompression::Zstd => {
            let compressed = zstd::bulk::compress(data, crate::ZSTD_ROBLOX_COMPAT_LEVEL)?;
            writer.write_u32::<LittleEndian>(compressed.len() as u32)?;
            writer.write_u32::<LittleEndian>(data.len() as u32)?;
            writer.write_u32::<LittleEndian>(0)?;
            writer.write_all(&compressed)?;
        }
    }
    Ok(())
}

fn read_string(reader: &mut Cursor<&[u8]>) -> Result<String> {
    let bytes = read_binary_string(reader)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_binary_string(reader: &mut Cursor<&[u8]>) -> Result<Vec<u8>> {
    let len = reader.read_u32::<LittleEndian>()? as usize;
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn skip_binary_string(reader: &mut Cursor<&[u8]>) -> Result<()> {
    let len = reader.read_u32::<LittleEndian>()? as u64;
    let next = reader
        .position()
        .checked_add(len)
        .ok_or_else(|| anyhow!("binary string length overflow"))?;
    if next > reader.get_ref().len() as u64 {
        bail!("binary string length exceeds chunk payload");
    }
    reader.set_position(next);
    Ok(())
}

fn write_string(writer: &mut Vec<u8>, value: &str) -> Result<()> {
    write_binary_string(writer, value.as_bytes())
}

fn write_binary_string(writer: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    writer.write_u32::<LittleEndian>(value.len() as u32)?;
    writer.write_all(value)?;
    Ok(())
}

fn read_referent_array(reader: &mut Cursor<&[u8]>, len: usize) -> Result<Vec<i32>> {
    let mut bytes = vec![0u8; len * 4];
    reader.read_exact(&mut bytes)?;
    let mut output = Vec::with_capacity(len);
    let mut last = 0i32;
    for index in 0..len {
        let mut item = [0u8; 4];
        for byte_index in 0..4 {
            item[byte_index] = bytes[index + len * byte_index];
        }
        let delta = untransform_i32(i32::from_be_bytes(item));
        let referent = last + delta;
        last = referent;
        output.push(referent);
    }
    Ok(output)
}

fn untransform_i32(value: i32) -> i32 {
    ((value as u32) >> 1) as i32 ^ -(value & 1)
}

fn parse_instance_id(id: &str) -> Result<usize> {
    let number = id
        .strip_prefix("inst_")
        .ok_or_else(|| anyhow!("invalid instance id {id}"))?
        .parse::<usize>()?;
    Ok(number - 1)
}

fn unique_path(path: &Path, used_paths: &mut std::collections::HashSet<PathBuf>) -> PathBuf {
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

fn script_extension(class_name: &str) -> &'static str {
    match class_name {
        "LocalScript" => ".client.luau",
        "ModuleScript" => ".module.luau",
        _ => ".server.luau",
    }
}

fn is_script_class(class_name: &str) -> bool {
    SCRIPT_CLASSES.contains(&class_name)
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

fn file_fingerprint(path: &Path) -> Result<crate::FileFingerprint> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified().ok().and_then(|time| {
        time.duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
    });
    Ok(crate::FileFingerprint {
        len: metadata.len(),
        modified_secs: modified.map(|(secs, _)| secs),
        modified_nanos: modified.map(|(_, nanos)| nanos),
    })
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}
