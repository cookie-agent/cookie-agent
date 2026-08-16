use std::{collections::BTreeMap, env};

use crate::ConfigError;
use cookie_agent_identity::ProviderId;
use serde::{Deserialize, Deserializer};
use zeroize::{Zeroize, Zeroizing};

pub(crate) fn validate_toml_value(value: &toml::Value) -> Result<(), ConfigError> {
    fn visit(value: &toml::Value, depth: usize) -> Result<(), ConfigError> {
        if depth > 32 {
            return Err(ConfigError::TomlLimit);
        }
        match value {
            toml::Value::Datetime(_) => Err(ConfigError::TomlLimit),
            toml::Value::Float(value) if !value.is_finite() => Err(ConfigError::TomlLimit),
            toml::Value::String(value) if value.len() > 256 * 1024 => Err(ConfigError::TomlLimit),
            toml::Value::Array(values) => {
                if values.len() > 4096 {
                    return Err(ConfigError::TomlLimit);
                }
                for value in values {
                    visit(value, depth + 1)?;
                }
                Ok(())
            }
            toml::Value::Table(values) => {
                if values.len() > 4096 {
                    return Err(ConfigError::TomlLimit);
                }
                for value in values.values() {
                    visit(value, depth + 1)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    visit(value, 0)
}

pub(crate) fn safe_toml_error(text: &str, error: &toml::de::Error) -> String {
    if let Some(span) = error.span() {
        let line = text[..span.start.min(text.len())]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        format!("malformed TOML at line {line}")
    } else {
        "configuration TOML is invalid".to_owned()
    }
}

/// A decoded TOML subtree whose owned strings are scrubbed before release.
pub(crate) struct SensitiveTomlValue(toml::Value);

impl std::fmt::Debug for SensitiveTomlValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SensitiveTomlValue(<redacted>)")
    }
}

impl SensitiveTomlValue {
    pub(crate) fn new(value: toml::Value) -> Self {
        Self(value)
    }

    pub(crate) fn value_mut(&mut self) -> &mut toml::Value {
        &mut self.0
    }

    pub(crate) fn value(&self) -> &toml::Value {
        &self.0
    }

    pub(crate) fn take(&mut self) -> toml::Value {
        std::mem::replace(&mut self.0, toml::Value::Boolean(false))
    }
}

impl<'de> Deserialize<'de> for SensitiveTomlValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        toml::Value::deserialize(deserializer).map(Self)
    }
}

impl Drop for SensitiveTomlValue {
    fn drop(&mut self) {
        zeroize_toml_value(&mut self.0);
        #[cfg(test)]
        SENSITIVE_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(crate) type SensitiveProviderValues = BTreeMap<ProviderId, SensitiveTomlValue>;

pub(crate) struct SensitiveJsonValue(serde_json::Value);

impl SensitiveJsonValue {
    pub(crate) fn from_toml(value: toml::Value) -> Self {
        fn convert(value: toml::Value) -> serde_json::Value {
            match value {
                toml::Value::String(value) => serde_json::Value::String(value),
                toml::Value::Integer(value) => serde_json::Value::Number(value.into()),
                toml::Value::Float(value) => serde_json::Value::Number(
                    serde_json::Number::from_f64(value).expect("TOML floats are finite"),
                ),
                toml::Value::Boolean(value) => serde_json::Value::Bool(value),
                toml::Value::Datetime(_) => unreachable!("TOML datetimes were rejected"),
                toml::Value::Array(values) => {
                    serde_json::Value::Array(values.into_iter().map(convert).collect())
                }
                toml::Value::Table(values) => serde_json::Value::Object(
                    values
                        .into_iter()
                        .map(|(key, value)| (key, convert(value)))
                        .collect(),
                ),
            }
        }
        Self(convert(value))
    }

    pub(crate) fn value(&self) -> &serde_json::Value {
        &self.0
    }
}

impl std::fmt::Debug for SensitiveJsonValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SensitiveJsonValue(<redacted>)")
    }
}

impl Drop for SensitiveJsonValue {
    fn drop(&mut self) {
        zeroize_json_value(&mut self.0);
        #[cfg(test)]
        SENSITIVE_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
static SENSITIVE_DROP_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn zeroize_toml_value(value: &mut toml::Value) {
    match value {
        toml::Value::String(text) => text.zeroize(),
        toml::Value::Array(values) => {
            for value in values {
                zeroize_toml_value(value);
            }
        }
        toml::Value::Table(table) => {
            let owned = std::mem::take(table);
            for (mut key, mut value) in owned {
                key.zeroize();
                zeroize_toml_value(&mut value);
            }
        }
        _ => {}
    }
}

fn zeroize_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => text.zeroize(),
        serde_json::Value::Array(values) => {
            for value in values {
                zeroize_json_value(value);
            }
        }
        serde_json::Value::Object(map) => {
            let owned = std::mem::take(map);
            for (mut key, mut value) in owned {
                key.zeroize();
                zeroize_json_value(&mut value);
            }
        }
        _ => {}
    }
}

