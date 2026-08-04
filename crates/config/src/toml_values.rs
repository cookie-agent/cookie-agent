use std::env;

use crate::ConfigError;

pub(crate) fn reject_toml_datetime(text: &str) -> Result<(), ConfigError> {
    let value = text
        .parse::<toml::Value>()
        .map_err(|error| ConfigError::Toml(safe_toml_error(&error)))?;
    fn visit(value: &toml::Value, depth: usize) -> Result<(), ConfigError> {
        if depth > 32 {
            return Err(ConfigError::TomlLimit);
        }
        match value {
            toml::Value::Datetime(_) => Err(ConfigError::TomlLimit),
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
    visit(&value, 0)
}

pub(crate) fn safe_toml_error(error: &toml::de::Error) -> String {
    let message = error.message();
    if let Some(span) = error.span() {
        format!("{message} at bytes {}..{}", span.start, span.end)
    } else {
        message.to_owned()
    }
}

pub(crate) fn interpolate_provider_values(
    value: &mut toml::Value,
    path: &mut Vec<String>,
) -> Result<(), ConfigError> {
    match value {
        toml::Value::String(text) if interpolation_allowed(path) => {
            *text = interpolate(text, path)?
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
        || matches!(path, [providers, _, headers, _] if providers == "providers" && headers == "headers")
        || matches!(path, [providers, _, auth, field] if providers == "providers" && auth == "auth" && matches!(field.as_str(), "token" | "key" | "username" | "password"))
        || matches!(path, [providers, _, auth, values, _] if providers == "providers" && auth == "auth" && values == "values")
}

fn interpolate(value: &str, path: &[String]) -> Result<String, ConfigError> {
    let mut output = String::new();
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
        let resolved = env::var_os(name)
            .ok_or_else(|| ConfigError::MissingEnvironment {
                path: path.join("."),
                variable: name.to_owned(),
            })?
            .into_string()
            .map_err(|_| ConfigError::NonUtf8Environment {
                path: path.join("."),
                variable: name.to_owned(),
            })?;
        output.push_str(&resolved);
        cursor = end + 1;
    }
    Ok(output)
}
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && (name.as_bytes()[0].is_ascii_uppercase() || name.as_bytes()[0] == b'_')
        && name
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}
