use jobworkerp_client::{
    jobworkerp::data::{
        JobExecutionOverrides, ResponseType, RetryPolicy, RetryType, StreamingOutputType,
    },
    proto::JobworkerpProto,
};

use super::{JobworkerpHandle, workflow::VerifiedRegisteredWorker};
use crate::{
    commands::llm_settings::LlmMode,
    error::{AppError, AppResult},
};

fn unavailable() -> AppError {
    AppError::Jobworkerp("configured semantic judge worker is unavailable or changed".into())
}

impl JobworkerpHandle {
    pub(crate) async fn observe_registered_chat(
        &self,
        mode: LlmMode,
    ) -> AppResult<VerifiedRegisteredWorker> {
        let (worker_id, worker_data, runner_data) = self
            .registered_worker_data(crate::commands::llm_settings::worker_name_for(mode))
            .await?;
        let expected_runner = match mode {
            LlmMode::Local => crate::jobworkerp::LLM_PROMPT_RUNNER_NAME,
            LlmMode::External => "LLM",
        };
        let method = runner_data
            .method_proto_map
            .as_ref()
            .and_then(|map| map.schemas.get("chat"))
            .ok_or_else(unavailable)?;
        if worker_id.value <= 0
            || runner_data.name != expected_runner
            || worker_data.response_type != ResponseType::Direct as i32
            || worker_data.broadcast_results
            || method.require_client_stream
            || !matches!(
                StreamingOutputType::try_from(method.output_type),
                Ok(StreamingOutputType::Streaming | StreamingOutputType::Both)
            )
        {
            return Err(unavailable());
        }
        JobworkerpProto::parse_job_args_schema_descriptor(&runner_data, Some("chat"))
            .map_err(|_| unavailable())?
            .filter(|descriptor| descriptor.name() == "LLMChatArgs")
            .ok_or_else(unavailable)?;
        JobworkerpProto::parse_result_schema_descriptor(&runner_data, Some("chat"))
            .map_err(|_| unavailable())?
            .filter(|descriptor| descriptor.name() == "LLMChatResult")
            .ok_or_else(unavailable)?;
        Ok(VerifiedRegisteredWorker {
            worker_id,
            worker_data,
            runner_data,
        })
    }

    pub(crate) async fn begin_registered_chat(
        &self,
        expected: &VerifiedRegisteredWorker,
        args: serde_json::Value,
        key: &str,
        policy: crate::coding_agent::semantic_judge::SemanticJudgePolicy,
        cancellation: &tokio_util::sync::CancellationToken,
        deadline: tokio::time::Instant,
    ) -> AppResult<SemanticChatDispatch> {
        policy.validate().map_err(|_| unavailable())?;
        let external = expected.runner_data.name == "LLM";
        if external
            && expected
                .runner_data
                .method_proto_map
                .as_ref()
                .and_then(|map| map.schemas.get("chat"))
                .is_none_or(|method| method.output_type != StreamingOutputType::Both as i32)
        {
            return Err(unavailable());
        }
        let options = super::workflow::VerifiedDispatchOptions {
            timeout_seconds: u64::from(policy.deadline_seconds),
            method: "chat",
            uniq_key: Some(key),
            cancellation: Some(cancellation),
            deadline: Some(deadline),
            overrides: Some(JobExecutionOverrides {
                retry_policy: Some(RetryPolicy {
                    r#type: if policy.max_retries == 0 {
                        RetryType::None
                    } else {
                        RetryType::Exponential
                    } as i32,
                    max_retry: policy.max_retries,
                    interval: 800,
                    max_interval: 60_000,
                    basis: 2.0,
                }),
                response_type: external.then_some(ResponseType::NoResult as i32),
                store_success: external.then_some(true),
                store_failure: external.then_some(true),
                ..Default::default()
            }),
        };
        if !external {
            return self
                .dispatch_verified_method_with_options(expected, args, options)
                .await
                .map(|stream| SemanticChatDispatch::Stream(Box::new(stream)));
        }
        let super::workflow::PreparedVerifiedMethodDispatch {
            mut client,
            request,
            ..
        } = self
            .prepare_verified_method_dispatch(expected, args, options)
            .await?;
        let expected_args = request.args.clone();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.enqueue(tonic::Request::new(request)),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?
        .into_inner();
        let job_id = response
            .id
            .filter(|id| id.value > 0)
            .ok_or_else(unavailable)?;
        if response.result.is_some() {
            return Err(unavailable());
        }
        Ok(SemanticChatDispatch::Collected(Box::new(
            CollectedSemanticJob {
                handle: self.clone(),
                job_id,
                worker_id: expected.worker_id,
                worker_name: expected.worker_data.name.clone(),
                args: expected_args,
                key: key.to_owned(),
                max_retries: policy.max_retries,
            },
        )))
    }
}