pub(crate) fn interpolate_provider_values(
    value: &mut toml::Value,
    path: &mut Vec<String>,
) -> Result<(), ConfigError> {
    match value {
        toml::Value::String(text) if interpolation_allowed(path) => {
            let replacement = interpolate(text, path)?;
            text.zeroize();
            *text = replacement;
        }
        toml::Value::String(text) if text.contains("${env:") => {
            return Err(ConfigError::Interpolation(path.join(".")));
        }
        toml::Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                path.push(index.to_string());
                interpolate_provider_values(value, path)?;
                path.pop();
            }
        }
        toml::Value::Table(values) => {
            for (key, value) in values {
                path.push(key.clone());
                interpolate_provider_values(value, path)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

fn interpolation_allowed(path: &[String]) -> bool {
    matches!(path, [providers, _, endpoint] if providers == "providers" && endpoint == "endpoint")
        || matches!(path, [providers, _, base_url] if providers == "providers" && base_url == "base_url")
        || matches!(path, [providers, _, setup, _] if providers == "providers" && setup == "setup")
        || matches!(path, [providers, _, api_key] if providers == "providers" && api_key == "api_key")
        || matches!(path, [providers, _, auth_override, values, _] if providers == "providers" && auth_override == "auth_override" && values == "values")
        || matches!(path, [providers, _, auth, values, _] if providers == "providers" && auth == "auth" && values == "values")
}

fn interpolate(value: &str, path: &[String]) -> Result<String, ConfigError> {
    let mut output = Zeroizing::new(String::new());
    let mut cursor = 0;
    let bytes = value.as_bytes();
    while cursor < bytes.len() {
        if bytes[cursor] != b'$' {
            output.push(value[cursor..].chars().next().expect("valid utf8"));
            cursor += value[cursor..]
                .chars()
                .next()
                .expect("valid utf8")
                .len_utf8();
            continue;
        }
        if bytes.get(cursor + 1) == Some(&b'$') {
            output.push('$');
            cursor += 2;
            continue;
        }
        if !value[cursor..].starts_with("${env:") {
            return Err(ConfigError::Interpolation(path.join(".")));
        }
        let start = cursor + 6;
        let end = value[start..]
            .find('}')
            .map(|offset| start + offset)
            .ok_or_else(|| ConfigError::Interpolation(path.join(".")))?;
        let name = &value[start..end];
        if !valid_env_name(name) {
            return Err(ConfigError::Interpolation(path.join(".")));
        }
        let resolved = environment_utf8(name, path)?;
        let resolved = Zeroizing::new(resolved);
        output.push_str(&resolved);
        cursor = end + 1;
    }
    Ok(std::mem::take(&mut *output))
}

fn environment_utf8(name: &str, path: &[String]) -> Result<String, ConfigError> {
    let value = env::var_os(name).ok_or_else(|| ConfigError::MissingEnvironment {
        path: path.join("."),
        variable: name.to_owned(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        match String::from_utf8(value.into_vec()) {
            Ok(value) => Ok(value),
            Err(error) => {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                Err(ConfigError::NonUtf8Environment {
                    path: path.join("."),
                    variable: name.to_owned(),
                })
            }
        }
    }
    #[cfg(not(unix))]
    {
        value
            .into_string()
            .map_err(|_| ConfigError::NonUtf8Environment {
                path: path.join("."),
                variable: name.to_owned(),
            })
    }
}
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && (name.as_bytes()[0].is_ascii_uppercase() || name.as_bytes()[0] == b'_')
        && name
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINEL: &str = "CONFIG_SECRET_SENTINEL_7f13c4";

    #[test]
    fn recursive_toml_zeroization_scrubs_values_and_keys() {
        let mut value: toml::Value =
            format!("secret = \"{SENTINEL}\"\n[nested]\n\"{SENTINEL}\" = [\"{SENTINEL}\"]\n")
                .parse()
                .unwrap();
        zeroize_toml_value(&mut value);
        assert!(!format!("{value:?}").contains(SENTINEL));
    }

    #[test]
    fn sensitive_intermediate_debug_is_always_redacted() {
        let value = SensitiveTomlValue::new(toml::Value::String(SENTINEL.to_owned()));
        assert!(!format!("{value:?}").contains(SENTINEL));
        let json = SensitiveJsonValue::from_toml(toml::Value::String(SENTINEL.to_owned()));
        assert!(!format!("{json:?}").contains(SENTINEL));
    }

    #[test]
    fn sensitive_intermediates_run_zeroizing_drop_paths() {
        let before = SENSITIVE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed);
        {
            let _toml = SensitiveTomlValue::new(toml::Value::String(SENTINEL.to_owned()));
            let _json = SensitiveJsonValue::from_toml(toml::Value::String(SENTINEL.to_owned()));
        }
        assert!(SENSITIVE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed) >= before + 2);
    }

    #[test]
    fn interpolation_error_scrubs_partial_output_and_does_not_echo_secret() {
        let path = vec!["providers".into(), "openai".into(), "api_key".into()];
        let error =
            interpolate(&format!("prefix-{SENTINEL}-${{env:INVALID-NAME}}"), &path).unwrap_err();
        assert!(!format!("{error:?}{error}").contains(SENTINEL));
    }
}
