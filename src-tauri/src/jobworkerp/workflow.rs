use std::{collections::HashMap, sync::Arc};

use jobworkerp_client::{
    client::helper::UseJobworkerpClientHelper,
    jobworkerp::data::{ResponseType, RunnerData, StreamingOutputType, WorkerData, WorkerId},
    proto::JobworkerpProto,
};
use prost::Message as _;
use serde::Serialize as _;
use serde_json::Value;

use super::{DispatchStream, JobworkerpHandle};
use crate::error::{AppError, AppResult};

pub(crate) struct VerifiedRegisteredWorker {
    pub(super) worker_id: WorkerId,
    pub(super) worker_data: WorkerData,
    pub(super) runner_data: RunnerData,
}

pub(super) struct VerifiedDispatchOptions<'a> {
    pub timeout_seconds: u64,
    pub method: &'a str,
    pub uniq_key: Option<&'a str>,
    pub cancellation: Option<&'a tokio_util::sync::CancellationToken>,
    pub overrides: Option<jobworkerp_client::jobworkerp::data::JobExecutionOverrides>,
    pub deadline: Option<tokio::time::Instant>,
}

pub(super) struct PreparedVerifiedMethodDispatch {
    pub client: jobworkerp_client::jobworkerp::service::job_service_client::JobServiceClient<
        tonic::transport::Channel,
    >,
    pub request: jobworkerp_client::jobworkerp::service::JobRequest,
    pub result_descriptor: prost_reflect::MessageDescriptor,
}

async fn before_enqueue<T>(
    future: impl std::future::Future<Output = T>,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    deadline: Option<tokio::time::Instant>,
) -> AppResult<T> {
    tokio::select! {
        biased;
        _ = async { match cancellation { Some(token) => token.cancelled().await, None => std::future::pending().await } } => Err(unavailable()),
        _ = async { match deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => Err(unavailable()),
        result = future => Ok(result),
    }
}

impl VerifiedRegisteredWorker {
    pub(crate) fn worker_id(&self) -> i64 {
        self.worker_id.value
    }
    pub(crate) fn chat_configuration_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let method = &self
            .runner_data
            .method_proto_map
            .as_ref()
            .expect("verified method map")
            .schemas["chat"];
        let material = serde_json::to_vec(&(
            self.worker_id.value,
            self.worker_data.runner_id,
            &self.runner_data.name,
            &self.runner_data.runner_settings_proto,
            &self.worker_data.runner_settings,
            &method.args_proto,
            &method.result_proto,
        ))
        .expect("snapshot serialization");
        format!("{:x}", Sha256::digest(material))
    }
    pub(crate) fn decoded_settings(&self) -> AppResult<serde_json::Value> {
        decode_runner_settings(&self.runner_data, &self.worker_data).ok_or_else(unavailable)
    }
    pub(crate) fn schema_digests(&self) -> (String, String) {
        use sha2::{Digest, Sha256};
        let method = &self
            .runner_data
            .method_proto_map
            .as_ref()
            .expect("verified method map")
            .schemas["run"];
        (
            format!("{:x}", Sha256::digest(method.args_proto.as_bytes())),
            format!("{:x}", Sha256::digest(method.result_proto.as_bytes())),
        )
    }
    pub(crate) async fn revalidate(&self, handle: &JobworkerpHandle) -> AppResult<()> {
        let (id, worker, runner) = handle
            .registered_worker_data(&self.worker_data.name)
            .await?;
        if id != self.worker_id || worker != self.worker_data || runner != self.runner_data {
            return Err(unavailable());
        }
        Ok(())
    }
}

/// Decode a worker's protobuf runner settings into JSON (camelCase names).
fn decode_runner_settings(runner_data: &RunnerData, worker_data: &WorkerData) -> Option<Value> {
    let descriptor = JobworkerpProto::parse_runner_settings_schema_descriptor(runner_data)
        .ok()
        .flatten()?;
    prost_reflect::DynamicMessage::decode(descriptor, worker_data.runner_settings.as_slice())
        .ok()?
        .serialize(serde_json::value::Serializer)
        .ok()
}

