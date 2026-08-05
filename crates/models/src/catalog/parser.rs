use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use cookie_agent_identity::{CanonicalModelId, CatalogRevision, ProviderId, ProviderModelId};
use serde::{
    Deserializer,
    de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest as _, Sha256};

use super::{
    CanonicalModelProvenance, CanonicalModelRecord, CatalogCacheMetaV1, CatalogClaim, CatalogError,
    CatalogLimits, CatalogModalities, CatalogModelEntry, CatalogModelProviderClaims,
    CatalogModelRecord, CatalogModelStatus, CatalogProviderClaims, CatalogProviderEntry,
    CatalogProviderRecord, CatalogQuarantineEntry, CatalogQuarantineReason, CatalogReasoningOption,
};

const MAX_DEPTH: usize = 32;
const MAX_PROVIDERS: usize = 4_096;
const MAX_PROVIDER_MODELS: usize = 65_536;
const MAX_CANONICAL_MODELS: usize = 65_536;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_STRING_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_ENV_FIELDS: usize = 64;
const MAX_REASONING_OPTIONS: usize = 64;
const MAX_METADATA_ITEMS: usize = 1_024;
const MAX_COST_TIERS: usize = 64;
const MAX_EXPERIMENTAL_MODES: usize = 256;
const MAX_RECORD_FIELDS: usize = 1_024;

pub(crate) struct ParsedCatalog {
    pub revision: CatalogRevision,
    pub providers: BTreeMap<ProviderId, CatalogProviderEntry>,
    pub canonical_models: BTreeMap<CanonicalModelId, CanonicalModelRecord>,
    pub quarantine: Vec<CatalogQuarantineEntry>,
    pub body: Arc<[u8]>,
}

pub(crate) fn parse_catalog(bytes: &[u8]) -> Result<ParsedCatalog, CatalogError> {
    let root = parse_json(bytes)?;
    let root = root
        .as_object()
        .ok_or_else(|| candidate("catalog root must be an object"))?;
    let root_fields = unique_fields(root).map_err(|_| candidate("catalog root has duplicates"))?;
    if root_fields.len() != 2
        || !root_fields.contains_key("providers")
        || !root_fields.contains_key("models")
    {
        return Err(candidate(
            "catalog root must contain exactly providers and models",
        ));
    }
    let raw_providers = root_fields["providers"]
        .as_object()
        .ok_or_else(|| candidate("catalog providers must be an object"))?;
    let raw_canonical = root_fields["models"]
        .as_object()
        .ok_or_else(|| candidate("catalog models must be an object"))?;
    if raw_providers.is_empty()
        || raw_canonical.is_empty()
        || raw_providers.len() > MAX_PROVIDERS
        || raw_canonical.len() > MAX_CANONICAL_MODELS
    {
        return Err(candidate("catalog root map bounds are invalid"));
    }
    for (_, provider) in raw_providers {
        if let Some(provider) = provider.as_object() {
            for (field, value) in provider {
                if field == "models"
                    && value
                        .as_object()
                        .is_some_and(|models| models.len() > MAX_PROVIDER_MODELS)
                {
                    return Err(candidate("catalog provider model map exceeds its limit"));
                }
            }
        }
    }

    let mut quarantine = Vec::new();
    let (canonical_models, canonical_digest) =
        parse_canonical_models(raw_canonical, &mut quarantine);
    let providers = parse_providers(raw_providers, &canonical_digest, &mut quarantine)?;
    quarantine.sort();
    let revision = CatalogRevision::new(format!("sha256:{:x}", Sha256::digest(bytes)))
        .map_err(|_| candidate("catalog revision is invalid"))?;
    Ok(ParsedCatalog {
        revision,
        providers,
        canonical_models,
        quarantine,
        body: Arc::from(bytes),
    })
}

pub(crate) fn parse_cache_meta(bytes: &[u8]) -> Result<CatalogCacheMetaV1, CatalogError> {
    parse_cache_document(bytes, "catalog cache metadata")
}

fn parse_cache_document<T>(bytes: &[u8], description: &str) -> Result<T, CatalogError>
where
    T: serde::de::DeserializeOwned,
{
    let value = parse_json(bytes).map_err(|_| {
        CatalogError::new(
            "invalid_catalog_cache_metadata",
            format!("{description} is invalid"),
        )
    })?;
    if let JsonValue::Object(object) = &value {
        ensure_unique_recursive(object).map_err(|_| {
            CatalogError::new(
                "invalid_catalog_cache_metadata",
                format!("{description} has duplicate fields"),
            )
        })?;
    } else {
        return Err(CatalogError::new(
            "invalid_catalog_cache_metadata",
            format!("{description} is not an object"),
        ));
    }
    serde_json::from_slice(&value.canonical_bytes()).map_err(|_| {
        CatalogError::new(
            "invalid_catalog_cache_metadata",
            format!("{description} is invalid"),
        )
    })
}

