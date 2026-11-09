//! Render the `auto-embedding-workers.yaml` so the embedding-runner
//! settings (model id, optional tokenizer id, dtype, device, max
//! sequence length) reflect the user's [`EmbeddingRuntime`].
//!
//! The bundled `auto-embedding-workers.yaml` is the source of truth. Only
//! the MultimodalEmbeddingRunner settings controlled by the user's
//! `EmbeddingRuntime` are overlaid, so upstream worker names, workflow
//! references, and newly-added workers remain intact.
//!
//! Why a renderer and not pure `expand_env`: the `tokenizer_model_id`
//! line must be **dropped entirely** when the runtime has no tokenizer
//! override (so the embedding runner falls back to its model-id-driven
//! default). YAML `expand_env` cannot drop a key — it can only substitute
//! a value — so the file is staged into `<data>/workers/staged/` at
//! sidecar startup with the line either present or absent, and the
//! sidecar's `MEMORY_WORKERS_YAML` env is pointed at the staged copy.

use std::path::{Path, PathBuf};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};

use super::embedding_settings::EmbeddingRuntime;

/// File name used for the staged YAML. Matches the committed bundle's
/// name so `MEMORY_WORKERS_YAML` semantics are identical on both paths.
pub const STAGED_AUTO_EMBEDDING_WORKERS_YAML: &str = "auto-embedding-workers.yaml";
pub const EMBEDDING_RUNNER_NAME: &str = "MultimodalEmbeddingRunner";
const ENV_PLACEHOLDER_PREFIX: &str = "LOOKBACK_YAML_ENV_PLACEHOLDER_";

/// 10 MiB upper bound on a single image payload. Kept regardless of
/// multimodality so a text-only swap doesn't need a YAML restage when
/// the user later switches back to a multimodal preset. The runner
/// silently ignores this when the model can't embed images.
const EMBEDDING_IMAGE_SIZE_LIMIT_BYTES: u32 = 10 * 1024 * 1024;

/// Overlay the runtime-owned runner settings on the bundled worker YAML.
/// Unrecognized keys and all other workers are retained from the template;
/// environment placeholders are restored for jobworkerp's pre-parse expansion.
pub fn render_auto_embedding_workers_yaml(
    template: &str,
    runtime: &EmbeddingRuntime,
) -> AppResult<String> {
    let (parseable_template, placeholders) = protect_unquoted_env_placeholders(template);
    let mut document = parse_embedding_workers_template(&parseable_template)?;
    overlay_runtime_settings(&mut document, runtime)?;
    let mut rendered = serde_yaml::to_string(&document).map_err(|error| {
        AppError::Config(format!("render bundled embedding workers YAML: {error}"))
    })?;
    for (placeholder, original) in placeholders {
        rendered = rendered.replace(&placeholder, &original);
    }
    Ok(rendered)
}

/// Render only the embedding worker, under the name Memories reported, for a
/// jobworkerp that Memories does not register it on. The other workers call
/// back into Memories and the bundled name needs Memories' space suffix, so
/// neither can be resolved here.
pub fn render_query_embedding_worker_yaml(
    template: &str,
    runtime: &EmbeddingRuntime,
    worker_name: &str,
) -> AppResult<String> {
    if worker_name.trim().is_empty() {
        return Err(AppError::Config(
            "query embedding worker name must not be empty".into(),
        ));
    }
    let (parseable_template, _) = protect_unquoted_env_placeholders(template);
    let mut document = parse_embedding_workers_template(&parseable_template)?;
    let worker = overlay_runtime_settings(&mut document, runtime)?;
    let mut worker = worker.clone();
    worker
        .as_mapping_mut()
        .ok_or_else(|| {
            AppError::Config("MultimodalEmbeddingRunner worker is not a mapping".into())
        })?
        .insert(
            yaml_key("name"),
            serde_yaml::Value::String(worker_name.to_string()),
        );

    let mut query_document = serde_yaml::Mapping::new();
    if let Some(defaults) = document.get("defaults") {
        query_document.insert(yaml_key("defaults"), defaults.clone());
    }
    query_document.insert(
        yaml_key("workers"),
        serde_yaml::Value::Sequence(vec![worker]),
    );
    let rendered = serde_yaml::to_string(&query_document).map_err(|error| {
        AppError::Config(format!("render query embedding worker YAML: {error}"))
    })?;
    if rendered.contains(ENV_PLACEHOLDER_PREFIX) {
        return Err(AppError::Config(
            "query embedding worker depends on an environment placeholder".into(),
        ));
    }
    Ok(rendered)
}

fn parse_embedding_workers_template(parseable_template: &str) -> AppResult<serde_yaml::Value> {
    serde_yaml::from_str(parseable_template)
        .map_err(|error| AppError::Config(format!("parse bundled embedding workers YAML: {error}")))
}