fn unavailable() -> AppError {
    AppError::Jobworkerp("registered workflow changed or could not be verified".into())
}

impl JobworkerpHandle {
    pub(super) async fn registered_worker_data(
        &self,
        name: &str,
    ) -> AppResult<(WorkerId, WorkerData, RunnerData)> {
        self.find_registered_worker(name)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)
    }

    /// `None` when no worker has this name; lookup failures are errors.
    async fn find_registered_worker(
        &self,
        name: &str,
    ) -> AppResult<Option<(WorkerId, WorkerData, RunnerData)>> {
        let Some((id, worker)) = self
            .inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), name)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find worker {name}: {e}")))?
        else {
            return Ok(None);
        };
        if worker.name != name {
            return Err(unavailable());
        }
        let runner_id = worker.runner_id.ok_or_else(unavailable)?;
        let runner = self
            .inner
            .jobworkerp_client
            .runner_client()
            .await
            .find(runner_id)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find runner of worker {name}: {e}")))?
            .into_inner()
            .data
            .filter(|runner| runner.id == Some(runner_id))
            .and_then(|runner| runner.data)
            .ok_or_else(unavailable)?;
        Ok(Some((id, worker, runner)))
    }

    /// The runner name and decoded settings of a worker, or `None` when no
    /// worker has this name. Settings that cannot be decoded are returned as
    /// `null` so callers treat them as unknown rather than as absent.
    pub(crate) async fn registered_worker_settings(
        &self,
        name: &str,
    ) -> AppResult<Option<(String, Value)>> {
        Ok(self
            .find_registered_worker(name)
            .await?
            .map(|(_, worker, runner)| {
                let settings = decode_runner_settings(&runner, &worker).unwrap_or(Value::Null);
                (runner.name, settings)
            }))
    }

    pub(crate) async fn observe_registered_workflow(
        &self,
        name: &str,
        expected_worker_id: i64,
        definition: &Value,
    ) -> AppResult<VerifiedRegisteredWorker> {
        let (worker_id, worker_data, runner_data) = self.registered_worker_data(name).await?;
        let method = runner_data
            .method_proto_map
            .as_ref()
            .and_then(|methods| methods.schemas.get("run"))
            .ok_or_else(unavailable)?;
        if worker_id.value != expected_worker_id
            || expected_worker_id <= 0
            || runner_data.name != "WORKFLOW"
            || runner_data.runner_type
                != jobworkerp_client::jobworkerp::data::RunnerType::Workflow as i32
            || worker_data.response_type != ResponseType::Direct as i32
            || worker_data.broadcast_results
            || worker_data.use_static
            || method.require_client_stream
            || !matches!(
                StreamingOutputType::try_from(method.output_type),
                Ok(StreamingOutputType::Streaming | StreamingOutputType::Both)
            )
        {
            return Err(unavailable());
        }
        let args_descriptor =
            JobworkerpProto::parse_job_args_schema_descriptor(&runner_data, Some("run"))
                .map_err(|_| unavailable())?
                .ok_or_else(unavailable)?;
        let result_descriptor =
            JobworkerpProto::parse_result_schema_descriptor(&runner_data, Some("run"))
                .map_err(|_| unavailable())?
                .ok_or_else(unavailable)?;
        if !args_descriptor
            .get_field_by_name("input")
            .is_some_and(|field| field.kind() == prost_reflect::Kind::String)
            || result_descriptor.full_name() != super::WORKFLOW_RESULT_MESSAGE
        {
            return Err(unavailable());
        }
        let settings =
            decode_runner_settings(&runner_data, &worker_data).ok_or_else(unavailable)?;
        let actual: Value = serde_json::from_str(
            settings
                .get("workflowData")
                .and_then(Value::as_str)
                .ok_or_else(unavailable)?,
        )
        .map_err(|_| unavailable())?;
        if &actual != definition {
            return Err(unavailable());
        }
        Ok(VerifiedRegisteredWorker {
            worker_id,
            worker_data,
            runner_data,
        })
    }

    pub(crate) async fn dispatch_verified_workflow_with_cancellation(
        &self,
        expected: &VerifiedRegisteredWorker,
        input: Value,
        timeout_seconds: u64,
        cancellation: Option<&tokio_util::sync::CancellationToken>,
    ) -> AppResult<DispatchStream> {
        self.dispatch_verified_method_with_options(
            expected,
            input,
            VerifiedDispatchOptions {
                timeout_seconds,
                method: "run",
                uniq_key: None,
                cancellation,
                overrides: None,
                deadline: None,
            },
        )
        .await
    }

    pub(super) async fn dispatch_verified_method_with_options(
        &self,
        expected: &VerifiedRegisteredWorker,
        input: Value,
        options: VerifiedDispatchOptions<'_>,
    ) -> AppResult<DispatchStream> {
        let PreparedVerifiedMethodDispatch {
            mut client,
            request,
            result_descriptor,
        } = self
            .prepare_verified_method_dispatch(expected, input, options)
            .await?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.enqueue_for_stream(tonic::Request::new(request)),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
        let job_id = response
            .metadata()
            .get_bin(jobworkerp_client::command::job::JOB_ID_HEADER_NAME)
            .map(|encoded| {
                encoded
                    .to_bytes()
                    .map_err(|_| unavailable())
                    .and_then(|bytes| {
                        jobworkerp_client::jobworkerp::data::JobId::decode(bytes)
                            .map_err(|_| unavailable())
                    })
            })
            .transpose()?;
        if job_id.is_some_and(|id| id.value <= 0) {
            return Err(unavailable());
        }
        let inner = response.into_inner();
        let result_descriptor = Some(result_descriptor);
        Ok(DispatchStream {
            job_id,
            inner,
            result_descriptor,
        })
    }
    pub(super) async fn prepare_verified_method_dispatch(
        &self,
        expected: &VerifiedRegisteredWorker,
        input: Value,
        options: VerifiedDispatchOptions<'_>,
    ) -> AppResult<PreparedVerifiedMethodDispatch> {
        let VerifiedDispatchOptions {
            timeout_seconds,
            method,
            uniq_key,
            cancellation,
            overrides,
            deadline,
        } = options;
        let lookup = self.registered_worker_data(&expected.worker_data.name);
        let (id, worker, runner) = before_enqueue(lookup, cancellation, deadline).await??;
        if id != expected.worker_id
            || worker != expected.worker_data
            || runner != expected.runner_data
        {
            return Err(unavailable());
        }
        let args_descriptor =
            JobworkerpProto::parse_job_args_schema_descriptor(&runner, Some(method))
                .map_err(|_| unavailable())?
                .ok_or_else(unavailable)?;
        let result_descriptor =
            JobworkerpProto::parse_result_schema_descriptor(&runner, Some(method))
                .map_err(|_| unavailable())?
                .filter(|descriptor| {
                    descriptor.name()
                        == if method == "run" {
                            "WorkflowResult"
                        } else {
                            "LLMChatResult"
                        }
                })
                .ok_or_else(unavailable)?;
        let args = JobworkerpProto::json_value_to_message(args_descriptor, &input, true, true)
            .map_err(|_| unavailable())?;
        let mut request =
            super::coding_agent::make_job_request(expected.worker_id, args, Some(method));
        request.uniq_key = uniq_key.map(str::to_owned);
        request.timeout = Some(timeout_seconds.checked_mul(1000).ok_or_else(unavailable)?);
        request.overrides = overrides;
        // Enqueue the verified ID directly; the generic helper can create or
        // resolve a different same-name worker after our configuration check.
        let client = self.inner.jobworkerp_client.job_client();
        let client = before_enqueue(client, cancellation, deadline).await?;
        if cancellation.is_some_and(|token| token.is_cancelled()) {
            return Err(unavailable());
        }
        if let Some(deadline) = deadline {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or_else(unavailable)?;
            let remaining_ms = u64::try_from(remaining.as_millis()).map_err(|_| unavailable())?;
            if remaining_ms == 0 {
                return Err(unavailable());
            }
            request.timeout = Some(request.timeout.unwrap_or(remaining_ms).min(remaining_ms));
        }
        Ok(PreparedVerifiedMethodDispatch {
            client,
            request,
            result_descriptor,
        })
    }
}