fn parse_providers(
    raw: &[(String, JsonValue)],
    canonical: &BTreeMap<String, (CanonicalModelId, String)>,
    quarantine: &mut Vec<CatalogQuarantineEntry>,
) -> Result<BTreeMap<ProviderId, CatalogProviderEntry>, CatalogError> {
    let duplicate = duplicate_keys(raw);
    let ambiguous = ambiguous_keys(raw);
    let mut providers = BTreeMap::new();
    for (key, value) in raw {
        let recoverable = ProviderId::new(key.clone()).ok();
        let reason = if duplicate.contains(key) {
            Some(CatalogQuarantineReason::DuplicateProviderId)
        } else if ambiguous.contains(&normalized_id(key)) {
            Some(CatalogQuarantineReason::AmbiguousProviderId)
        } else {
            None
        };
        if let Some(reason) = reason {
            quarantine_provider(quarantine, key, reason.clone());
            if let Some(id) = recoverable {
                providers.entry(id.clone()).or_insert(CatalogProviderEntry {
                    id,
                    record: None,
                    quarantine: Some(reason),
                });
            }
            continue;
        }
        let Some(id) = recoverable else {
            quarantine_provider(
                quarantine,
                key,
                CatalogQuarantineReason::InvalidCatalogProviderRecord,
            );
            continue;
        };
        match parse_provider(key, value, canonical, quarantine)? {
            Ok(record) => {
                providers.insert(
                    id.clone(),
                    CatalogProviderEntry {
                        id,
                        record: Some(record),
                        quarantine: None,
                    },
                );
            }
            Err(reason) => {
                quarantine_provider(quarantine, key, reason.clone());
                quarantine_provider_children(quarantine, key, value, reason.clone());
                providers.insert(
                    id.clone(),
                    CatalogProviderEntry {
                        id,
                        record: None,
                        quarantine: Some(reason),
                    },
                );
            }
        }
    }
    if providers.is_empty() {
        return Err(candidate("catalog has no recoverable provider identities"));
    }
    Ok(providers)
}

fn parse_provider(
    key: &str,
    value: &JsonValue,
    canonical: &BTreeMap<String, (CanonicalModelId, String)>,
    quarantine: &mut Vec<CatalogQuarantineEntry>,
) -> Result<Result<CatalogProviderRecord, CatalogQuarantineReason>, CatalogError> {
    let Some(object) = value.as_object() else {
        return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord));
    };
    let fields = match unique_fields(object) {
        Ok(fields) => fields,
        Err(()) => return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord)),
    };
    if !exact_fields(
        &fields,
        &["id", "env", "npm", "api", "shape", "name", "doc", "models"],
        &["id", "name", "doc", "models"],
    ) {
        return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord));
    }
    let Some(embedded_id) = fields["id"].as_str() else {
        return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord));
    };
    if embedded_id != key {
        return Ok(Err(CatalogQuarantineReason::ProviderIdentityMismatch));
    }
    let id = match ProviderId::new(key.to_owned()) {
        Ok(id) => id,
        Err(_) => return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord)),
    };
    let environment = match fields.get("env") {
        Some(value) => match claim_string_array(value, MAX_ENV_FIELDS) {
            Some(value) => CatalogClaim::Present(value),
            None => return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord)),
        },
        None => CatalogClaim::Absent,
    };
    let npm = match text_claim(&fields, "npm") {
        Ok(value) => value,
        Err(_) => return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord)),
    };
    let api = match text_claim(&fields, "api") {
        Ok(value) => value,
        Err(_) => return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord)),
    };
    let shape = match text_claim(&fields, "shape") {
        Ok(value) => value,
        Err(_) => return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord)),
    };
    let Some(name) = bounded_text(fields["name"]) else {
        return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord));
    };
    let Some(documentation_url) = bounded_text(fields["doc"]) else {
        return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord));
    };
    let Some(raw_models) = fields["models"].as_object() else {
        return Ok(Err(CatalogQuarantineReason::InvalidCatalogProviderRecord));
    };
    if raw_models.len() > MAX_PROVIDER_MODELS {
        return Err(candidate("catalog provider model map exceeds its limit"));
    }
    let models = parse_provider_models(key, raw_models, canonical, quarantine);
    let projected_environment = match &environment {
        CatalogClaim::Absent => Vec::new(),
        CatalogClaim::Present(value) => value.clone(),
    };
    let projected_npm = match &npm {
        CatalogClaim::Absent => String::new(),
        CatalogClaim::Present(value) => value.clone(),
    };
    let projected_api = match &api {
        CatalogClaim::Absent => None,
        CatalogClaim::Present(value) => Some(value.clone()),
    };
    let projected_shape = match &shape {
        CatalogClaim::Absent => None,
        CatalogClaim::Present(value) => Some(value.clone()),
    };
    Ok(Ok(CatalogProviderRecord {
        id,
        name,
        environment: projected_environment,
        npm: projected_npm,
        api: projected_api,
        shape: projected_shape,
        claims: CatalogProviderClaims {
            environment,
            npm,
            api,
            shape,
        },
        documentation_url,
        models,
    }))
}

