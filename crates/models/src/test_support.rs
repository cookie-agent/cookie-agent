use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use futures_util::{StreamExt as _, stream};
use oven_sdk::{
    AbortSignal, BoxFuture, LanguageModel, LanguageModelDescriptor, ModelError, Request,
    StreamPart, StreamResponse,
};

#[derive(Clone, Debug)]
pub enum ScriptedStep {
    Stream(Vec<Result<StreamPart, ModelError>>),
    Error(ModelError),
}

impl ScriptedStep {
    pub fn stream(items: impl IntoIterator<Item = Result<StreamPart, ModelError>>) -> Self {
        Self::Stream(items.into_iter().collect())
    }

    #[must_use]
    pub fn error(error: ModelError) -> Self {
        Self::Error(error)
    }
}

#[derive(Clone)]
pub struct ScriptedModel {
    descriptor: LanguageModelDescriptor,
    steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl ScriptedModel {
    #[must_use]
    pub fn new(
        descriptor: LanguageModelDescriptor,
        steps: impl IntoIterator<Item = ScriptedStep>,
    ) -> Self {
        Self {
            descriptor,
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl LanguageModel for ScriptedModel {
    fn descriptor(&self) -> LanguageModelDescriptor {
        self.descriptor.clone()
    }

    fn stream<'a>(
        &'a self,
        request: Request,
        abort: AbortSignal,
    ) -> BoxFuture<'a, Result<StreamResponse, ModelError>> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        let step = self
            .steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        Box::pin(async move {
            if abort.is_aborted() {
                return Err(ModelError::abort("scripted model call was aborted"));
            }
            match step {
                Some(ScriptedStep::Stream(items)) => {
                    Ok(StreamResponse::new(stream::iter(items).boxed()))
                }
                Some(ScriptedStep::Error(error)) => Err(error),
                None => Err(ModelError::invalid_request(
                    "scripted model has no remaining step",
                )),
            }
        })
    }
}
