//! The plan's model list: fetched from the account, cached beside the
//! sign-in, and served as the built-in provider's catalog record.

use std::collections::BTreeMap;

use cookie_agent_identity::{ProviderId, ProviderModelId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    ChatGptCredentials, ChatGptError, DISPLAY_NAME, PROVIDER_ID,
    credentials::{HttpAnswer, user_agent},
    now_unix,
};
use crate::catalog::{
    CatalogLimits, CatalogModalities, CatalogModelEntry, CatalogModelRecord, CatalogModelStatus,
    CatalogProviderEntry, CatalogProviderRecord, CatalogReasoningOption,
};

const CACHE: &str = "chatgpt-models.json";
const CACHE_LOCK: &str = "chatgpt-models.lock";
const CACHE_LIMIT: u64 = 4 * 1024 * 1024;
/// Output limit for a model models.dev does not list: requests never send
/// one, so it only sizes the input budget.
const FALLBACK_OUTPUT_TOKENS: u64 = 128_000;
/// Codex's multi-agent effort, which token sharing refuses (`multi_agent`).
const UNSUPPORTED_EFFORTS: &[&str] = &["ultra"];

/// One model the plan lists for use through the API.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptModel {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    /// The input window Codex reports.
    pub context_window: Option<u64>,
    pub input_modalities: Vec<String>,
    /// Reasoning efforts, in the order the list gives them.
    pub efforts: Vec<String>,
    /// Codex's ordering; lower comes first.
    pub priority: Option<i64>,
}

/// A fetched model list and the account it belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatGptModelList {
    /// The subject of the account the list was fetched for.
    pub subject: String,
    pub models: Vec<ChatGptModel>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelCache {
    subject: String,
    fetched_at: u64,
    models: Vec<ChatGptModel>,
}

impl ChatGptCredentials {
    /// Lists the signed-in plan's models and caches them, with the account
    /// they were fetched for: another may have signed in by the time the
    /// list arrives. The list does not prove access; a completed request
    /// does.
    pub async fn fetch_models(&self) -> Result<ChatGptModelList, ChatGptError> {
        let subject = self
            .account()
            .filter(|account| !account.needs_sign_in)
            .ok_or(ChatGptError::SignInRequired)?
            .subject;
        let mut answer = self.list_models().await?;
        if answer.status == 401 {
            // A refused token is refreshed once, as a model request does.
            let token = self.access_token().await?;
            oven_sdk_openai::OpenAiTokenSource::rejected(self, &token);
            answer = self.list_models().await?;
        }
        if !answer.success {
            return Err(ChatGptError::Failed(format!(
                "the model list answered HTTP {}",
                answer.status
            )));
        }
        let models = parse_models(&answer.body)?;
        // An account that signed in meanwhile keeps its own cache.
        if self.is_signed_in_as(&subject) {
            let cache = ModelCache {
                subject: subject.clone(),
                fetched_at: now_unix(),
                models: models.clone(),
            };
            let bytes = serde_json::to_vec_pretty(&cache)
                .map_err(|_| ChatGptError::Failed("could not encode the model list".into()))?;
            self.directory
                .lock(CACHE_LOCK)?
                .atomic_replace(CACHE, &bytes)?;
        }
        Ok(ChatGptModelList { subject, models })
    }

    /// Whether `subject` is the signed-in account and can still refresh.
    #[must_use]
    pub fn is_signed_in_as(&self, subject: &str) -> bool {
        self.account()
            .is_some_and(|account| account.subject == subject && !account.needs_sign_in)
    }

    /// The cached list of the signed-in account, or nothing when the cache
    /// belongs to another account or is missing.
    #[must_use]
    pub fn cached_models(&self) -> Vec<ChatGptModel> {
        let Some(account) = self.account() else {
            return Vec::new();
        };
        self.directory
            .read(CACHE, CACHE_LIMIT)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<ModelCache>(&bytes).ok())
            .filter(|cache| cache.subject == account.subject)
            .map(|cache| cache.models)
            .unwrap_or_default()
    }

    async fn list_models(&self) -> Result<HttpAnswer, ChatGptError> {
        let token = self.access_token().await?;
        let response = self
            .http
            .get(format!(
                "{}/models",
                self.endpoints.api.trim_end_matches('/')
            ))
            .header("user-agent", user_agent())
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(|_| ChatGptError::Failed("could not reach api.openai.com".into()))?;
        HttpAnswer::read(response).await
    }
}