fn parse_provider_models(
    provider_key: &str,
    raw: &[(String, JsonValue)],
    canonical: &BTreeMap<String, (CanonicalModelId, String)>,
    quarantine: &mut Vec<CatalogQuarantineEntry>,
) -> BTreeMap<ProviderModelId, CatalogModelEntry> {
    let duplicate = duplicate_keys(raw);
    let ambiguous = ambiguous_keys(raw);
    let mut models = BTreeMap::new();
    for (key, value) in raw {
        let recoverable = ProviderModelId::new(key.clone()).ok();
        let reason = if duplicate.contains(key) {
            Some(CatalogQuarantineReason::DuplicateProviderModelId)
        } else if ambiguous.contains(&normalized_id(key)) {
            Some(CatalogQuarantineReason::AmbiguousProviderModelId)
        } else {
            None
        };
        if let Some(reason) = reason {
            quarantine_model(quarantine, provider_key, key, reason.clone());
            if let Some(id) = recoverable {
                models.entry(id.clone()).or_insert(CatalogModelEntry {
                    id,
                    record: None,
                    quarantine: Some(reason),
                });
            }
            continue;
        }
        let Some(id) = recoverable else {
            quarantine_model(
                quarantine,
                provider_key,
                key,
                CatalogQuarantineReason::InvalidCatalogModelRecord,
            );
            continue;
        };
        match parse_model(key, value, canonical.get(key)) {
            Ok(record) => {
                models.insert(
                    id.clone(),
                    CatalogModelEntry {
                        id,
                        record: Some(record),
                        quarantine: None,
                    },
                );
            }
            Err(reason) => {
                quarantine_model(quarantine, provider_key, key, reason.clone());
                models.insert(
                    id.clone(),
                    CatalogModelEntry {
                        id,
                        record: None,
                        quarantine: Some(reason),
                    },
                );
            }
        }
    }
    models
}

fn parse_model(
    key: &str,
    value: &JsonValue,
    canonical: Option<&(CanonicalModelId, String)>,
) -> Result<CatalogModelRecord, CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    ensure_unique_recursive(object)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    const ALLOWED: &[&str] = &[
        "id",
        "name",
        "description",
        "family",
        "attachment",
        "reasoning",
        "tool_call",
        "structured_output",
        "temperature",
        "open_weights",
        "status",
        "release_date",
        "last_updated",
        "modalities",
        "limit",
        "shape",
        "provider",
        "reasoning_options",
        "cost",
        "knowledge",
        "interleaved",
        "experimental",
    ];
    const REQUIRED: &[&str] = &[
        "id",
        "name",
        "description",
        "attachment",
        "reasoning",
        "tool_call",
        "open_weights",
        "release_date",
        "last_updated",
        "modalities",
        "limit",
    ];
    if !exact_fields(&fields, ALLOWED, REQUIRED) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    if fields["id"].as_str() != Some(key) {
        return Err(CatalogQuarantineReason::ProviderModelIdentityMismatch);
    }
    let id = ProviderModelId::new(key.to_owned())
        .map_err(|_| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let name =
        bounded_text(fields["name"]).ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let description = bounded_text(fields["description"])
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let family = optional_text(&fields, "family")?;
    let attachment = required_bool(&fields, "attachment")?;
    let reasoning = required_bool(&fields, "reasoning")?;
    let tool_call = required_bool(&fields, "tool_call")?;
    let structured_output = optional_bool(&fields, "structured_output")?;
    let temperature = optional_bool(&fields, "temperature")?;
    let open_weights = required_bool(&fields, "open_weights")?;
    let status = match fields.get("status").and_then(|value| value.as_str()) {
        None => CatalogModelStatus::Stable,
        Some("alpha") => CatalogModelStatus::Alpha,
        Some("beta") => CatalogModelStatus::Beta,
        Some("deprecated") => CatalogModelStatus::Deprecated,
        Some(_) => return Err(CatalogQuarantineReason::InvalidCatalogModelRecord),
    };
    let release_date = date_text(fields["release_date"])?;
    let last_updated = date_text(fields["last_updated"])?;
    let modalities = parse_modalities(fields["modalities"])?;
    let limits = parse_limits(fields["limit"])?;
    let shape = optional_text(&fields, "shape")?;
    let provider = fields
        .get("provider")
        .map(|value| parse_provider_claims(value))
        .transpose()?;
    let reasoning_options = fields
        .get("reasoning_options")
        .map(|value| parse_reasoning_options(value))
        .transpose()?
        .unwrap_or_default();
    if reasoning != fields.contains_key("reasoning_options") {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    validate_model_ignored_fields(&fields, reasoning)?;
    let canonical_provenance = canonical.map(|(id, digest)| CanonicalModelProvenance {
        id: id.clone(),
        metadata_digest: digest.clone(),
    });
    Ok(CatalogModelRecord {
        id,
        name,
        description,
        family,
        attachment,
        reasoning,
        tool_call,
        structured_output,
        temperature,
        open_weights,
        status,
        release_date,
        last_updated,
        modalities,
        limits,
        shape,
        provider,
        reasoning_options,
        canonical_provenance,
    })
}

fn parse_canonical_models(
    raw: &[(String, JsonValue)],
    quarantine: &mut Vec<CatalogQuarantineEntry>,
) -> (
    BTreeMap<CanonicalModelId, CanonicalModelRecord>,
    BTreeMap<String, (CanonicalModelId, String)>,
) {
    let duplicate = duplicate_keys(raw);
    let ambiguous = ambiguous_keys(raw);
    let mut records = BTreeMap::new();
    let mut digests = BTreeMap::new();
    for (key, value) in raw {
        let reason = if duplicate.contains(key) {
            Some(CatalogQuarantineReason::DuplicateCanonicalModelId)
        } else if ambiguous.contains(&normalized_id(key)) {
            Some(CatalogQuarantineReason::AmbiguousCanonicalModelId)
        } else {
            None
        };
        if let Some(reason) = reason {
            quarantine_canonical(quarantine, key, reason);
            continue;
        }
        match parse_canonical(key, value) {
            Ok(record) => {
                digests.insert(
                    key.clone(),
                    (record.id.clone(), record.metadata_digest.clone()),
                );
                records.insert(record.id.clone(), record);
            }
            Err(reason) => quarantine_canonical(quarantine, key, reason),
        }
    }
    (records, digests)
}

fn parse_canonical(
    key: &str,
    value: &JsonValue,
) -> Result<CanonicalModelRecord, CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    ensure_unique_recursive(object)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    const ALLOWED: &[&str] = &[
        "id",
        "name",
        "description",
        "family",
        "attachment",
        "reasoning",
        "tool_call",
        "structured_output",
        "temperature",
        "open_weights",
        "release_date",
        "last_updated",
        "modalities",
        "limit",
        "knowledge",
        "benchmarks",
        "weights",
        "license",
        "links",
    ];
    const REQUIRED: &[&str] = &[
        "id",
        "name",
        "description",
        "attachment",
        "reasoning",
        "tool_call",
        "open_weights",
        "release_date",
        "last_updated",
        "modalities",
        "limit",
    ];
    if !exact_fields(&fields, ALLOWED, REQUIRED) {
        return Err(CatalogQuarantineReason::InvalidCanonicalModelRecord);
    }
    if fields["id"].as_str() != Some(key) {
        return Err(CatalogQuarantineReason::CanonicalModelIdentityMismatch);
    }
    let id = CanonicalModelId::new(key.to_owned())
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    let name =
        bounded_text(fields["name"]).ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    let description = bounded_text(fields["description"])
        .ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    let family = optional_text(&fields, "family")
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    let release_date = date_text(fields["release_date"])
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    let last_updated = date_text(fields["last_updated"])
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    required_bool(&fields, "attachment")
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    required_bool(&fields, "reasoning")
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    required_bool(&fields, "tool_call")
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    required_bool(&fields, "open_weights")
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    optional_bool(&fields, "structured_output")
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    optional_bool(&fields, "temperature")
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    parse_modalities(fields["modalities"])
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    validate_canonical_limits(fields["limit"])
        .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    validate_canonical_ignored_fields(&fields)?;
    let metadata_digest = format!("sha256:{:x}", Sha256::digest(value.canonical_bytes()));
    Ok(CanonicalModelRecord {
        id,
        name,
        description,
        family,
        release_date,
        last_updated,
        metadata_digest,
    })
}

