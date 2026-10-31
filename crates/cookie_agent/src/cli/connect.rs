//! `cookie connect` and `cookie disconnect`: choosing a provider, its auth
//! method, setup values, and credentials at the terminal, and connecting or
//! disconnecting it through the daemon. `cookie onboard` reuses the flow.

use std::{
    collections::BTreeMap,
    fmt,
    io::{self, IsTerminal, Write},
    sync::Arc,
};

#[cfg(all(test, unix))]
use std::sync::atomic::{AtomicUsize as TestAtomicUsize, Ordering as TestOrdering};

use anyhow::Context as _;
use cookie_agent::availability;
use cookie_agent_protocol::oauth_callback::OAuthCallbackListener;
use cookie_agent_protocol::{
    AuthMethodDescriptor, Client, ClientConnectId, ClientRequestId, EffectiveAuthState,
    ProviderConfigurationState, ProviderConnectResult, ProviderDescriptor,
    ProviderDisconnectParams, ProviderDisconnectResult, ProviderSupportState,
    RuntimeSnapshotResult, SafeCode, SafeSetupValue, parse_setup_value, setup_value_text,
};
use serde::Serialize;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::client_identity;

#[cfg(all(test, unix))]
pub(super) static SECRET_VALUES_WIPED: TestAtomicUsize = TestAtomicUsize::new(0);

#[derive(Default, Serialize)]
#[serde(transparent)]
pub(super) struct SecretValues(pub(super) BTreeMap<String, String>);

impl fmt::Debug for SecretValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValues(<redacted>)")
    }
}

impl Drop for SecretValues {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
        #[cfg(all(test, unix))]
        SECRET_VALUES_WIPED.fetch_add(1, TestOrdering::SeqCst);
    }
}

impl SecretValues {
    /// The protocol's credential map, built without leaving a plaintext
    /// copy behind: the JSON in between is zeroized.
    pub(super) fn into_credentials(
        self,
    ) -> anyhow::Result<cookie_agent_protocol::ProviderCredentialValues> {
        let serialized = Zeroizing::new(serde_json::to_vec(&self)?);
        serde_json::from_slice(&serialized).context("credentials")
    }
}

pub(super) trait ConnectIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()>;
    fn read_public(&mut self, prompt: &str) -> anyhow::Result<String>;
    fn read_secret(&mut self, prompt: &str) -> anyhow::Result<Zeroizing<String>>;
}

pub(super) struct StdioConnectIo;

impl ConnectIo for StdioConnectIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        println!("{line}");
        Ok(())
    }

    fn read_public(&mut self, prompt: &str) -> anyhow::Result<String> {
        read_public_line(prompt)
    }

    fn read_secret(&mut self, prompt: &str) -> anyhow::Result<Zeroizing<String>> {
        read_secret_line(prompt)
    }
}

async fn runtime_snapshot(client: &Client) -> anyhow::Result<RuntimeSnapshotResult> {
    client
        .runtime_snapshot()
        .await
        .context("runtime.snapshot.get failed")
}

pub(super) async fn run_connect(
    url: &str,
    token: &str,
    provider_id: Option<String>,
) -> anyhow::Result<()> {
    require_interactive_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        "connect",
    )?;
    let client = Client::connect_websocket_with_token(url, token).await?;
    let mut io = StdioConnectIo;
    run_connect_with(&client, provider_id, &mut io).await
}

