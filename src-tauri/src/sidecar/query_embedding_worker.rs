//! Make sure the jobworkerp that runs the app's workflows can embed search
//! queries in the same space as the vectors Memories stores.
//!
//! RAG and cross-thread Issue resolution embed their queries through the
//! worker named by Memories (`mm_embedding_worker_name`). Memories registers
//! that worker only on its own jobworkerp, so whenever it uses another one
//! (a remote Memories, for example) the name does not exist where the
//! workflows run, and hybrid search silently degrades to full-text search.

use std::path::Path;

use super::lifecycle::{SidecarWarning, SidecarWarningKind};
use crate::commands::embedding_settings::{EmbeddingRuntime, runtime_for_embedding_space};
use crate::commands::embedding_workers_yaml::{
    EMBEDDING_RUNNER_NAME, read_bundled_embedding_workers_template,
    render_query_embedding_worker_yaml,
};
use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse;
use crate::jobworkerp::JobworkerpHandle;

/// How the memories-dependent workflows embed their search queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoriesQueryEmbedding {
    pub worker_name: String,
    /// The query prefix of Memories' embedding model. `None` when the model
    /// is not known here, leaving the configured prefix in place; an empty
    /// string means the model takes no prefix.
    pub query_prefix: Option<String>,
}

impl MemoriesQueryEmbedding {
    #[cfg(test)]
    pub fn named(worker_name: &str) -> Self {
        Self {
            worker_name: worker_name.to_string(),
            query_prefix: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryEmbeddingAction {
    /// Memories has no vector store, so there is nothing to search against.
    NoSpace,
    Ready,
    Register,
    /// The name is taken by another model. It may be the worker Memories
    /// embeds documents with, so it is never overwritten.
    ModelMismatch {
        found: String,
    },
}

/// `existing` is the registered worker's runner name and model id.
fn decide(
    space_model_id: Option<&str>,
    existing: Option<(&str, Option<&str>)>,
) -> QueryEmbeddingAction {
    let Some(space_model_id) = space_model_id.filter(|id| !id.trim().is_empty()) else {
        return QueryEmbeddingAction::NoSpace;
    };
    match existing {
        None => QueryEmbeddingAction::Register,
        Some((runner, model))
            if runner == EMBEDDING_RUNNER_NAME && model == Some(space_model_id) =>
        {
            QueryEmbeddingAction::Ready
        }
        Some((runner, model)) => QueryEmbeddingAction::ModelMismatch {
            found: format!("{} ({runner})", model.unwrap_or("unknown model")),
        },
    }
}

/// Check Memories' query embedding worker on `handle` and register it when
/// it is missing. Every failure is a warning: without the worker, search
/// falls back to full text, which must not keep the app from starting.
pub(crate) async fn prepare_query_embedding(
    handle: &JobworkerpHandle,
    response: &GetEmbeddingSpaceResponse,
    local_runtime: &EmbeddingRuntime,
    bundled_workflows_dir: Option<&Path>,
    warnings: &mut Vec<SidecarWarning>,
) -> MemoriesQueryEmbedding {
    let worker_name = response.mm_embedding_worker_name.as_str();
    let space = response.current_space.as_ref();
    let space_runtime = space.and_then(|space| {
        runtime_for_embedding_space(&space.model_id, space.dimension, local_runtime)
    });
    match ensure_worker(
        handle,
        worker_name,
        space.map(|space| space.model_id.as_str()),
        space_runtime.as_ref(),
        bundled_workflows_dir,
    )
    .await
    {
        Ok(action) => {
            tracing::info!(
                worker = worker_name,
                ?action,
                "query embedding worker checked"
            )
        }
        Err(error) => {
            tracing::warn!(worker = worker_name, %error, "query embedding worker unavailable");
            warnings.push(SidecarWarning {
                kind: SidecarWarningKind::WorkerApplyFailed,
                message: error.to_string(),
                detail: Some(worker_name.to_string()),
            });
        }
    }
    MemoriesQueryEmbedding {
        worker_name: worker_name.to_string(),
        query_prefix: space_runtime.map(|runtime| runtime.query_prefix.unwrap_or_default()),
    }
}

async fn ensure_worker(
    handle: &JobworkerpHandle,
    worker_name: &str,
    space_model_id: Option<&str>,
    space_runtime: Option<&EmbeddingRuntime>,
    bundled_workflows_dir: Option<&Path>,
) -> AppResult<QueryEmbeddingAction> {
    if decide(space_model_id, None) == QueryEmbeddingAction::NoSpace {
        return Ok(QueryEmbeddingAction::NoSpace);
    }
    let existing = handle.registered_worker_settings(worker_name).await?;
    let action = decide(
        space_model_id,
        existing.as_ref().map(|(runner, settings)| {
            (
                runner.as_str(),
                // Decoded protobuf settings use JSON (camelCase) names.
                settings.get("modelId").and_then(serde_json::Value::as_str),
            )
        }),
    );
    let space_model_id = space_model_id.unwrap_or_default();
    match &action {
        QueryEmbeddingAction::NoSpace | QueryEmbeddingAction::Ready => {}
        QueryEmbeddingAction::ModelMismatch { found } => {
            return Err(AppError::WorkerRegistration(format!(
                "query embedding worker {worker_name} uses {found}, not Memories' {space_model_id}; search falls back to full text"
            )));
        }
        QueryEmbeddingAction::Register => {
            let runtime = space_runtime.ok_or_else(|| {
                AppError::WorkerRegistration(format!(
                    "cannot identify runner settings for Memories' embedding model {space_model_id}; search falls back to full text"
                ))
            })?;
            let bundle = bundled_workflows_dir
                .ok_or_else(|| AppError::Config("no bundled workflows dir".into()))?;
            let template = read_bundled_embedding_workers_template(bundle)?;
            let body = render_query_embedding_worker_yaml(&template, runtime, worker_name)?;
            handle
                .register_workers_from_yaml_str(&body, Path::new("."))
                .await?;
        }
    }
    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE_MODEL: &str = "Qwen/Qwen3-VL-Embedding-2B";

    #[test]
    fn no_space_means_nothing_to_register() {
        assert_eq!(decide(None, None), QueryEmbeddingAction::NoSpace);
        assert_eq!(decide(Some(" "), None), QueryEmbeddingAction::NoSpace);
    }

    #[test]
    fn a_missing_worker_is_registered() {
        assert_eq!(
            decide(Some(SPACE_MODEL), None),
            QueryEmbeddingAction::Register
        );
    }

    #[test]
    fn a_worker_with_the_space_model_is_ready() {
        assert_eq!(
            decide(
                Some(SPACE_MODEL),
                Some((EMBEDDING_RUNNER_NAME, Some(SPACE_MODEL)))
            ),
            QueryEmbeddingAction::Ready
        );
    }

    #[test]
    fn another_model_or_runner_is_a_mismatch() {
        for existing in [
            (EMBEDDING_RUNNER_NAME, Some("Qwen/Qwen3-Embedding-0.6B")),
            ("EmbeddingLlmRunner", Some(SPACE_MODEL)),
            (EMBEDDING_RUNNER_NAME, None),
        ] {
            assert!(
                matches!(
                    decide(Some(SPACE_MODEL), Some(existing)),
                    QueryEmbeddingAction::ModelMismatch { .. }
                ),
                "{existing:?}"
            );
        }
    }
}