fn parse_modalities(value: &JsonValue) -> Result<CatalogModalities, CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !exact_fields(&fields, &["input", "output"], &["input", "output"]) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    let input = string_array(fields["input"], 32, false)
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let output = string_array(fields["output"], 32, false)
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    Ok(CatalogModalities { input, output })
}

fn parse_limits(value: &JsonValue) -> Result<CatalogLimits, CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !exact_fields(
        &fields,
        &["context", "input", "output"],
        &["context", "output"],
    ) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    let context = fields["context"]
        .as_u64()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let input = fields
        .get("input")
        .map(|value| {
            value
                .as_u64()
                .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)
        })
        .transpose()?;
    let output = fields["output"]
        .as_u64()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    Ok(CatalogLimits {
        context,
        input,
        output,
    })
}

fn validate_canonical_limits(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !exact_fields(&fields, &["context", "input", "output"], &["context"])
        || fields["context"].as_u64().is_none()
        || fields
            .get("input")
            .is_some_and(|value| value.as_u64().is_none())
        || fields
            .get("output")
            .is_some_and(|value| value.as_u64().is_none())
    {
        Err(CatalogQuarantineReason::InvalidCatalogModelRecord)
    } else {
        Ok(())
    }
}

fn parse_provider_claims(
    value: &JsonValue,
) -> Result<CatalogModelProviderClaims, CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !exact_fields(&fields, &["npm", "api", "shape", "body", "headers"], &[]) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    fields
        .get("body")
        .map(|value| validate_json_record(value))
        .transpose()?;
    fields
        .get("headers")
        .map(|value| validate_string_record(value))
        .transpose()?;
    let shape = optional_text(&fields, "shape")?;
    if shape
        .as_deref()
        .is_some_and(|shape| !matches!(shape, "responses" | "completions"))
    {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    Ok(CatalogModelProviderClaims {
        npm: optional_text(&fields, "npm")?,
        api: optional_text(&fields, "api")?,
        shape,
    })
}

