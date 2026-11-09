//! App-owned terminal sessions. Feed subscriptions are disposable views and
//! never own or cancel the underlying sandbox stream.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

use super::{ExecutionIdentity, ExecutionResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    Accepted,
    Attempted,
    Unknown,
    Rejected,
}

#[async_trait]
#[allow(clippy::double_must_use)] // async_trait's boxed future is already must-use.
pub trait TerminalControl: Send + Sync {
    async fn send_input(&self, bytes: Vec<u8>) -> Result<InputDelivery, ExecutionResult>;
    async fn finish_input(&self) -> Result<InputDelivery, ExecutionResult>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptStatus {
    NotProvided,
    IntentPersisted,
    Attempted,
    Accepted,
    Unknown,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalEvent {
    Output { stream: String, data: Vec<u8> },
    Exit { exit_code: i32 },
    FeedClosed,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum TerminalError {
    #[error("terminal session is not available")]
    NotFound,
    #[error("terminal session already has an app owner")]
    AlreadyOwned,
    #[error("terminal input is too large")]
    InputTooLarge,
    #[error("terminal input operation failed")]
    InputFailed,
}

struct TerminalOwner {
    identity: ExecutionIdentity,
    control: Arc<dyn TerminalControl>,
    feed: broadcast::Sender<TerminalEvent>,
    prompt_status: Mutex<PromptStatus>,
}

#[derive(Clone, Default)]
pub struct TerminalOwnerRegistry {
    owners: Arc<Mutex<HashMap<String, Arc<TerminalOwner>>>>,
}

impl TerminalOwnerRegistry {
    pub fn install(
        &self,
        identity: ExecutionIdentity,
        control: Arc<dyn TerminalControl>,
        feed_capacity: usize,
    ) -> Result<(), TerminalError> {
        let mut owners = self.owners.lock().expect("terminal owner map poisoned");
        let key = owner_key(&identity.run_id, &identity.attempt_id);
        if owners.contains_key(&key) {
            return Err(TerminalError::AlreadyOwned);
        }
        let (feed, _) = broadcast::channel(feed_capacity.max(1));
        owners.insert(
            key,
            Arc::new(TerminalOwner {
                identity,
                control,
                feed,
                prompt_status: Mutex::new(PromptStatus::NotProvided),
            }),
        );
        Ok(())
    }

    pub fn subscribe(
        &self,
        run_id: &str,
        attempt_id: &str,
    ) -> Result<broadcast::Receiver<TerminalEvent>, TerminalError> {
        self.owner(run_id, attempt_id)
            .map(|owner| owner.feed.subscribe())
            .ok_or(TerminalError::NotFound)
    }

    pub async fn send_input(
        &self,
        run_id: &str,
        attempt_id: &str,
        bytes: Vec<u8>,
        max_input_bytes: usize,
    ) -> Result<InputDelivery, TerminalError> {
        if bytes.len() > max_input_bytes {
            return Err(TerminalError::InputTooLarge);
        }
        let owner = self
            .owner(run_id, attempt_id)
            .ok_or(TerminalError::NotFound)?;
        owner
            .control
            .send_input(bytes)
            .await
            .map_err(|_| TerminalError::InputFailed)
    }

    pub async fn finish_input(
        &self,
        run_id: &str,
        attempt_id: &str,
    ) -> Result<InputDelivery, TerminalError> {
        let owner = self
            .owner(run_id, attempt_id)
            .ok_or(TerminalError::NotFound)?;
        owner
            .control
            .finish_input()
            .await
            .map_err(|_| TerminalError::InputFailed)
    }

    pub fn publish(&self, run_id: &str, attempt_id: &str, event: TerminalEvent) {
        if let Some(owner) = self.owner(run_id, attempt_id) {
            let _ = owner.feed.send(event);
        }
    }

    pub fn prompt_status(
        &self,
        run_id: &str,
        attempt_id: &str,
    ) -> Result<PromptStatus, TerminalError> {
        let owner = self
            .owner(run_id, attempt_id)
            .ok_or(TerminalError::NotFound)?;
        Ok(owner
            .prompt_status
            .lock()
            .expect("terminal prompt status poisoned")
            .clone())
    }

    pub(crate) fn set_prompt_status(&self, run_id: &str, attempt_id: &str, status: PromptStatus) {
        if let Some(owner) = self.owner(run_id, attempt_id) {
            *owner
                .prompt_status
                .lock()
                .expect("terminal prompt status poisoned") = status;
        }
    }

    /// Remove after host-confirmed cancellation, fully verified normal finish,
    /// or application shutdown. Subscriber drops never call this method.
    pub fn remove_owner(&self, run_id: &str, attempt_id: &str) -> bool {
        self.owners
            .lock()
            .expect("terminal owner map poisoned")
            .remove(&owner_key(run_id, attempt_id))
            .is_some()
    }

    /// Remove only the exact job identity that installed this owner. A late
    /// task cleanup from an older sandbox cannot unregister a replacement.
    pub fn remove_owner_for_identity(&self, identity: &ExecutionIdentity) -> bool {
        let mut owners = self.owners.lock().expect("terminal owner map poisoned");
        let key = owner_key(&identity.run_id, &identity.attempt_id);
        if owners
            .get(&key)
            .is_some_and(|owner| owner.identity == *identity)
        {
            owners.remove(&key);
            true
        } else {
            false
        }
    }

    pub fn owner_count(&self) -> usize {
        self.owners
            .lock()
            .expect("terminal owner map poisoned")
            .len()
    }

    pub fn identity(&self, run_id: &str, attempt_id: &str) -> Option<ExecutionIdentity> {
        self.owner(run_id, attempt_id)
            .map(|owner| owner.identity.clone())
    }

    fn owner(&self, run_id: &str, attempt_id: &str) -> Option<Arc<TerminalOwner>> {
        self.owners
            .lock()
            .expect("terminal owner map poisoned")
            .get(&owner_key(run_id, attempt_id))
            .cloned()
    }
}

fn owner_key(run_id: &str, attempt_id: &str) -> String {
    format!("{run_id}:{attempt_id}")
}