pub(super) async fn run_connect_with<I: ConnectIo>(
    client: &Client,
    provider_id: Option<String>,
    io: &mut I,
) -> anyhow::Result<()> {
    client
        .handshake_as(client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    connect_provider_flow(client, provider_id, io).await
}

/// Chooses a provider (`provider_id`, else asks), its auth method, setup
/// values, and credentials, and connects it over `client`, which has made
/// its handshake.
pub(super) async fn connect_provider_flow<I: ConnectIo>(
    client: &Client,
    provider_id: Option<String>,
    io: &mut I,
) -> anyhow::Result<()> {
    let runtime = runtime_snapshot(client).await?;
    let provider = choose_provider(&runtime.snapshot.providers, provider_id, io)?;
    print_provider_details(provider, runtime.snapshot.revision.as_str(), io)?;
    ensure_supported(provider)?;
    io.write_line("Scope: durable, global (available to all workspaces).")?;
    if !matches!(
        io.read_public(connect_confirmation_prompt(provider))?
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ) {
        anyhow::bail!("provider connection cancelled");
    }

    if let Some(method) = cookie_agent_client_state::providers::sign_in_method(provider) {
        return sign_in_provider_flow(client, provider, method, io).await;
    }

    // The TUI's order: the auth method, its setup values, then credentials.
    let auth_method = choose_auth_method(provider, io)?;
    let setup_values = collect_setup_values(provider, |prompt| io.read_public(prompt))?;
    let auth_values =
        collect_auth_values(auth_method, |prompt| io.read_secret(prompt))?.into_credentials()?;
    let result: ProviderConnectResult = client
        .connect_provider_retrying(
            cookie_agent_protocol::ProviderConnectParams {
                provider_id: provider.id.clone(),
                expected_revision: runtime.snapshot.revision,
                setup_values,
                auth_method: auth_method.id.clone(),
                auth_values,
                client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string())
                    .expect("UUID is a valid connect ID"),
            },
            provider,
        )
        .await
        .context("provider.connect failed")?;
    io.write_line(&format!(
        "provider.connect succeeded for {} at catalog revision {}{}.",
        result.durable_connection.provider_id,
        result.runtime.revision,
        if result.replayed { " (replayed)" } else { "" }
    ))?;
    Ok(())
}

/// A provider's browser sign-in. The redirect returns to a listener on this
/// machine; when the browser runs elsewhere, the user pastes the address it
/// could not load instead.
async fn sign_in_provider_flow<I: ConnectIo>(
    client: &Client,
    provider: &cookie_agent_protocol::ProviderDescriptor,
    method: &cookie_agent_protocol::SignInMethodDescriptor,
    io: &mut I,
) -> anyhow::Result<()> {
    let listener = OAuthCallbackListener::bind_on_path(method.redirect_path.as_str())
        .await
        .context("listen for the sign-in redirect")?;
    let redirect_uri = listener.redirect_uri();
    let port = redirect_uri
        .trim_start_matches("http://127.0.0.1:")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let began = client
        .begin_provider_auth(cookie_agent_protocol::ProviderAuthBeginParams {
            provider_id: provider.id.clone(),
            method: method.id.clone(),
            redirect_uri: Some(redirect_uri),
        })
        .await
        .context("provider.auth.begin failed")?;
    io.write_line(&format!(
        "{}: open this address in a browser and sign in:\n\n{}\n",
        method.display_name, began.authorization_url
    ))?;
    io.write_line(&format!(
        "{} To catch the redirect from another machine instead, forward its port first: \
         ssh -N -L {port}:127.0.0.1:{port} <this machine>",
        cookie_agent_client_state::providers::SIGN_IN_PASTE_HINT
    ))?;
    let caught = Arc::new(std::sync::Mutex::new(None));
    let background = {
        let client = client.clone();
        let caught = Arc::clone(&caught);
        let provider_id = provider.id.clone();
        tokio::spawn(async move {
            while let Ok(callback) = listener.accept().await {
                let result = client
                    .complete_provider_auth(cookie_agent_protocol::ProviderAuthCompleteParams {
                        provider_id: provider_id.clone(),
                        callback_url: callback.url().to_owned(),
                    })
                    .await;
                let _ = callback.respond(result.is_ok()).await;
                let done = result.is_ok();
                match &result {
                    Ok(_) => println!("\nSigned in. Press Enter to finish."),
                    Err(error) => println!("\nThe sign-in failed: {error}"),
                }
                *caught
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
                if done {
                    return;
                }
            }
        })
    };
    let outcome = loop {
        let line = io.read_public(
            "Paste the address the browser landed on, or press Enter once it says signed in: ",
        )?;
        let line = line.trim();
        if line.is_empty() {
            let finished = caught
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            match finished {
                Some(Ok(result)) => break result,
                Some(Err(error)) => io.write_line(&format!("The sign-in failed: {error}"))?,
                None => io.write_line("Still waiting for the browser.")?,
            }
            continue;
        }
        match client
            .complete_provider_auth(cookie_agent_protocol::ProviderAuthCompleteParams {
                provider_id: provider.id.clone(),
                callback_url: line.to_owned(),
            })
            .await
        {
            Ok(result) => break result,
            Err(error) => io.write_line(&format!("The sign-in failed: {error}"))?,
        }
    };
    background.abort();
    io.write_line(&format!(
        "Signed in to {}{}.",
        outcome.provider_id,
        outcome
            .account
            .email
            .as_ref()
            .map(|email| format!(" as {email}"))
            .unwrap_or_default()
    ))?;
    Ok(())
}