fn parse_reasoning_options(
    value: &JsonValue,
) -> Result<Vec<CatalogReasoningOption>, CatalogQuarantineReason> {
    let values = value
        .as_array()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if values.len() > MAX_REASONING_OPTIONS {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    values
        .iter()
        .map(|value| {
            let object = value
                .as_object()
                .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
            let fields = unique_fields(object)
                .map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
            match fields.get("type").and_then(|value| value.as_str()) {
                Some("effort") => {
                    if !exact_fields(&fields, &["type", "values"], &["type", "values"]) {
                        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
                    }
                    let raw = fields["values"]
                        .as_array()
                        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
                    if raw.len() > 32 {
                        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
                    }
                    let mut seen = BTreeSet::new();
                    let mut values = Vec::with_capacity(raw.len());
                    for value in raw {
                        let value = match value {
                            JsonValue::Null => None,
                            JsonValue::String(value)
                                if matches!(
                                    value.as_str(),
                                    "none"
                                        | "minimal"
                                        | "low"
                                        | "medium"
                                        | "high"
                                        | "xhigh"
                                        | "max"
                                        | "default"
                                ) =>
                            {
                                Some(value.clone())
                            }
                            _ => return Err(CatalogQuarantineReason::InvalidCatalogModelRecord),
                        };
                        if !seen.insert(value.clone()) {
                            return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
                        }
                        values.push(value);
                    }
                    Ok(CatalogReasoningOption::Effort { values })
                }
                Some("toggle") => {
                    if !exact_fields(&fields, &["type"], &["type"]) {
                        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
                    }
                    Ok(CatalogReasoningOption::Toggle)
                }
                Some("budget_tokens") => {
                    if !exact_fields(&fields, &["type", "min", "max"], &["type"]) {
                        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
                    }
                    let min = fields
                        .get("min")
                        .map(|value| value.as_i64())
                        .transpose_value()?;
                    let max = fields
                        .get("max")
                        .map(|value| value.as_i64())
                        .transpose_value()?;
                    if min.is_some_and(|value| value < -1)
                        || max.is_some_and(|value| value < 0)
                        || matches!((min, max), (Some(min), Some(max)) if min >= 0 && min > max)
                    {
                        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
                    }
                    Ok(CatalogReasoningOption::BudgetTokens { min, max })
                }
                _ => Err(CatalogQuarantineReason::InvalidCatalogModelRecord),
            }
        })
        .collect()
}

fn validate_model_ignored_fields(
    fields: &BTreeMap<&str, &JsonValue>,
    reasoning: bool,
) -> Result<(), CatalogQuarantineReason> {
    if let Some(cost) = fields.get("cost") {
        validate_output_cost(cost, reasoning)?;
    }
    fields
        .get("knowledge")
        .map(|value| date_text(value).map(|_| ()))
        .transpose()?;
    fields
        .get("interleaved")
        .map(|value| validate_interleaved(value))
        .transpose()?;
    fields
        .get("experimental")
        .map(|value| validate_experimental(value))
        .transpose()?;
    Ok(())
}

fn validate_canonical_ignored_fields(
    fields: &BTreeMap<&str, &JsonValue>,
) -> Result<(), CatalogQuarantineReason> {
    fields
        .get("knowledge")
        .map(|value| date_text(value).map(|_| ()))
        .transpose()?;
    fields
        .get("license")
        .map(|value| {
            bounded_text(value)
                .map(|_| ())
                .ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)
        })
        .transpose()?;
    fields
        .get("links")
        .map(|value| validate_links(value))
        .transpose()?;
    fields
        .get("weights")
        .map(|value| validate_weights(value))
        .transpose()?;
    fields
        .get("benchmarks")
        .map(|value| validate_benchmarks(value))
        .transpose()?;
    Ok(())
}

fn validate_output_cost(value: &JsonValue, reasoning: bool) -> Result<(), CatalogQuarantineReason> {
    let fields = cost_fields(
        value,
        &[
            "input",
            "output",
            "reasoning",
            "cache_read",
            "cache_write",
            "input_audio",
            "output_audio",
            "context_over_200k",
            "tiers",
        ],
    )?;
    validate_base_cost(&fields)?;
    if !reasoning && fields.contains_key("reasoning") {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    fields
        .get("context_over_200k")
        .map(|value| validate_base_cost(&cost_fields(value, BASE_COST_FIELDS)?))
        .transpose()?;
    if let Some(value) = fields.get("tiers") {
        let tiers = value
            .as_array()
            .filter(|tiers| tiers.len() <= MAX_COST_TIERS)
            .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
        let mut sizes = BTreeSet::new();
        for tier in tiers {
            let fields = cost_fields(
                tier,
                &[
                    "input",
                    "output",
                    "reasoning",
                    "cache_read",
                    "cache_write",
                    "input_audio",
                    "output_audio",
                    "tier",
                ],
            )?;
            validate_base_cost(&fields)?;
            let tier = fields["tier"]
                .as_object()
                .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
            let tier = unique_fields(tier)
                .map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
            if !exact_fields(&tier, &["type", "size"], &["type", "size"])
                || tier["type"].as_str() != Some("context")
            {
                return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
            }
            let size = tier["size"]
                .as_u64()
                .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
            if !sizes.insert(size) {
                return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
            }
        }
    }
    Ok(())
}

const BASE_COST_FIELDS: &[&str] = &[
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "input_audio",
    "output_audio",
];

fn cost_fields<'a>(
    value: &'a JsonValue,
    allowed: &[&str],
) -> Result<BTreeMap<&'a str, &'a JsonValue>, CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !exact_fields(&fields, allowed, &["input", "output"]) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    Ok(fields)
}

fn validate_base_cost(fields: &BTreeMap<&str, &JsonValue>) -> Result<(), CatalogQuarantineReason> {
    for field in BASE_COST_FIELDS {
        if let Some(value) = fields.get(field)
            && !value.as_f64().is_some_and(|value| value >= 0.0)
        {
            return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
        }
    }
    Ok(())
}

