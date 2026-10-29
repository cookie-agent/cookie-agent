//! The built-in ChatGPT provider: its sign-in, its provider-store
//! connection, and the plan's model list.
//!
//! The sign-in lives in [`ChatGptCredentials`], beside the provider store.
//! The store holds only the signed-in account (`chatgpt-sign-in-v1` with the
//! account's subject), so a token refresh never recompiles the runtime; the
//! account's state and the model list do, through the model manager.

use std::{sync::Arc, time::Duration};

use cookie_agent_models::chatgpt::{
    ChatGptCredentials, ChatGptError, KEEPALIVE_INTERVAL, PROVIDER_ID, REDIRECT_PATH,
};
use cookie_agent_protocol::{
    ProviderAuthBeginParams, ProviderAuthBeginResult, ProviderAuthCancelParams,
    ProviderAuthCancelResult, ProviderAuthCompleteParams, ProviderAuthCompleteResult,
    ProviderAuthCompletedNotification, ProviderId, RuntimeChangeReason, SafeDisplayText,
    SignInAccountDescriptor,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{Engine, EngineError, on_blocking_pool};

/// How often the model list is fetched again and the sign-in kept alive.
pub const CHATGPT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How long the daemon's own listener waits for the browser.
const LISTENER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The most of a callback request the listener reads.
const CALLBACK_REQUEST_LIMIT: usize = 16 * 1024;

fn sign_in_error(provider_id: &ProviderId, reason: impl std::fmt::Display) -> EngineError {
    EngineError::ProviderSignIn {
        provider_id: provider_id.clone(),
        reason: reason.to_string(),
    }
}

impl Engine {
    fn chatgpt_for(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Arc<ChatGptCredentials>, EngineError> {
        if provider_id.as_str() != PROVIDER_ID {
            return Err(sign_in_error(
                provider_id,
                "sign_in_unsupported: the provider has no sign-in",
            ));
        }
        self.inner.model_manager.chatgpt().cloned().ok_or_else(|| {
            sign_in_error(
                provider_id,
                "sign_in_unsupported: this daemon has no ChatGPT sign-in",
            )
        })
    }

    /// Starts a provider's sign-in. Without a `redirect_uri` the daemon
    /// catches the browser's redirect itself and announces the outcome with
    /// `provider.auth.completed`.
    pub async fn begin_provider_auth(
        &self,
        params: ProviderAuthBeginParams,
    ) -> Result<ProviderAuthBeginResult, EngineError> {
        let credentials = self.chatgpt_for(&params.provider_id)?;
        if params.method.as_str() != cookie_agent_models::recipes::CHATGPT_SIGN_IN_METHOD {
            return Err(sign_in_error(
                &params.provider_id,
                format!(
                    "sign_in_method_unknown: the provider offers no sign-in `{}`",
                    params.method
                ),
            ));
        }
        self.stop_sign_in_listener();
        let Some(redirect_uri) = params.redirect_uri else {
            return self
                .begin_with_daemon_listener(credentials, params.provider_id)
                .await;
        };
        let authorization_url = credentials
            .begin_sign_in(&redirect_uri)
            .map_err(|error| sign_in_error(&params.provider_id, error))?;
        Ok(ProviderAuthBeginResult {
            provider_id: params.provider_id,
            authorization_url,
            redirect_uri,
        })
    }

    /// Finishes a sign-in, connects the account, and fetches its models.
    pub async fn complete_provider_auth(
        &self,
        params: ProviderAuthCompleteParams,
    ) -> Result<ProviderAuthCompleteResult, EngineError> {
        let credentials = self.chatgpt_for(&params.provider_id)?;
        let signed_in = credentials
            .complete_sign_in(&params.callback_url)
            .await
            .map_err(|error| sign_in_error(&params.provider_id, error))?;
        self.connect_chatgpt_account(signed_in.subject.clone())
            .await?;
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.refresh_chatgpt_models().await {
                eprintln!(
                    "cookie-agent: could not list the ChatGPT plan's models after sign-in: {error}"
                );
            }
        });
        Ok(ProviderAuthCompleteResult {
            provider_id: params.provider_id,
            account: SignInAccountDescriptor {
                email: signed_in
                    .email
                    .as_deref()
                    .and_then(|email| SafeDisplayText::new(email).ok()),
                sign_in_required: false,
            },
        })
    }

    /// Drops a sign-in in progress, and the daemon's listener for it.
    pub fn cancel_provider_auth(
        &self,
        params: ProviderAuthCancelParams,
    ) -> Result<ProviderAuthCancelResult, EngineError> {
        let credentials = self.chatgpt_for(&params.provider_id)?;
        self.stop_sign_in_listener();
        credentials.cancel_sign_in();
        Ok(ProviderAuthCancelResult {
            provider_id: params.provider_id,
        })
    }

    /// Revokes and forgets the ChatGPT sign-in after its provider-store
    /// connection was removed.
    pub async fn sign_out_chatgpt(&self) -> Result<(), EngineError> {
        let provider_id =
            ProviderId::new(PROVIDER_ID).map_err(|_| EngineError::RuntimeCompileFailed)?;
        let credentials = self.chatgpt_for(&provider_id)?;
        credentials
            .sign_out()
            .await
            .map_err(|error| sign_in_error(&provider_id, error))?;
        self.publish_chatgpt_account().await;
        Ok(())
    }

    /// Fetches the signed-in plan's models and publishes the runtime they
    /// compile to. Does nothing without a usable sign-in.
    pub async fn refresh_chatgpt_models(&self) -> Result<(), EngineError> {
        let Some(credentials) = self.inner.model_manager.chatgpt().cloned() else {
            return Ok(());
        };
        if credentials
            .account()
            .is_none_or(|account| account.needs_sign_in)
        {
            return Ok(());
        }
        let provider_id =
            ProviderId::new(PROVIDER_ID).map_err(|_| EngineError::RuntimeCompileFailed)?;
        let models = match credentials.fetch_models().await {
            Ok(models) => models,
            // A sign-in that broke while listing is published as such.
            Err(ChatGptError::SignInRequired) => {
                self.publish_chatgpt_account().await;
                return Ok(());
            }
            Err(error) => return Err(sign_in_error(&provider_id, error)),
        };
        let engine = self.clone();
        on_blocking_pool(move || {
            let _mutation = engine
                .inner
                .runtime_mutation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = engine.current_runtime();
            let replaced =
                engine
                    .inner
                    .model_manager
                    .replace_chatgpt_models(models, |candidate| {
                        engine
                            .prepare_publication(
                                &engine.inner.config.load(),
                                candidate,
                                &previous,
                                vec![RuntimeChangeReason::CatalogRefreshed],
                            )
                            .map_err(|_| {
                                cookie_agent_models::ModelManagerError::RuntimeCompileFailed
                            })
                    })?;
            if let Some((_, publication)) = replaced {
                engine.publish(publication);
            }
            Ok(())
        })
        .await
    }

    /// Publishes the runtime when the signed-in account's state changed: a
    /// sign-in that can no longer refresh, or a sign-out.
    pub(crate) async fn publish_chatgpt_account(&self) {
        let engine = self.clone();
        let published = on_blocking_pool(move || {
            let _mutation = engine
                .inner
                .runtime_mutation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = engine.current_runtime();
            let refreshed = engine
                .inner
                .model_manager
                .refresh_chatgpt_account(|candidate| {
                    engine
                        .prepare_publication(
                            &engine.inner.config.load(),
                            candidate,
                            &previous,
                            vec![RuntimeChangeReason::ProviderStoreChanged],
                        )
                        .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
                })?;
            if let Some((_, publication)) = refreshed {
                engine.publish(publication);
            }
            Ok(())
        })
        .await;
        if let Err(error) = published {
            eprintln!("cookie-agent: could not publish the ChatGPT sign-in's state: {error}");
        }
    }

    /// Keeps the sign-in alive, refreshes the model list hourly, and
    /// publishes the account's state whenever it changes, until `shutdown`.
    pub async fn run_chatgpt_maintenance(
        self,
        shutdown: tokio_util::sync::CancellationToken,
        cadence: Duration,
    ) {
        let Some(credentials) = self.inner.model_manager.chatgpt().cloned() else {
            return;
        };
        let mut changes = credentials.subscribe();
        let mut tick = tokio::time::interval(cadence);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.publish_chatgpt_account().await;
                }
                _ = tick.tick() => {
                    if let Err(error) = credentials.keep_alive(KEEPALIVE_INTERVAL).await {
                        eprintln!("cookie-agent: could not keep the ChatGPT sign-in alive: {error}");
                    }
                    if let Err(error) = self.refresh_chatgpt_models().await {
                        eprintln!("cookie-agent: could not list the ChatGPT plan's models: {error}");
                    }
                }
            }
        }
    }

    /// Writes the account's provider-store connection and publishes it.
    async fn connect_chatgpt_account(&self, subject: String) -> Result<(), EngineError> {
        use cookie_agent_models::provider_store::{ClientConnectId, ProviderAuthValues};

        let engine = self.clone();
        on_blocking_pool(move || {
            let _mutation = engine
                .inner
                .runtime_mutation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = engine.current_runtime();
            let current = engine.inner.model_manager.current();
            let request = cookie_agent_models::ProviderConnectRequest {
                provider_id: ProviderId::new(PROVIDER_ID)
                    .map_err(|_| EngineError::RuntimeCompileFailed)?,
                expected_catalog_revision: current.catalog().revision.clone(),
                setup_values: std::collections::BTreeMap::new(),
                auth_method: cookie_agent_protocol::AuthMethodId::new(
                    cookie_agent_models::recipes::CHATGPT_SIGN_IN_METHOD,
                )
                .map_err(|_| EngineError::RuntimeCompileFailed)?,
                auth_values: ProviderAuthValues::new(std::collections::BTreeMap::from([(
                    cookie_agent_protocol::AuthFieldName::new("subject")
                        .map_err(|_| EngineError::RuntimeCompileFailed)?,
                    subject,
                )]))
                .map_err(cookie_agent_models::ModelManagerError::from)?,
                client_connect_id: ClientConnectId::new(uuid::Uuid::now_v7().to_string())
                    .map_err(cookie_agent_models::ModelManagerError::from)?,
            };
            let result = engine
                .inner
                .model_manager
                .connect(request, |candidate, _| {
                    engine
                        .prepare_publication(
                            &engine.inner.config.load(),
                            candidate,
                            &previous,
                            vec![RuntimeChangeReason::ProviderConnected],
                        )
                        .map_err(|_| cookie_agent_models::ModelManagerError::RuntimeCompileFailed)
                })?;
            if let Some(publication) = result.publication {
                engine.publish(publication);
            }
            Ok(())
        })
        .await
    }

    fn stop_sign_in_listener(&self) {
        if let Some(listener) = self
            .inner
            .chatgpt_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            listener.abort();
        }
    }

    async fn begin_with_daemon_listener(
        &self,
        credentials: Arc<ChatGptCredentials>,
        provider_id: ProviderId,
    ) -> Result<ProviderAuthBeginResult, EngineError> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| {
                sign_in_error(
                    &provider_id,
                    format!("sign_in_failed: could not listen for the redirect: {error}"),
                )
            })?;
        let port = listener
            .local_addr()
            .map_err(|_| EngineError::RuntimeCompileFailed)?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");
        let authorization_url = credentials
            .begin_sign_in(&redirect_uri)
            .map_err(|error| sign_in_error(&provider_id, error))?;
        let engine = self.clone();
        let task_provider = provider_id.clone();
        let task = tokio::spawn(async move {
            engine
                .serve_sign_in_callback(listener, port, task_provider)
                .await;
        });
        *self
            .inner
            .chatgpt_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task.abort_handle());
        Ok(ProviderAuthBeginResult {
            provider_id,
            authorization_url,
            redirect_uri,
        })
    }

    /// Waits for the browser's redirect, finishes the sign-in with it, and
    /// announces the outcome.
    async fn serve_sign_in_callback(
        &self,
        listener: tokio::net::TcpListener,
        port: u16,
        provider_id: ProviderId,
    ) {
        let deadline = tokio::time::sleep(LISTENER_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            let mut stream = tokio::select! {
                () = &mut deadline => return,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => stream,
                    Err(_) => continue,
                },
            };
            let Some(path) = read_request_path(&mut stream).await else {
                continue;
            };
            if !path.starts_with(REDIRECT_PATH) {
                let _ = respond(&mut stream, 404, "Not found.").await;
                continue;
            }
            let outcome = self
                .complete_provider_auth(ProviderAuthCompleteParams {
                    provider_id: provider_id.clone(),
                    callback_url: format!("http://127.0.0.1:{port}{path}"),
                })
                .await;
            let (page, notification) = match outcome {
                Ok(result) => (
                    "Signed in to ChatGPT. You can close this tab.".to_owned(),
                    ProviderAuthCompletedNotification {
                        provider_id: provider_id.clone(),
                        account: Some(result.account),
                        error: None,
                    },
                ),
                Err(error) => (
                    format!("The ChatGPT sign-in failed: {error}"),
                    ProviderAuthCompletedNotification {
                        provider_id: provider_id.clone(),
                        account: None,
                        error: SafeDisplayText::new(error.user_message()).ok(),
                    },
                ),
            };
            let _ = respond(&mut stream, 200, &page).await;
            let _ = self.inner.provider_auth_events.send(notification);
            return;
        }
    }

    /// Whether this daemon has the built-in ChatGPT provider.
    #[must_use]
    pub fn model_manager_chatgpt_enabled(&self) -> bool {
        self.inner.model_manager.chatgpt().is_some()
    }

    /// `provider.auth.completed` for sign-ins the daemon's listener finished.
    #[must_use]
    pub fn subscribe_provider_auth(
        &self,
    ) -> tokio::sync::broadcast::Receiver<ProviderAuthCompletedNotification> {
        self.inner.provider_auth_events.subscribe()
    }
}

/// The path and query of a `GET` request, or `None` for anything else.
async fn read_request_path(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let count = stream.read(&mut chunk).await.ok()?;
            if count == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..count]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                return Some(());
            }
            if buffer.len() > CALLBACK_REQUEST_LIMIT {
                return None;
            }
        }
    })
    .await
    .ok()
    .flatten();
    read?;
    let line = std::str::from_utf8(&buffer).ok()?.lines().next()?;
    let mut parts = line.split(' ');
    (parts.next()? == "GET")
        .then(|| parts.next().map(str::to_owned))
        .flatten()
        .filter(|path| path.starts_with('/'))
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    text: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        if status == 200 { "OK" } else { "Not Found" },
        text.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}