impl super::DispatchStream {
    pub(crate) async fn collect_semantic_text(mut self) -> AppResult<Vec<u8>> {
        crate::coding_agent::judge_provider::collect_judge_stream(&mut self.inner)
            .await
            .map_err(|_| unavailable())
    }
}

pub(crate) enum SemanticChatDispatch {
    Stream(Box<super::DispatchStream>),
    Collected(Box<CollectedSemanticJob>),
}

impl SemanticChatDispatch {
    pub(crate) fn job_id(&self) -> Option<jobworkerp_client::jobworkerp::data::JobId> {
        match self {
            Self::Stream(stream) => stream.job_id,
            Self::Collected(job) => Some(job.job_id),
        }
    }
    pub(crate) async fn collect_semantic_text(self) -> AppResult<Vec<u8>> {
        match self {
            Self::Stream(stream) => stream.collect_semantic_text().await,
            Self::Collected(job) => job.collect().await,
        }
    }
}

pub(crate) struct CollectedSemanticJob {
    handle: JobworkerpHandle,
    job_id: jobworkerp_client::jobworkerp::data::JobId,
    worker_id: jobworkerp_client::jobworkerp::data::WorkerId,
    worker_name: String,
    args: Vec<u8>,
    key: String,
    max_retries: u32,
}

impl CollectedSemanticJob {
    async fn collect(self) -> AppResult<Vec<u8>> {
        use jobworkerp_client::jobworkerp::{
            data::ResultStatus, service::FindJobResultListRequest,
        };
        loop {
            let request = FindJobResultListRequest {
                worker_ids: vec![self.worker_id],
                uniq_key: Some(self.key.clone()),
                statuses: vec![
                    ResultStatus::Success as i32,
                    ResultStatus::FatalError as i32,
                    ResultStatus::OtherError as i32,
                    ResultStatus::MaxRetry as i32,
                    ResultStatus::Abort as i32,
                    ResultStatus::Cancelled as i32,
                ],
                limit: Some(2),
                offset: Some(0),
                ascending: Some(false),
                ..Default::default()
            };
            let limit = self
                .args
                .len()
                .checked_add(512 * 1024)
                .ok_or_else(unavailable)?;
            let mut results = self
                .handle
                .inner
                .jobworkerp_client
                .job_result_client()
                .await
                .max_decoding_message_size(limit)
                .find_list_by(tonic::Request::new(request))
                .await
                .map_err(|_| unavailable())?
                .into_inner();
            let result = results.message().await.map_err(|_| unavailable())?;
            if let Some(result) = result {
                if results
                    .message()
                    .await
                    .map_err(|_| unavailable())?
                    .is_some()
                {
                    return Err(unavailable());
                }
                let data = result.data.ok_or_else(unavailable)?;
                // Result lookup reconstructs delivery settings from current worker defaults.
                // Bind the result to execution inputs, not mutable delivery metadata.
                if data.job_id != Some(self.job_id)
                    || data.worker_id != Some(self.worker_id)
                    || data.worker_name != self.worker_name
                    || data.args != self.args
                    || data.uniq_key.as_deref() != Some(&self.key)
                    || data.using.as_deref() != Some("chat")
                    || data.status != ResultStatus::Success as i32
                    || data.retried > self.max_retries
                {
                    return Err(unavailable());
                }
                return crate::coding_agent::judge_provider::decode_terminal_chat_text(
                    &data.output.ok_or_else(unavailable)?.items,
                )
                .map_err(|_| unavailable());
            }
            // The server owns bounded provider retries; result polling never resends the job.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}