fn validate_interleaved(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    if value.as_bool() == Some(true) {
        return Ok(());
    }
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if exact_fields(&fields, &["field"], &["field"])
        && matches!(
            fields["field"].as_str(),
            Some("reasoning_content" | "reasoning_details")
        )
    {
        Ok(())
    } else {
        Err(CatalogQuarantineReason::InvalidCatalogModelRecord)
    }
}

fn validate_experimental(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !exact_fields(&fields, &["modes"], &[]) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    let Some(modes) = fields.get("modes") else {
        return Ok(());
    };
    let modes = modes
        .as_object()
        .filter(|modes| modes.len() <= MAX_EXPERIMENTAL_MODES)
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    for (name, mode) in modes {
        validate_record_key(name)?;
        let mode = mode
            .as_object()
            .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
        let mode =
            unique_fields(mode).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
        if !exact_fields(&mode, &["cost", "provider"], &[]) {
            return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
        }
        mode.get("cost")
            .map(|value| validate_base_cost(&cost_fields(value, BASE_COST_FIELDS)?))
            .transpose()?;
        mode.get("provider")
            .map(|value| validate_provider_parameters(value))
            .transpose()?;
    }
    Ok(())
}

fn validate_provider_parameters(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !exact_fields(&fields, &["body", "headers"], &[]) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    fields
        .get("body")
        .map(|value| validate_json_record(value))
        .transpose()?;
    fields
        .get("headers")
        .map(|value| validate_string_record(value))
        .transpose()?;
    Ok(())
}

fn validate_json_record(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let fields = value
        .as_object()
        .filter(|fields| fields.len() <= MAX_RECORD_FIELDS)
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    for (key, _) in fields {
        validate_record_key(key)?;
    }
    Ok(())
}

fn validate_string_record(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let fields = value
        .as_object()
        .filter(|fields| fields.len() <= MAX_RECORD_FIELDS)
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    for (key, value) in fields {
        validate_record_key(key)?;
        bounded_text(value).ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    }
    Ok(())
}

fn validate_links(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let values = metadata_array(value)?;
    for value in values {
        let fields = metadata_object(value, &["label", "url", "type"], &["url"])?;
        bounded_text(fields["url"]).ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
        optional_metadata_text(&fields, "label")?;
        if fields.get("type").is_some_and(|value| {
            !matches!(
                value.as_str(),
                Some(
                    "announcement"
                        | "blog"
                        | "docs"
                        | "license"
                        | "model_card"
                        | "paper"
                        | "weights"
                        | "other"
                )
            )
        }) {
            return Err(CatalogQuarantineReason::InvalidCanonicalModelRecord);
        }
    }
    Ok(())
}

fn validate_weights(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let values = metadata_array(value)?;
    for value in values {
        let fields = metadata_object(value, &["label", "url", "format", "quantization"], &["url"])?;
        bounded_text(fields["url"]).ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
        for field in ["label", "format", "quantization"] {
            optional_metadata_text(&fields, field)?;
        }
    }
    Ok(())
}

fn validate_benchmarks(value: &JsonValue) -> Result<(), CatalogQuarantineReason> {
    let values = metadata_array(value)?;
    for value in values {
        let fields = metadata_object(
            value,
            &[
                "name", "score", "metric", "harness", "variant", "dataset", "version", "source",
                "date",
            ],
            &["name", "score"],
        )?;
        bounded_text(fields["name"]).ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
        if fields["score"].as_f64().is_none() && bounded_text(fields["score"]).is_none() {
            return Err(CatalogQuarantineReason::InvalidCanonicalModelRecord);
        }
        for field in [
            "metric", "harness", "variant", "dataset", "version", "source",
        ] {
            optional_metadata_text(&fields, field)?;
        }
        fields
            .get("date")
            .map(|value| {
                date_text(value)
                    .map(|_| ())
                    .map_err(|_| CatalogQuarantineReason::InvalidCanonicalModelRecord)
            })
            .transpose()?;
    }
    Ok(())
}

fn metadata_array(value: &JsonValue) -> Result<&[JsonValue], CatalogQuarantineReason> {
    value
        .as_array()
        .filter(|values| values.len() <= MAX_METADATA_ITEMS)
        .ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)
}

fn metadata_object<'a>(
    value: &'a JsonValue,
    allowed: &[&str],
    required: &[&str],
) -> Result<BTreeMap<&'a str, &'a JsonValue>, CatalogQuarantineReason> {
    let object = value
        .as_object()
        .ok_or(CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    let fields =
        unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCanonicalModelRecord)?;
    if exact_fields(&fields, allowed, required) {
        Ok(fields)
    } else {
        Err(CatalogQuarantineReason::InvalidCanonicalModelRecord)
    }
}

fn optional_metadata_text(
    fields: &BTreeMap<&str, &JsonValue>,
    name: &str,
) -> Result<(), CatalogQuarantineReason> {
    if fields
        .get(name)
        .is_some_and(|value| bounded_text(value).is_none())
    {
        Err(CatalogQuarantineReason::InvalidCanonicalModelRecord)
    } else {
        Ok(())
    }
}

fn validate_record_key(value: &str) -> Result<(), CatalogQuarantineReason> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        Err(CatalogQuarantineReason::InvalidCatalogModelRecord)
    } else {
        Ok(())
    }
}