/// Apply the runtime-owned runner settings to the single embedding worker
/// and return it.
fn overlay_runtime_settings<'a>(
    document: &'a mut serde_yaml::Value,
    runtime: &EmbeddingRuntime,
) -> AppResult<&'a serde_yaml::Value> {
    let workers = document
        .get_mut("workers")
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| {
            AppError::Config("bundled embedding workers YAML has no workers list".into())
        })?;
    let mut matching_workers = workers.iter_mut().filter(|worker| {
        worker.get("runner").and_then(serde_yaml::Value::as_str) == Some(EMBEDDING_RUNNER_NAME)
    });
    let worker = matching_workers.next().ok_or_else(|| {
        AppError::Config("bundled embedding workers YAML has no MultimodalEmbeddingRunner".into())
    })?;
    if matching_workers.next().is_some() {
        return Err(AppError::Config(
            "bundled embedding workers YAML has multiple MultimodalEmbeddingRunner workers".into(),
        ));
    }
    let settings = worker
        .get_mut("settings")
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or_else(|| {
            AppError::Config("MultimodalEmbeddingRunner worker has no settings mapping".into())
        })?;

    insert_string(settings, "model_id", &runtime.model_id);
    set_optional_string(
        settings,
        "tokenizer_model_id",
        runtime.tokenizer_id.as_deref(),
    );
    insert_string(settings, "device", embedding_device());
    insert_string(settings, "dtype", &runtime.dtype);
    set_optional_string(
        settings,
        "onnx_model_file",
        runtime.onnx_model_file.as_deref(),
    );
    set_optional_string(settings, "onnx_pooling", runtime.onnx_pooling.as_deref());
    settings.insert(
        yaml_key("max_sequence_length"),
        serde_yaml::Value::Number(runtime.max_sequence_length.into()),
    );
    settings.insert(
        yaml_key("image_size_limit_bytes"),
        serde_yaml::Value::Number(EMBEDDING_IMAGE_SIZE_LIMIT_BYTES.into()),
    );

    let chunking_config = settings
        .entry(yaml_key("chunking_config"))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
        .as_mapping_mut()
        .ok_or_else(|| {
            AppError::Config("MultimodalEmbeddingRunner chunking_config is not a mapping".into())
        })?;
    chunking_config.insert(
        yaml_key("max_chunk_tokens"),
        serde_yaml::Value::Number(runtime.max_sequence_length.into()),
    );
    chunking_config.insert(
        yaml_key("min_chunk_tokens"),
        serde_yaml::Value::Number(0.into()),
    );
    Ok(worker)
}

fn yaml_key(key: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(key.to_string())
}

fn insert_string(settings: &mut serde_yaml::Mapping, key: &str, value: &str) {
    settings.insert(yaml_key(key), serde_yaml::Value::String(value.to_string()));
}

fn set_optional_string(settings: &mut serde_yaml::Mapping, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        insert_string(settings, key, value);
    } else {
        settings.remove(yaml_key(key));
    }
}

/// `serde_yaml` rejects jobworkerp's unquoted `%{ENV_VAR}` scalar syntax.
/// Replace only unquoted placeholders while parsing, then restore them in
/// the serialized YAML so jobworkerp still expands native numeric/boolean
/// settings before its own YAML parse.
fn protect_unquoted_env_placeholders(template: &str) -> (String, Vec<(String, String)>) {
    let bytes = template.as_bytes();
    let mut output = String::with_capacity(template.len());
    let mut placeholders = Vec::new();
    let mut index = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        let character = template[index..]
            .chars()
            .next()
            .expect("index is within template");
        if !character.is_ascii() {
            output.push(character);
            index += character.len_utf8();
            escaped = false;
            continue;
        }
        if !in_single_quote && !in_double_quote && byte == b'#' {
            let end = template[index..]
                .find('\n')
                .map_or(template.len(), |relative| index + relative);
            output.push_str(&template[index..end]);
            index = end;
            continue;
        }
        if in_double_quote && byte == b'\\' && !escaped {
            escaped = true;
            output.push('\u{005c}');
            index += 1;
            continue;
        }
        if byte == b'\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        } else if byte == b'"' && !in_single_quote && !escaped {
            in_double_quote = !in_double_quote;
        }
        if !in_single_quote
            && !in_double_quote
            && template[index..].starts_with("%{")
            && let Some(relative_end) = template[index + 2..].find('}')
        {
            let end = index + 2 + relative_end + 1;
            let original = &template[index..end];
            let placeholder = format!("{ENV_PLACEHOLDER_PREFIX}{}", placeholders.len());
            output.push_str(&placeholder);
            placeholders.push((placeholder, original.to_string()));
            index = end;
            escaped = false;
            continue;
        }
        output.push(character);
        index += 1;
        escaped = false;
    }
    (output, placeholders)
}

// The mm-embedding-runner crate is built with one backend (`metal`, `cuda`, or
// CPU-only) and refuses to register under any other `device` value
// (`UnsupportedDevice`), so the legal value is fixed by the staged plugin
// binary, not user-selectable.
fn embedding_device() -> &'static str {
    embedding_device_for(std::env::consts::OS, option_env!("LOOKBACK_EMBEDDING_GPU"))
}