/// The models of a `GET /v1/models` answer that are listed and usable
/// through the API.
fn parse_models(body: &[u8]) -> Result<Vec<ChatGptModel>, ChatGptError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ChatGptError::Failed("the model list is not JSON".into()))?;
    let entries = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| ChatGptError::Failed("the model list has no models".into()))?;
    let text = |entry: &Value, key: &str| entry.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut models = entries
        .iter()
        .filter(|entry| entry.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter(|entry| entry.get("supported_in_api").and_then(Value::as_bool) != Some(false))
        .filter_map(|entry| {
            let slug = text(entry, "slug")?;
            ProviderModelId::new(slug.clone()).ok()?;
            Some(ChatGptModel {
                display_name: text(entry, "display_name").unwrap_or_else(|| slug.clone()),
                slug,
                description: text(entry, "description").unwrap_or_default(),
                context_window: entry.get("context_window").and_then(Value::as_u64),
                input_modalities: entry
                    .get("input_modalities")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["text".to_owned()]),
                efforts: entry
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| level.get("effort").and_then(Value::as_str))
                            .filter(|effort| !UNSUPPORTED_EFFORTS.contains(effort))
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                priority: entry.get("priority").and_then(Value::as_i64),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        (left.priority.unwrap_or(i64::MAX), &left.slug)
            .cmp(&(right.priority.unwrap_or(i64::MAX), &right.slug))
    });
    models.dedup_by(|left, right| left.slug == right.slug);
    Ok(models)
}

/// The built-in provider's catalog row for `models`. Output limits and dates
/// come from the models.dev `openai` record of the same model when it has
/// one.
#[must_use]
pub fn provider_entry(
    models: &[ChatGptModel],
    openai: Option<&CatalogProviderRecord>,
) -> CatalogProviderEntry {
    let id = ProviderId::new(PROVIDER_ID).expect("the ChatGPT provider ID is valid");
    let models = models
        .iter()
        .filter_map(|model| {
            let model_id = ProviderModelId::new(model.slug.clone()).ok()?;
            let known = openai
                .and_then(|record| record.models.get(&model_id))
                .and_then(|entry| entry.record.as_ref());
            let output = known.map_or(FALLBACK_OUTPUT_TOKENS, |record| record.limits.output);
            let input = model
                .context_window
                .or_else(|| known.and_then(|record| record.limits.input))
                .unwrap_or(272_000);
            let record = CatalogModelRecord {
                id: model_id.clone(),
                name: model.display_name.clone(),
                description: model.description.clone(),
                family: known.and_then(|record| record.family.clone()),
                attachment: model.input_modalities.iter().any(|value| value == "image"),
                reasoning: true,
                tool_call: true,
                structured_output: Some(true),
                temperature: Some(false),
                open_weights: false,
                status: CatalogModelStatus::Stable,
                release_date: known
                    .map(|record| record.release_date.clone())
                    .unwrap_or_default(),
                last_updated: known
                    .map(|record| record.last_updated.clone())
                    .unwrap_or_default(),
                modalities: CatalogModalities {
                    input: model
                        .input_modalities
                        .iter()
                        .filter(|value| matches!(value.as_str(), "text" | "image"))
                        .cloned()
                        .collect(),
                    output: vec!["text".to_owned()],
                },
                limits: CatalogLimits {
                    // Codex reports the input window; the context window holds
                    // the output too.
                    context: input + output,
                    input: Some(input),
                    output,
                },
                shape: Some("responses".to_owned()),
                provider: None,
                reasoning_options: if model.efforts.is_empty() {
                    Vec::new()
                } else {
                    vec![CatalogReasoningOption::Effort {
                        values: model.efforts.iter().cloned().map(Some).collect(),
                    }]
                },
                cost: None,
                interleaved: None,
                canonical_provenance: None,
                dynamically_loaded_tools: Some(true),
            };
            Some((
                model_id.clone(),
                CatalogModelEntry {
                    id: model_id,
                    record: Some(record),
                    quarantine: None,
                },
            ))
        })
        .collect::<BTreeMap<_, _>>();
    CatalogProviderEntry {
        id: id.clone(),
        record: Some(CatalogProviderRecord {
            id,
            name: DISPLAY_NAME.to_owned(),
            environment: Vec::new(),
            npm: crate::recipes::CHATGPT_NPM.to_owned(),
            api: Some("https://api.openai.com/v1".to_owned()),
            shape: Some("responses".to_owned()),
            documentation_url: "https://developers.openai.com/siwc".to_owned(),
            models,
        }),
        quarantine: None,
    }
}

#[cfg(test)]
pub(super) fn parse_models_for_test(body: &[u8]) -> Result<Vec<ChatGptModel>, ChatGptError> {
    parse_models(body)
}