trait OptionalNumberExt {
    fn transpose_value(self) -> Result<Option<i64>, CatalogQuarantineReason>;
}

impl OptionalNumberExt for Option<Option<i64>> {
    fn transpose_value(self) -> Result<Option<i64>, CatalogQuarantineReason> {
        match self {
            None => Ok(None),
            Some(Some(value)) => Ok(Some(value)),
            Some(None) => Err(CatalogQuarantineReason::InvalidCatalogModelRecord),
        }
    }
}

fn ensure_unique_recursive(object: &[(String, JsonValue)]) -> Result<(), CatalogQuarantineReason> {
    ensure_unique_recursive_except(object, "")
}

fn ensure_unique_recursive_except(
    object: &[(String, JsonValue)],
    isolated_field: &str,
) -> Result<(), CatalogQuarantineReason> {
    unique_fields(object).map_err(|()| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    for (key, value) in object {
        if key == isolated_field {
            continue;
        }
        ensure_value_unique(value, isolated_field)?;
    }
    Ok(())
}

fn ensure_value_unique(
    value: &JsonValue,
    isolated_field: &str,
) -> Result<(), CatalogQuarantineReason> {
    match value {
        JsonValue::Object(object) => ensure_unique_recursive_except(object, isolated_field),
        JsonValue::Array(values) => {
            for value in values {
                ensure_value_unique(value, isolated_field)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn required_bool(
    fields: &BTreeMap<&str, &JsonValue>,
    name: &str,
) -> Result<bool, CatalogQuarantineReason> {
    fields[name]
        .as_bool()
        .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)
}

fn optional_bool(
    fields: &BTreeMap<&str, &JsonValue>,
    name: &str,
) -> Result<Option<bool>, CatalogQuarantineReason> {
    fields
        .get(name)
        .map(|value| {
            value
                .as_bool()
                .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)
        })
        .transpose()
}

fn optional_text(
    fields: &BTreeMap<&str, &JsonValue>,
    name: &str,
) -> Result<Option<String>, CatalogQuarantineReason> {
    fields
        .get(name)
        .map(|value| bounded_text(value).ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord))
        .transpose()
}

fn text_claim(
    fields: &BTreeMap<&str, &JsonValue>,
    name: &str,
) -> Result<CatalogClaim<String>, CatalogQuarantineReason> {
    match fields.get(name) {
        Some(value) => bounded_text(value)
            .map(CatalogClaim::Present)
            .ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord),
        None => Ok(CatalogClaim::Absent),
    }
}

fn date_text(value: &JsonValue) -> Result<String, CatalogQuarantineReason> {
    let value = bounded_text(value).ok_or(CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let parts = value.split('-').collect::<Vec<_>>();
    if !matches!(parts.len(), 2 | 3)
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts.get(2).is_some_and(|day| day.len() != 2)
    {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    let year = parts[0]
        .parse::<u32>()
        .map_err(|_| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    let month = parts[1]
        .parse::<u32>()
        .map_err(|_| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
    if !(1..=12).contains(&month) {
        return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
    }
    if let Some(day) = parts.get(2) {
        let day = day
            .parse::<u32>()
            .map_err(|_| CatalogQuarantineReason::InvalidCatalogModelRecord)?;
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let maximum = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ][(month - 1) as usize];
        if day == 0 || day > maximum {
            return Err(CatalogQuarantineReason::InvalidCatalogModelRecord);
        }
    }
    Ok(value)
}

fn bounded_text(value: &JsonValue) -> Option<String> {
    let value = value.as_str()?;
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        None
    } else {
        Some(value.to_owned())
    }
}

fn string_array(value: &JsonValue, maximum: usize, require_unique: bool) -> Option<Vec<String>> {
    let values = value.as_array()?;
    if values.is_empty() || values.len() > maximum {
        return None;
    }
    let parsed = values
        .iter()
        .map(bounded_text)
        .collect::<Option<Vec<_>>>()?;
    if require_unique && parsed.iter().collect::<BTreeSet<_>>().len() != parsed.len() {
        None
    } else {
        Some(parsed)
    }
}

fn claim_string_array(value: &JsonValue, maximum: usize) -> Option<Vec<String>> {
    let values = value.as_array()?;
    if values.len() > maximum {
        return None;
    }
    let parsed = values
        .iter()
        .map(bounded_text)
        .collect::<Option<Vec<_>>>()?;
    (parsed.iter().collect::<BTreeSet<_>>().len() == parsed.len()).then_some(parsed)
}

fn exact_fields(fields: &BTreeMap<&str, &JsonValue>, allowed: &[&str], required: &[&str]) -> bool {
    fields.keys().all(|key| allowed.contains(key))
        && required.iter().all(|key| fields.contains_key(key))
}

fn duplicate_keys(raw: &[(String, JsonValue)]) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut duplicate = BTreeSet::new();
    for (key, _) in raw {
        if !seen.insert(key.clone()) {
            duplicate.insert(key.clone());
        }
    }
    duplicate
}

fn ambiguous_keys(raw: &[(String, JsonValue)]) -> BTreeSet<String> {
    let mut groups: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for (key, _) in raw {
        groups.entry(normalized_id(key)).or_default().insert(key);
    }
    groups
        .into_iter()
        .filter_map(|(normalized, exact)| (exact.len() > 1).then_some(normalized))
        .collect()
}

fn normalized_id(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn unique_fields(object: &[(String, JsonValue)]) -> Result<BTreeMap<&str, &JsonValue>, ()> {
    let mut fields = BTreeMap::new();
    for (key, value) in object {
        if fields.insert(key.as_str(), value).is_some() {
            return Err(());
        }
    }
    Ok(fields)
}

fn quarantine_provider(
    quarantine: &mut Vec<CatalogQuarantineEntry>,
    provider: &str,
    reason: CatalogQuarantineReason,
) {
    quarantine.push(CatalogQuarantineEntry {
        provider_id: Some(provider.to_owned()),
        model_id: None,
        canonical_model_id: None,
        reason,
    });
}

fn quarantine_model(
    quarantine: &mut Vec<CatalogQuarantineEntry>,
    provider: &str,
    model: &str,
    reason: CatalogQuarantineReason,
) {
    quarantine.push(CatalogQuarantineEntry {
        provider_id: Some(provider.to_owned()),
        model_id: Some(model.to_owned()),
        canonical_model_id: None,
        reason,
    });
}

fn quarantine_provider_children(
    quarantine: &mut Vec<CatalogQuarantineEntry>,
    provider: &str,
    value: &JsonValue,
    reason: CatalogQuarantineReason,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (field, value) in object {
        if field == "models"
            && let Some(models) = value.as_object()
        {
            for (model, _) in models {
                quarantine_model(quarantine, provider, model, reason.clone());
            }
        }
    }
}

fn quarantine_canonical(
    quarantine: &mut Vec<CatalogQuarantineEntry>,
    model: &str,
    reason: CatalogQuarantineReason,
) {
    quarantine.push(CatalogQuarantineEntry {
        provider_id: None,
        model_id: None,
        canonical_model_id: Some(model.to_owned()),
        reason,
    });
}

fn candidate(message: &'static str) -> CatalogError {
    CatalogError::new("invalid_catalog_candidate", message)
}

#[derive(Clone, Debug)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn as_object(&self) -> Option<&[(String, Self)]> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => value.as_u64(),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(value) => value.as_i64(),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => value.as_f64(),
            _ => None,
        }
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        fn value(raw: &JsonValue) -> serde_json::Value {
            match raw {
                JsonValue::Null => serde_json::Value::Null,
                JsonValue::Bool(value) => serde_json::Value::Bool(*value),
                JsonValue::Number(value) => serde_json::Value::Number(value.clone()),
                JsonValue::String(value) => serde_json::Value::String(value.clone()),
                JsonValue::Array(values) => {
                    serde_json::Value::Array(values.iter().map(value).collect())
                }
                JsonValue::Object(fields) => serde_json::Value::Object(
                    fields
                        .iter()
                        .map(|(key, raw)| (key.clone(), value(raw)))
                        .collect(),
                ),
            }
        }
        serde_json::to_vec(&value(self)).expect("JSON AST is serializable")
    }
}