pub(super) async fn run_disconnect(
    url: &str,
    token: &str,
    provider_id: Option<String>,
) -> anyhow::Result<()> {
    require_interactive_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        "disconnect",
    )?;
    let client = Client::connect_websocket_with_token(url, token).await?;
    let mut io = StdioConnectIo;
    client
        .handshake_as(client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    let runtime = runtime_snapshot(&client).await?;
    let provider = choose_provider(&runtime.snapshot.providers, provider_id, &mut io)?;
    print_provider_details(provider, runtime.snapshot.revision.as_str(), &mut io)?;
    println!("Scope: durable, global (available to all workspaces).");
    if !prompt_confirmation("Disconnect this managed provider? [y/N] ")? {
        anyhow::bail!("provider disconnection cancelled");
    }
    let result: ProviderDisconnectResult = client
        .disconnect_provider(ProviderDisconnectParams {
            provider_id: provider.id.clone(),
            expected_revision: runtime.snapshot.revision,
            expected_connection_generation: provider
                .durable_connection
                .as_ref()
                .map(|connection| connection.connection_generation),
            client_request_id: ClientRequestId::new(Uuid::now_v7().to_string())
                .expect("UUID is a valid request ID"),
        })
        .await
        .context("provider.disconnect failed")?;
    println!(
        "provider.disconnect succeeded for {} at catalog revision {}{}.",
        result.provider_id,
        result.runtime.snapshot.revision,
        if result.replayed { " (replayed)" } else { "" }
    );
    Ok(())
}

pub(super) fn ensure_supported(provider: &ProviderDescriptor) -> anyhow::Result<()> {
    if provider.support.state != ProviderSupportState::Supported {
        let reason = provider
            .support
            .reason
            .as_ref()
            .map_or("unspecified", SafeCode::as_str);
        anyhow::bail!("provider is not connectable: {reason}");
    }
    Ok(())
}

pub(super) fn connect_confirmation_prompt(provider: &ProviderDescriptor) -> &'static str {
    if matches!(
        provider.configuration,
        ProviderConfigurationState::Authored | ProviderConfigurationState::AuthoredAndStored
    ) && provider.effective_auth_state == EffectiveAuthState::Unavailable
    {
        "Complete setup and authentication for this authored provider? [y/N] "
    } else if provider.durable_connection.is_some() {
        "Reconnect or update this provider? [y/N] "
    } else {
        "Connect this provider? [y/N] "
    }
}

pub(super) fn collect_setup_values(
    provider: &ProviderDescriptor,
    mut read: impl FnMut(&str) -> anyhow::Result<String>,
) -> anyhow::Result<BTreeMap<cookie_agent_protocol::SetupFieldId, SafeSetupValue>> {
    let stored = provider
        .durable_connection
        .as_ref()
        .map(|connection| &connection.setup_values);
    provider
        .setup_fields
        .iter()
        .filter_map(|field| {
            let existing = stored.and_then(|values| values.get(&field.id));
            let fallback = existing.or(field.default.as_ref());
            let prompt = if let Some(value) = fallback {
                format!("{} [{}]: ", field.display_name, setup_value_text(value))
            } else {
                format!("{}: ", field.display_name)
            };
            let answer = match read(&prompt) {
                Ok(answer) => answer,
                Err(error) => return Some(Err(error)),
            };
            if answer.is_empty() {
                return fallback
                    .cloned()
                    .map(|value| Ok((field.id.clone(), value)))
                    .or_else(|| {
                        field.required.then(|| {
                            Err(anyhow::anyhow!(
                                "required setup field `{}` was blank",
                                field.id
                            ))
                        })
                    });
            }
            Some(
                parse_setup_value(field, &answer)
                    .map_err(anyhow::Error::from)
                    .map(|value| (field.id.clone(), value)),
            )
        })
        .collect()
}

