use std::collections::BTreeMap;

use cookie_agent_identity::SetupFieldId;
use serde::Serialize;

use crate::authoring::{BoundedSetupString, SafeSetupValue};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupFieldType {
    String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SetupFieldRecipe {
    pub id: &'static str,
    pub value_type: SetupFieldType,
    pub required: bool,
    pub default: Option<&'static str>,
    pub environment_alias: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SetupRecipe {
    pub id: &'static str,
    pub fields: &'static [SetupFieldRecipe],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointPolicy {
    DefaultWithAuthoredHttpsOverride { default: &'static str },
    VertexPublisher,
    BedrockRegional,
    AzureOpenai,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidatedSetup {
    pub recipe_id: &'static str,
    pub values: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SetupValidationError {
    #[error("invalid_setup_shape")]
    Shape,
    #[error("invalid_setup_value")]
    Value,
}

pub fn validate_setup(
    recipe: &SetupRecipe,
    input: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> Result<ValidatedSetup, SetupValidationError> {
    let mut values = BTreeMap::new();
    for (id, value) in input {
        let field = recipe
            .fields
            .iter()
            .find(|field| field.id == id.as_str())
            .ok_or(SetupValidationError::Shape)?;
        let value = setup_string(value).ok_or(SetupValidationError::Value)?;
        validate_field(field.id, value)?;
        values.insert(field.id.to_owned(), value.to_owned());
    }
    for field in recipe.fields {
        if !values.contains_key(field.id) {
            if let Some(default) = field.default {
                values.insert(field.id.to_owned(), default.to_owned());
            } else if field.required {
                return Err(SetupValidationError::Shape);
            }
        }
    }
    Ok(ValidatedSetup {
        recipe_id: recipe.id,
        values,
    })
}

fn setup_string(value: &SafeSetupValue) -> Option<&str> {
    match value {
        SafeSetupValue::String(value) => Some(BoundedSetupString::as_str(value)),
        SafeSetupValue::Code(value) => Some(value.as_str()),
        SafeSetupValue::Integer(_) | SafeSetupValue::Bool(_) => None,
    }
}

fn validate_field(id: &str, value: &str) -> Result<(), SetupValidationError> {
    let bounded = !value.is_empty() && value.len() <= 256 && value.is_ascii();
    let lower_token = bounded
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    let valid = match id {
        "project" => lower_token && value.as_bytes()[0].is_ascii_alphanumeric(),
        "location" | "region" | "resource_name" | "deployment" => lower_token,
        "model" | "version" | "deployment_type" => bounded && !value.chars().any(char::is_control),
        "resource" => value == "publishers/google",
        "api_version" => {
            value.len() == 10
                && value.as_bytes()[4] == b'-'
                && value.as_bytes()[7] == b'-'
                && value
                    .bytes()
                    .enumerate()
                    .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SetupValidationError::Value)
    }
}