struct ParseState {
    entries: usize,
}

struct JsonSeed<'a> {
    depth: usize,
    state: &'a std::cell::RefCell<ParseState>,
}

impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = JsonValue;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.depth > MAX_DEPTH {
            return Err(D::Error::custom("JSON depth limit exceeded"));
        }
        deserializer.deserialize_any(JsonVisitor {
            depth: self.depth,
            state: self.state,
        })
    }
}

struct JsonVisitor<'a> {
    depth: usize,
    state: &'a std::cell::RefCell<ParseState>,
}

impl<'de> Visitor<'de> for JsonVisitor<'_> {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(JsonValue::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_string(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAX_STRING_BYTES {
            Err(E::custom("JSON string limit exceeded"))
        } else {
            Ok(JsonValue::String(value))
        }
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(JsonSeed {
            depth: self.depth + 1,
            state: self.state,
        })? {
            count_entry::<A::Error>(self.state)?;
            values.push(value);
        }
        Ok(JsonValue::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.len() > MAX_STRING_BYTES {
                return Err(A::Error::custom("JSON string limit exceeded"));
            }
            let value = map.next_value_seed(JsonSeed {
                depth: self.depth + 1,
                state: self.state,
            })?;
            count_entry::<A::Error>(self.state)?;
            fields.push((key, value));
        }
        Ok(JsonValue::Object(fields))
    }
}

fn count_entry<E: serde::de::Error>(state: &std::cell::RefCell<ParseState>) -> Result<(), E> {
    let mut state = state.borrow_mut();
    state.entries = state.entries.saturating_add(1);
    if state.entries > MAX_ENTRIES {
        Err(E::custom("JSON entry limit exceeded"))
    } else {
        Ok(())
    }
}

fn parse_json(bytes: &[u8]) -> Result<JsonValue, CatalogError> {
    let state = std::cell::RefCell::new(ParseState { entries: 0 });
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = JsonSeed {
        depth: 0,
        state: &state,
    }
    .deserialize(&mut deserializer)
    .map_err(|_| candidate("catalog JSON is invalid or exceeds limits"))?;
    deserializer
        .end()
        .map_err(|_| candidate("catalog JSON has trailing data"))?;
    Ok(value)
}