fn choose_auth_method<'a>(
    provider: &'a ProviderDescriptor,
    io: &mut impl ConnectIo,
) -> anyhow::Result<&'a AuthMethodDescriptor> {
    if provider.auth_methods.is_empty() {
        anyhow::bail!("provider has no supported authentication method");
    }
    if provider.auth_methods.len() == 1 {
        return Ok(&provider.auth_methods[0]);
    }
    io.write_line("Authentication methods:")?;
    for (index, method) in provider.auth_methods.iter().enumerate() {
        io.write_line(&format!(
            "  {}. {} ({})",
            index + 1,
            method.display_name,
            method.id
        ))?;
    }
    let answer = io.read_public("Authentication method number or ID: ")?;
    if let Ok(index) = answer.parse::<usize>()
        && let Some(method) = index
            .checked_sub(1)
            .and_then(|index| provider.auth_methods.get(index))
    {
        return Ok(method);
    }
    provider
        .auth_methods
        .iter()
        .find(|method| method.id.as_str() == answer)
        .with_context(|| format!("authentication method `{answer}` was not found"))
}

pub(super) fn collect_auth_values(
    method: &AuthMethodDescriptor,
    mut read: impl FnMut(&str) -> anyhow::Result<Zeroizing<String>>,
) -> anyhow::Result<SecretValues> {
    let mut values = SecretValues::default();
    for field in &method.credentials {
        let mut value = read(&format!(
            "{} (secret, blank does not retain): ",
            field.display_name
        ))?;
        if value.is_empty() {
            if field.required {
                anyhow::bail!("required credential field `{}` was blank", field.id);
            }
        } else {
            values
                .0
                .insert(field.id.as_str().to_owned(), std::mem::take(&mut *value));
        }
    }
    Ok(values)
}

pub(super) fn require_interactive_tty(
    stdin: bool,
    stdout: bool,
    stderr: bool,
    command: &str,
) -> anyhow::Result<()> {
    if !(stdin && stdout && stderr) {
        anyhow::bail!(
            "cookie {command} requires an interactive TTY; credentials are never accepted as command-line arguments"
        );
    }
    Ok(())
}

pub(super) fn choose_provider<'a>(
    providers: &'a [ProviderDescriptor],
    requested: Option<String>,
    io: &mut impl ConnectIo,
) -> anyhow::Result<&'a ProviderDescriptor> {
    if let Some(requested) = requested {
        return providers
            .iter()
            .find(|provider| provider.id.as_str() == requested)
            .with_context(|| format!("runtime provider `{requested}` was not found"));
    }
    if providers.is_empty() {
        anyhow::bail!("the runtime has no providers");
    }
    io.write_line("Runtime providers:")?;
    for (index, provider) in providers.iter().enumerate() {
        let support = provider.support.reason.as_ref().map_or_else(
            || format!("{:?}", provider.support.state).to_ascii_lowercase(),
            |reason| format!("{:?}: {}", provider.support.state, reason).to_ascii_lowercase(),
        );
        io.write_line(&format!(
            "  {}. {} ({}) — {} · models: {}",
            index + 1,
            provider.display_name,
            provider.id,
            support,
            availability::model_counts_text(&provider.model_counts)
        ))?;
    }
    let answer = io.read_public("Provider number or ID: ")?;
    if let Ok(index) = answer.parse::<usize>()
        && let Some(provider) = index.checked_sub(1).and_then(|index| providers.get(index))
    {
        return Ok(provider);
    }
    providers
        .iter()
        .find(|provider| provider.id.as_str() == answer)
        .with_context(|| format!("runtime provider `{answer}` was not found"))
}