fn embedding_device_for(target_os: &str, packaged_gpu: Option<&str>) -> &'static str {
    match packaged_gpu
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("metal") => "Metal",
        Some("cuda") => "CUDA",
        Some("cpu") => "CPU",
        _ if target_os == "macos" => "Metal",
        _ => "CUDA",
    }
}

pub fn read_bundled_embedding_workers_template(bundled_workflows_dir: &Path) -> AppResult<String> {
    let source = bundled_workflows_dir.join(STAGED_AUTO_EMBEDDING_WORKERS_YAML);
    std::fs::read_to_string(&source).map_err(|error| {
        AppError::Config(format!(
            "read bundled embedding workers YAML {}: {error}",
            source.display()
        ))
    })
}

/// Write the rendered YAML into the staged workers dir and return the
/// path. The companion `auto-embedding.yaml` is copied alongside so
/// `$file: auto-embedding.yaml` resolves against the same directory.
///
/// Returns `Err` when either bundled YAML is unavailable or cannot be
/// rendered. Staging a hand-written fallback would silently discard the
/// release's worker contract and the user's selected embedding settings.
pub fn stage_auto_embedding_workers_yaml(
    runtime: &EmbeddingRuntime,
    data: &DataPaths,
    bundled_workflows_dir: Option<&Path>,
) -> AppResult<PathBuf> {
    let bundle = bundled_workflows_dir.ok_or_else(|| {
        AppError::Config(
            "stage_auto_embedding_workers_yaml: no bundled workflows dir; cannot resolve companion `auto-embedding.yaml`"
                .to_string(),
        )
    })?;
    let companion_src = bundle.join("auto-embedding.yaml");
    if !companion_src.exists() {
        return Err(AppError::Config(format!(
            "stage_auto_embedding_workers_yaml: companion `auto-embedding.yaml` not found at {}",
            companion_src.display()
        )));
    }
    let worker_template = read_bundled_embedding_workers_template(bundle)?;

    let staged_dir = data.staged_workers_dir();
    std::fs::create_dir_all(&staged_dir).map_err(|e| {
        AppError::Config(format!(
            "create staged workers dir {}: {e}",
            staged_dir.display()
        ))
    })?;

    // Stage the companion FIRST so a partial failure (companion copy
    // erroring after the worker YAML is written) cannot leave behind a
    // stale `auto-embedding-workers.yaml` pointing at a missing `$file`.
    // `std::fs::copy` is idempotent on this tiny YAML; not worth a
    // mtime-skip helper.
    let companion_dst = staged_dir.join("auto-embedding.yaml");
    std::fs::copy(&companion_src, &companion_dst).map_err(|e| {
        AppError::Config(format!(
            "copy auto-embedding.yaml {} → {}: {e}",
            companion_src.display(),
            companion_dst.display()
        ))
    })?;

    let target = staged_dir.join(STAGED_AUTO_EMBEDDING_WORKERS_YAML);
    let body = render_auto_embedding_workers_yaml(&worker_template, runtime)?;
    std::fs::write(&target, body)
        .map_err(|e| AppError::Config(format!("write staged yaml {}: {e}", target.display())))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::embedding_presets;
    use crate::commands::embedding_settings::{EmbeddingSettings, resolve_embedding_runtime};

    const BUNDLED_WORKERS_TEMPLATE: &str =
        include_str!("../../../workers/workflows/auto-embedding-workers.yaml");

    fn default_runtime() -> EmbeddingRuntime {
        resolve_embedding_runtime(&Default::default())
    }

    fn render(runtime: &EmbeddingRuntime) -> String {
        render_auto_embedding_workers_yaml(BUNDLED_WORKERS_TEMPLATE, runtime).unwrap()
    }

    fn parse_after_jobworkerp_expansion(yaml: &str) -> serde_yaml::Value {
        let expanded = yaml
            .replace("%{MEMORY_GRPC_HOST}", "127.0.0.1")
            .replace("%{MEMORY_GRPC_PORT}", "9010");
        serde_yaml::from_str(&expanded).unwrap_or_else(|error| {
            panic!("rendered YAML is invalid after env expansion: {error}\n---\n{expanded}\n---")
        })
    }

    #[test]
    fn query_worker_yaml_holds_only_the_named_embedding_worker() {
        let runtime = resolve_embedding_runtime(&EmbeddingSettings {
            preset_id: Some("ruri-v3-310m-onnx-int8".into()),
            ..Default::default()
        });
        let yaml = render_query_embedding_worker_yaml(
            BUNDLED_WORKERS_TEMPLATE,
            &runtime,
            "memories-mm-embedding-0123456789abcdef",
        )
        .unwrap();
        // Nothing may be left for jobworkerp's env expansion: the suffix and
        // the Memories callback address are unknown to this process.
        assert!(!yaml.contains("%{"), "{yaml}");
        assert!(!yaml.contains("MEMORY_GRPC"), "{yaml}");

        let document: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let workers = document["workers"].as_sequence().unwrap();
        assert_eq!(workers.len(), 1);
        let worker = &workers[0];
        assert_eq!(worker["name"], "memories-mm-embedding-0123456789abcdef");
        assert_eq!(worker["runner"], "MultimodalEmbeddingRunner");
        assert_eq!(
            worker["settings"]["model_id"],
            "sirasagi62/ruri-v3-310m-ONNX"
        );
        assert_eq!(worker["settings"]["dtype"], "F32");
        assert_eq!(worker["settings"]["device"], embedding_device());
        assert_eq!(
            worker["settings"]["onnx_model_file"],
            "onnx/model_int8.onnx"
        );
        assert_eq!(document["defaults"]["response_type"], "DIRECT");
    }

    #[test]
    fn query_worker_yaml_rejects_an_empty_worker_name() {
        assert!(
            render_query_embedding_worker_yaml(BUNDLED_WORKERS_TEMPLATE, &default_runtime(), " ")
                .is_err()
        );
    }

    #[test]
    fn render_default_preset_omits_tokenizer_line() {
        // Every shipped preset has `tokenizer_hf_repo: None`. The default
        // runtime must therefore render WITHOUT a `tokenizer_model_id:`
        // line — present-but-empty would crash the runner.
        let yaml = render(&default_runtime());
        assert!(!yaml.contains("tokenizer_model_id"));
    }

    #[test]
    fn render_default_preset_uses_default_model_id() {
        let yaml = render(&default_runtime());
        let preset = embedding_presets::default_preset();
        let parsed = parse_after_jobworkerp_expansion(&yaml);
        let mm = parsed["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|worker| worker["runner"].as_str() == Some("MultimodalEmbeddingRunner"))
            .unwrap();
        assert_eq!(mm["settings"]["model_id"].as_str(), Some(preset.hf_repo));
    }

    #[test]
    fn render_includes_tokenizer_line_when_runtime_has_one() {
        let runtime = EmbeddingRuntime {
            model_id: "intfloat/multilingual-e5-base".into(),
            tokenizer_id: Some("intfloat/multilingual-e5-large".into()),
            vector_size: 768,
            dtype: "BF16".into(),
            max_sequence_length: 512,
            onnx_model_file: None,
            onnx_pooling: None,
            document_prefix: None,
            query_prefix: None,
            is_multimodal: false,
            model_revision: "unversioned".into(),
            distance_type: "cosine".into(),
        };
        let yaml = render(&runtime);
        let parsed = parse_after_jobworkerp_expansion(&yaml);
        let mm = parsed["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|worker| worker["runner"].as_str() == Some("MultimodalEmbeddingRunner"))
            .unwrap();
        assert_eq!(
            mm["settings"]["tokenizer_model_id"].as_str(),
            Some("intfloat/multilingual-e5-large")
        );
    }

    #[test]
    fn render_propagates_dtype_and_platform_device() {
        let runtime = EmbeddingRuntime {
            model_id: "org/name".into(),
            tokenizer_id: None,
            vector_size: 1024,
            dtype: "BF16".into(),
            max_sequence_length: 256,
            onnx_model_file: None,
            onnx_pooling: None,
            document_prefix: None,
            query_prefix: None,
            is_multimodal: false,
            model_revision: "unversioned".into(),
            distance_type: "cosine".into(),
        };
        let yaml = render(&runtime);
        assert!(yaml.contains("dtype: BF16"));
        assert!(yaml.contains(&format!("device: {}", embedding_device())));
        assert!(yaml.contains("max_sequence_length: 256"));
    }

    #[test]
    fn embedding_device_tracks_packaged_gpu_backend() {
        assert_eq!(embedding_device_for("macos", Some("metal")), "Metal");
        assert_eq!(embedding_device_for("linux", Some("cuda")), "CUDA");
        assert_eq!(embedding_device_for("linux", Some("cpu")), "CPU");
    }

    #[test]
    fn rendered_yaml_parses_as_valid_yaml() {
        // A round-trip through serde_yaml catches structural issues
        // (wrong indent, missing colons) that string assertions can't.
        let runtime = default_runtime();
        let yaml = render(&runtime);
        let parsed = parse_after_jobworkerp_expansion(&yaml);
        let workers = parsed.get("workers").and_then(|v| v.as_sequence()).unwrap();
        let mm = workers
            .iter()
            .find(|w| w.get("runner").and_then(|n| n.as_str()) == Some("MultimodalEmbeddingRunner"))
            .expect("memories-mm-embedding worker present");
        let model = mm
            .get("settings")
            .and_then(|s| s.get("model_id"))
            .and_then(|s| s.as_str())
            .unwrap();
        assert_eq!(model, runtime.model_id);
    }

    #[test]
    fn renderer_preserves_special_characters_in_model_id() {
        // Defensive: HF repo ids never contain `"`, but if a custom
        // entry sneaks one in, the escaper must keep the YAML parseable.
        let runtime = EmbeddingRuntime {
            model_id: "org/with\"quote".into(),
            tokenizer_id: None,
            vector_size: 768,
            dtype: "F16".into(),
            max_sequence_length: 512,
            onnx_model_file: None,
            onnx_pooling: None,
            document_prefix: None,
            query_prefix: None,
            is_multimodal: false,
            model_revision: "unversioned".into(),
            distance_type: "cosine".into(),
        };
        let yaml = render(&runtime);
        let parsed = parse_after_jobworkerp_expansion(&yaml);
        let workers = parsed.get("workers").and_then(|v| v.as_sequence()).unwrap();
        let model = workers
            .iter()
            .find(|w| w.get("runner").and_then(|n| n.as_str()) == Some("MultimodalEmbeddingRunner"))
            .and_then(|w| w.get("settings").and_then(|s| s.get("model_id")))
            .and_then(|s| s.as_str())
            .unwrap();
        assert_eq!(model, "org/with\"quote");
    }

    #[test]
    fn render_ruri_preset_emits_int8_onnx_settings() {
        let runtime = resolve_embedding_runtime(&EmbeddingSettings {
            preset_id: Some("ruri-v3-310m-onnx-int8".into()),
            ..Default::default()
        });
        let yaml = render(&runtime);
        let parsed = parse_after_jobworkerp_expansion(&yaml);
        let mm = parsed["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|worker| worker["runner"].as_str() == Some("MultimodalEmbeddingRunner"))
            .unwrap();
        assert_eq!(
            mm["settings"]["model_id"].as_str(),
            Some("sirasagi62/ruri-v3-310m-ONNX")
        );
        assert_eq!(
            mm["settings"]["onnx_model_file"].as_str(),
            Some("onnx/model_int8.onnx")
        );
        assert_eq!(
            mm["settings"]["onnx_pooling"].as_str(),
            Some("ONNX_POOLING_MEAN")
        );
    }

    #[test]
    fn stage_writes_to_staged_workers_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        let bundle = tmp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("auto-embedding.yaml"), b"do: []\n").unwrap();
        std::fs::write(
            bundle.join(STAGED_AUTO_EMBEDDING_WORKERS_YAML),
            BUNDLED_WORKERS_TEMPLATE,
        )
        .unwrap();
        let runtime = default_runtime();
        let path = stage_auto_embedding_workers_yaml(&runtime, &data, Some(&bundle)).unwrap();
        assert_eq!(
            path,
            data.staged_workers_dir()
                .join(STAGED_AUTO_EMBEDDING_WORKERS_YAML)
        );
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("memories-mm-embedding"));
    }

    #[test]
    fn stage_preserves_the_bundled_worker_contract_and_future_template_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        let bundle = tmp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("auto-embedding.yaml"), "document: {}\n").unwrap();
        std::fs::write(
            bundle.join(STAGED_AUTO_EMBEDDING_WORKERS_YAML),
            r#"
defaults:
  future_default: kept
workers:
  - name: "%{MEMORY_MM_EMBEDDING_WORKER:-memories-mm-embedding}%{MEMORY_EMBEDDING_SPACE_SUFFIX}"
    runner: MultimodalEmbeddingRunner
    settings:
      model_id: old/model
      tokenizer_model_id: old/tokenizer
      device: CPU
      dtype: F32
      onnx_model_file: old/model.onnx
      onnx_pooling: OLD_POOLING
      max_sequence_length: 8
      image_size_limit_bytes: 1
      chunking_config:
        max_chunk_tokens: 8
        min_chunk_tokens: 1
        future_chunk_option: kept
      future_runner_option: kept
  - name: "memories-auto-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"
    runner: WORKFLOW
    settings:
      workflow_data:
        $file: auto-embedding.yaml
  - name: memories-report-embedding-failure
    runner: GRPC
    settings:
      method: llm_memory.service.EmbeddingSpaceService/ReportEmbeddingFailure
  - name: memories-auto-image-caption
    runner: WORKFLOW
    settings:
      workflow_data:
        $file: auto-image-caption.yaml
  - name: future-worker
    runner: FutureRunner
    future_field: kept
"#,
        )
        .unwrap();

        let staged =
            stage_auto_embedding_workers_yaml(&default_runtime(), &data, Some(&bundle)).unwrap();
        let yaml: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(staged).unwrap()).unwrap();
        let workers = yaml["workers"].as_sequence().unwrap();
        let mm = workers
            .iter()
            .find(|worker| worker["runner"].as_str() == Some("MultimodalEmbeddingRunner"))
            .unwrap();
        let settings = &mm["settings"];
        let runtime = default_runtime();
        assert_eq!(
            settings["model_id"].as_str(),
            Some(runtime.model_id.as_str())
        );
        assert_eq!(
            settings["max_sequence_length"].as_u64(),
            Some(u64::from(runtime.max_sequence_length))
        );
        assert_eq!(settings["device"].as_str(), Some(embedding_device()));
        assert_eq!(settings["dtype"].as_str(), Some(runtime.dtype.as_str()));
        assert_eq!(settings["tokenizer_model_id"], serde_yaml::Value::Null);
        assert_eq!(settings["onnx_model_file"], serde_yaml::Value::Null);
        assert_eq!(settings["onnx_pooling"], serde_yaml::Value::Null);
        assert_eq!(
            settings["image_size_limit_bytes"].as_u64(),
            Some(u64::from(EMBEDDING_IMAGE_SIZE_LIMIT_BYTES))
        );
        assert_eq!(
            settings["chunking_config"]["future_chunk_option"].as_str(),
            Some("kept")
        );
        assert_eq!(settings["future_runner_option"].as_str(), Some("kept"));

        let worker_names: Vec<_> = workers
            .iter()
            .filter_map(|worker| worker["name"].as_str())
            .collect();
        assert!(worker_names.contains(
            &"%{MEMORY_MM_EMBEDDING_WORKER:-memories-mm-embedding}%{MEMORY_EMBEDDING_SPACE_SUFFIX}"
        ));
        assert!(worker_names.contains(&"memories-auto-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"));
        assert!(worker_names.contains(&"memories-report-embedding-failure"));
        assert!(worker_names.contains(&"memories-auto-image-caption"));
        assert_eq!(
            workers.last().unwrap()["future_field"].as_str(),
            Some("kept")
        );
        assert_eq!(yaml["defaults"]["future_default"].as_str(), Some("kept"));
    }

    #[test]
    fn app_workflows_keep_prefix_overlay_and_use_space_suffixed_embedding_workers() {
        let workflow_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows");
        let lookback_recall =
            std::fs::read_to_string(workflow_root.join("rag/lookback-recall.yaml")).unwrap();
        assert!(lookback_recall.contains("MEMORY_EMBEDDING_QUERY_PREFIX"));
        assert!(lookback_recall.contains("%{LOOKBACK_MM_EMBEDDING_WORKER}"));
        assert!(!lookback_recall.contains("LOOKBACK_EMBED_WORKER"));

        let contracts = [
            ("auto-embedding-workers.yaml", "memories-auto-embedding"),
            (
                "auto-image-embedding-workers.yaml",
                "memories-auto-image-embedding",
            ),
            (
                "auto-thread-embedding-workers.yaml",
                "memories-auto-thread-embedding",
            ),
            (
                "thread-reflection/auto-reflection-intent-embedding-workers.yaml",
                "memories-auto-reflection-intent-embedding",
            ),
            (
                "thread-reflection/auto-reflection-summary-embedding-workers.yaml",
                "memories-auto-reflection-summary-embedding",
            ),
        ];
        for (relative_path, worker_name) in contracts {
            let yaml = std::fs::read_to_string(workflow_root.join(relative_path))
                .unwrap_or_else(|error| panic!("read {relative_path}: {error}"));
            assert!(
                yaml.contains(worker_name),
                "{relative_path} lost {worker_name}"
            );
            assert!(
                yaml.contains("%{MEMORY_EMBEDDING_SPACE_SUFFIX}"),
                "{relative_path} does not suffix the space-specific worker"
            );
        }

        let memory_workflow =
            std::fs::read_to_string(workflow_root.join("auto-embedding.yaml")).unwrap();
        let image_workflow =
            std::fs::read_to_string(workflow_root.join("auto-image-embedding.yaml")).unwrap();
        let thread_workflow =
            std::fs::read_to_string(workflow_root.join("auto-thread-embedding.yaml")).unwrap();
        for (relative_path, workflow) in [
            ("auto-embedding.yaml", memory_workflow),
            ("auto-image-embedding.yaml", image_workflow),
            ("auto-thread-embedding.yaml", thread_workflow),
        ] {
            assert!(
                workflow.contains("token:"),
                "{relative_path} lost dispatch token input"
            );
            assert!(
                workflow.contains("MEMORY_EMBEDDING_SPACE_SUFFIX"),
                "{relative_path} uses a fixed embedding worker"
            );
            assert!(
                workflow.contains("memories-report-embedding-failure"),
                "{relative_path} lost failure reporting"
            );
        }
    }

    #[tokio::test]
    async fn rag_worker_file_include_uses_the_current_memories_embedding_worker_name() {
        use std::collections::HashMap;

        use crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse;

        let workers_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers");
        let worker_yaml = std::fs::read_to_string(workers_root.join("llm-workers.yaml")).unwrap();
        let expanded = jobworkerp_client::client::yaml_common::expand_env(&worker_yaml).unwrap();
        let mut document: serde_yaml::Value = serde_yaml::from_str(&expanded).unwrap();
        let recall = document["workers"]
            .as_sequence_mut()
            .unwrap()
            .iter_mut()
            .find(|worker| worker["name"].as_str() == Some("lookback_recall"))
            .expect("llm-workers.yaml registers the app-owned RAG overlay");

        let response = GetEmbeddingSpaceResponse {
            mm_embedding_worker_name: "memories-mm-embedding-0123456789abcdef".into(),
            registration_state: 1,
            ..Default::default()
        };
        let worker_name = response.mm_embedding_worker_name;
        let overrides =
            HashMap::from([("LOOKBACK_MM_EMBEDDING_WORKER".into(), worker_name.clone())]);
        let previous_suffix = std::env::var_os("MEMORY_EMBEDDING_SPACE_SUFFIX");
        // Exercise the normal startup case: Memories supplies this placeholder
        // only while it expands its own YAML, not through the app process env.
        unsafe { std::env::remove_var("MEMORY_EMBEDDING_SPACE_SUFFIX") };
        let result = jobworkerp_client::client::yaml_common::resolve_includes_with_overrides(
            &mut recall["settings"],
            &workers_root,
            &overrides,
        )
        .await;
        match previous_suffix {
            Some(value) => unsafe { std::env::set_var("MEMORY_EMBEDDING_SPACE_SUFFIX", value) },
            None => unsafe { std::env::remove_var("MEMORY_EMBEDDING_SPACE_SUFFIX") },
        }
        result.expect("the real $file loader must resolve the app RAG workflow");

        let workflow_text = recall["settings"]["workflow_data"]
            .as_str()
            .expect("$file expansion produces the workflow source");
        let workflow: serde_yaml::Value = serde_yaml::from_str(workflow_text).unwrap();
        let registered_worker_name = workflow["do"][0]["embedQuery"]["run"]["worker"]["name"]
            .as_str()
            .expect("RAG query embedding names a jobworkerp worker");
        let workflow_config = workflow_text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(registered_worker_name, worker_name);
        assert!(
            !workflow_config.contains("%{"),
            "registered RAG workflow must not retain unresolved env references"
        );
    }

    #[test]
    fn app_rag_manifest_references_only_bundled_lookback_workflows() {
        let workflow_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows");
        let manifest_path = workflow_root.join("rag-tools-manifest.yaml");
        let manifest_text = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest: serde_yaml::Value = serde_yaml::from_str(&manifest_text).unwrap();
        let worker_entries = manifest["workers"]["entries"].as_sequence().unwrap();
        let function_entries = manifest["function_sets"]["entries"].as_sequence().unwrap();
        let expected = [
            ("lookback_recall", "rag/lookback-recall.yaml"),
            ("lookback_get_context", "rag/lookback-get-context.yaml"),
            (
                "lookback_get_reflections",
                "rag/lookback-get-reflections.yaml",
            ),
        ];

        for (name, workflow) in expected {
            let worker = worker_entries
                .iter()
                .find(|entry| entry["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("manifest does not register {name}"));
            assert_eq!(
                worker["settings"]["workflow_data"]["$file"].as_str(),
                Some(workflow)
            );
            assert!(workflow_root.join(workflow).is_file());
        }
        let recall_targets = function_entries
            .iter()
            .find(|entry| entry["name"].as_str() == Some("lookback-rag"))
            .unwrap()["targets"]
            .as_sequence()
            .unwrap();
        for (name, _) in expected {
            assert!(
                recall_targets
                    .iter()
                    .any(|target| target["type"].as_str() == Some("WORKER")
                        && target["name"].as_str() == Some(name))
            );
        }
    }

    #[test]
    fn bundled_embedding_worker_workflow_files_all_resolve() {
        fn collect_file_references(value: &serde_yaml::Value, files: &mut Vec<String>) {
            if let Some(file) = value.get("$file").and_then(serde_yaml::Value::as_str) {
                files.push(file.to_string());
            }
            match value {
                serde_yaml::Value::Sequence(values) => {
                    for value in values {
                        collect_file_references(value, files);
                    }
                }
                serde_yaml::Value::Mapping(values) => {
                    for value in values.values() {
                        collect_file_references(value, files);
                    }
                }
                _ => {}
            }
        }

        let workflow_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows");
        let worker_files = [
            "auto-embedding-workers.yaml",
            "auto-image-embedding-workers.yaml",
            "auto-thread-embedding-workers.yaml",
            "thread-reflection/auto-reflection-intent-embedding-workers.yaml",
            "thread-reflection/auto-reflection-summary-embedding-workers.yaml",
        ];

        for relative_path in worker_files {
            let worker_path = workflow_root.join(relative_path);
            let text = std::fs::read_to_string(&worker_path)
                .unwrap_or_else(|error| panic!("read {relative_path}: {error}"));
            let (parseable, _) = protect_unquoted_env_placeholders(&text);
            let yaml: serde_yaml::Value = serde_yaml::from_str(&parseable)
                .unwrap_or_else(|error| panic!("parse {relative_path}: {error}"));
            let mut references = Vec::new();
            collect_file_references(&yaml, &mut references);

            assert!(
                !references.is_empty(),
                "{relative_path} has no workflow refs"
            );
            for reference in references {
                assert!(
                    worker_path.parent().unwrap().join(&reference).is_file(),
                    "{relative_path} references missing workflow {reference}"
                );
            }
        }
    }

    #[test]
    fn image_caption_is_separate_and_uses_conditional_writeback() {
        let workflow_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows");
        let worker_yaml =
            std::fs::read_to_string(workflow_root.join("auto-image-embedding-workers.yaml"))
                .unwrap();
        let (parseable, _) = protect_unquoted_env_placeholders(&worker_yaml);
        let workers: serde_yaml::Value = serde_yaml::from_str(&parseable).unwrap();
        let caption_worker = workers["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|worker| worker["name"].as_str() == Some("memories-auto-image-caption"))
            .expect("separate caption workflow worker is registered");
        assert_eq!(
            caption_worker["settings"]["workflow_data"]["$file"].as_str(),
            Some("auto-image-caption.yaml")
        );
        assert!(workflow_root.join("auto-image-caption.yaml").is_file());

        let caption =
            std::fs::read_to_string(workflow_root.join("auto-image-caption.yaml")).unwrap();
        assert!(caption.contains("required: [memory_id, media_object_id]"));
        assert!(caption.contains("name: memories-write-back-caption"));
        assert!(caption.contains("media_object_id: { value: $workflow.input.media_object_id }"));
        assert!(caption.contains("caption: .caption"));
        assert!(!caption.contains("memories-upsert-embedding"));
        assert!(!caption.contains("MEMORY_EMBEDDING_SPACE_SUFFIX"));
        assert!(worker_yaml.contains("method: llm_memory.service.MemoryService/WriteBackCaption"));
    }

    #[test]
    fn stage_copies_auto_embedding_yaml_alongside() {
        // The WORKFLOW worker uses `$file: auto-embedding.yaml` so the
        // sibling file must be staged too — else jobworkerp's loader
        // fails to register the workflow worker at startup.
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        let bundle = tmp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("auto-embedding.yaml"), b"do: []\n").unwrap();
        std::fs::write(
            bundle.join(STAGED_AUTO_EMBEDDING_WORKERS_YAML),
            BUNDLED_WORKERS_TEMPLATE,
        )
        .unwrap();

        let runtime = default_runtime();
        stage_auto_embedding_workers_yaml(&runtime, &data, Some(&bundle)).unwrap();

        let staged_sibling = data.staged_workers_dir().join("auto-embedding.yaml");
        assert!(staged_sibling.exists(), "sibling YAML not staged");
        assert_eq!(
            std::fs::read_to_string(&staged_sibling).unwrap(),
            "do: []\n"
        );
    }

    #[test]
    fn stage_errors_when_bundle_dir_missing() {
        // Regression: writing a staged YAML whose `$file: auto-embedding.yaml`
        // cannot be resolved would point `MEMORY_WORKERS_YAML` at a file the
        // worker loader cannot register. Refuse to stage in that case so the
        // caller falls back to the committed bundle path.
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        let err = stage_auto_embedding_workers_yaml(&default_runtime(), &data, None).unwrap_err();
        assert!(
            matches!(err, crate::error::AppError::Config(_)),
            "expected AppError::Config, got {err:?}"
        );
        let staged_target = data
            .staged_workers_dir()
            .join(STAGED_AUTO_EMBEDDING_WORKERS_YAML);
        assert!(
            !staged_target.exists(),
            "staged worker YAML must NOT be written when companion is missing"
        );
    }

    #[test]
    fn stage_errors_when_companion_missing_in_bundle() {
        // Same protection as above, but for the case where the bundle dir
        // exists yet `auto-embedding.yaml` is absent (broken install / dev
        // setup that only copied the worker YAML).
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        let bundle = tmp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let err = stage_auto_embedding_workers_yaml(&default_runtime(), &data, Some(&bundle))
            .unwrap_err();
        assert!(
            matches!(err, crate::error::AppError::Config(_)),
            "expected AppError::Config, got {err:?}"
        );
        let staged_target = data
            .staged_workers_dir()
            .join(STAGED_AUTO_EMBEDDING_WORKERS_YAML);
        assert!(
            !staged_target.exists(),
            "staged worker YAML must NOT be written when companion is missing"
        );
    }

    #[test]
    fn stage_errors_when_bundled_worker_template_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        let bundle = tmp.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("auto-embedding.yaml"), "do: []\n").unwrap();

        let error = stage_auto_embedding_workers_yaml(&default_runtime(), &data, Some(&bundle))
            .unwrap_err();

        assert!(matches!(error, AppError::Config(_)));
        assert!(
            !data
                .staged_workers_dir()
                .join(STAGED_AUTO_EMBEDDING_WORKERS_YAML)
                .exists(),
            "no stale or reconstructed worker definition may be staged"
        );
    }
}