pub(super) fn print_provider_details(
    provider: &ProviderDescriptor,
    revision: &str,
    io: &mut impl ConnectIo,
) -> anyhow::Result<()> {
    io.write_line(&format!("Provider ID: {}", provider.id))?;
    io.write_line(&format!("Name: {}", provider.display_name))?;
    io.write_line(&format!("Catalog revision: {revision}"))?;
    io.write_line(&format!("Presence: {:?}", provider.presence))?;
    io.write_line(&format!("Support: {:?}", provider.support.state))?;
    if let Some(reason) = &provider.support.reason {
        io.write_line(&format!("Support reason: {reason}"))?;
    }
    io.write_line(&format!("Configuration: {:?}", provider.configuration))?;
    io.write_line(&format!(
        "Models: {}",
        availability::model_counts_text(&provider.model_counts)
    ))?;
    for model in &provider.unavailable_models {
        io.write_line(&format!(
            "  {} — {}",
            model.id,
            availability::unavailable_model_text(model)
        ))?;
    }
    if let Some(url) = &provider.documentation_url {
        io.write_line(&format!("Docs: {url}"))?;
    }
    if let Some(hint) = availability::env_config_hint(provider) {
        io.write_line(&format!("Config alternative: {hint}"))?;
    }
    Ok(())
}

fn read_public_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("flush prompt")?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).context("read input")?;
    Ok(answer.trim().to_owned())
}

fn prompt_confirmation(prompt: &str) -> anyhow::Result<bool> {
    Ok(matches!(
        read_public_line(prompt)?.to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(unix)]
fn read_secret_line(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    struct EchoGuard(libc::termios);
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            // SAFETY: stdin remains a valid process file descriptor.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.0);
            }
        }
    }

    print!("{prompt}");
    io::stdout().flush().context("flush credential prompt")?;
    // SAFETY: initialized by tcgetattr before use.
    let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
    // SAFETY: connect verifies stdin is a TTY.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
        return Err(io::Error::last_os_error()).context("disable credential echo");
    }
    let guard = EchoGuard(original);
    let mut hidden = original;
    hidden.c_lflag &= !libc::ECHO;
    // SAFETY: both termios values are initialized.
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &hidden) } != 0 {
        return Err(io::Error::last_os_error()).context("disable credential echo");
    }
    let mut value = String::new();
    let read = io::stdin().read_line(&mut value).context("read credential");
    drop(guard);
    println!();
    read?;
    while matches!(value.as_bytes().last(), Some(b'\n' | b'\r')) {
        value.pop();
    }
    Ok(Zeroizing::new(value))
}

#[cfg(windows)]
fn read_secret_line(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        },
    };

    struct ConsoleModeGuard {
        handle: HANDLE,
        mode: u32,
    }
    impl Drop for ConsoleModeGuard {
        fn drop(&mut self) {
            // SAFETY: the console handle and original mode remain valid for this process.
            unsafe {
                SetConsoleMode(self.handle, self.mode);
            }
        }
    }

    print!("{prompt}");
    io::stdout().flush().context("flush credential prompt")?;
    // SAFETY: GetStdHandle requires no caller-owned pointers.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("open credential console");
    }
    let mut original = 0;
    // SAFETY: original points to writable mode storage.
    if unsafe { GetConsoleMode(handle, &mut original) } == 0 {
        return Err(io::Error::last_os_error()).context("read credential console mode");
    }
    // SAFETY: handle is a console input handle and the mode only disables echo.
    if unsafe { SetConsoleMode(handle, original & !ENABLE_ECHO_INPUT) } == 0 {
        return Err(io::Error::last_os_error()).context("disable credential echo");
    }
    let guard = ConsoleModeGuard {
        handle,
        mode: original,
    };
    let mut value = String::new();
    let read = io::stdin().read_line(&mut value).context("read credential");
    drop(guard);
    println!();
    read?;
    while matches!(value.as_bytes().last(), Some(b'\n' | b'\r')) {
        value.pop();
    }
    Ok(Zeroizing::new(value))
}
