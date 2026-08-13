use std::fmt;

use crate::{BoundedSetupString, SafeCode, SafeSetupValue, SetupFieldDescriptor, SetupFieldType};

/// An error returned when text does not satisfy a setup field descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupValueParseError {
    message: String,
}

impl SetupValueParseError {
    fn new(field: &SetupFieldDescriptor, message: impl fmt::Display) -> Self {
        Self {
            message: format!("setup field `{}` {message}", field.id),
        }
    }
}

impl fmt::Display for SetupValueParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SetupValueParseError {}

/// Parses text according to a setup field's type and validation constraints.
///
/// Boolean input is case-insensitive and accepts `true`, `yes`, `y`, and `1`
/// for true and `false`, `no`, `n`, and `0` for false.
pub fn parse_setup_value(
    field: &SetupFieldDescriptor,
    raw: &str,
) -> Result<SafeSetupValue, SetupValueParseError> {
    let length = u32::try_from(raw.len()).unwrap_or(u32::MAX);
    if field
        .validation
        .min_length
        .is_some_and(|minimum| length < minimum)
    {
        return Err(SetupValueParseError::new(
            field,
            "is shorter than the allowed minimum",
        ));
    }
    if field
        .validation
        .max_length
        .is_some_and(|maximum| length > maximum)
    {
        return Err(SetupValueParseError::new(
            field,
            "is longer than the allowed maximum",
        ));
    }

    match field.validation.value_type {
        SetupFieldType::String => BoundedSetupString::new(raw.to_owned())
            .map(SafeSetupValue::String)
            .map_err(|error| SetupValueParseError::new(field, error)),
        SetupFieldType::Code => SafeCode::new(raw.to_owned())
            .map(SafeSetupValue::Code)
            .map_err(|error| SetupValueParseError::new(field, error)),
        SetupFieldType::Integer => {
            let value = raw
                .parse::<i64>()
                .map_err(|_| SetupValueParseError::new(field, "must be an integer"))?;
            if field
                .validation
                .minimum
                .is_some_and(|minimum| value < minimum)
                || field
                    .validation
                    .maximum
                    .is_some_and(|maximum| value > maximum)
            {
                return Err(SetupValueParseError::new(
                    field,
                    "is outside the allowed range",
                ));
            }
            Ok(SafeSetupValue::Integer(value))
        }
        SetupFieldType::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => Ok(SafeSetupValue::Bool(true)),
            "false" | "no" | "n" | "0" => Ok(SafeSetupValue::Bool(false)),
            _ => Err(SetupValueParseError::new(field, "must be true or false")),
        },
    }
}

/// Formats a setup value for editing or display.
#[must_use]
pub fn setup_value_text(value: &SafeSetupValue) -> String {
    match value {
        SafeSetupValue::Bool(value) => value.to_string(),
        SafeSetupValue::Integer(value) => value.to_string(),
        SafeSetupValue::Code(value) => value.to_string(),
        SafeSetupValue::String(value) => value.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        SafeDisplayText, SetupFieldId, SetupFieldValidation, SetupValueParseError,
        parse_setup_value,
    };

    use super::{SafeSetupValue, SetupFieldDescriptor, SetupFieldType};

    fn field(value_type: SetupFieldType) -> SetupFieldDescriptor {
        SetupFieldDescriptor {
            id: SetupFieldId::new("example").expect("field ID"),
            display_name: SafeDisplayText::new("Example").expect("display name"),
            help: SafeDisplayText::new("Example value").expect("help"),
            required: true,
            default: None,
            validation: SetupFieldValidation {
                value_type,
                min_length: None,
                max_length: None,
                minimum: None,
                maximum: None,
            },
            safe_to_project: true,
        }
    }

    fn message(error: SetupValueParseError) -> String {
        error.to_string()
    }

    #[test]
    fn validates_string_length() {
        let mut descriptor = field(SetupFieldType::String);
        descriptor.validation.min_length = Some(2);
        descriptor.validation.max_length = Some(4);

        assert_eq!(
            message(parse_setup_value(&descriptor, "a").unwrap_err()),
            "setup field `example` is shorter than the allowed minimum"
        );
        assert_eq!(
            message(parse_setup_value(&descriptor, "abcde").unwrap_err()),
            "setup field `example` is longer than the allowed maximum"
        );
        assert!(matches!(
            parse_setup_value(&descriptor, "abcd"),
            Ok(SafeSetupValue::String(_))
        ));
    }

    #[test]
    fn validates_integer_range_and_format() {
        let mut descriptor = field(SetupFieldType::Integer);
        descriptor.validation.minimum = Some(2);
        descriptor.validation.maximum = Some(4);

        assert_eq!(
            parse_setup_value(&descriptor, "3"),
            Ok(SafeSetupValue::Integer(3))
        );
        assert_eq!(
            message(parse_setup_value(&descriptor, "1").unwrap_err()),
            "setup field `example` is outside the allowed range"
        );
        assert_eq!(
            message(parse_setup_value(&descriptor, "three").unwrap_err()),
            "setup field `example` must be an integer"
        );
    }

    #[test]
    fn parses_documented_boolean_variants_case_insensitively() {
        let descriptor = field(SetupFieldType::Bool);
        for raw in ["true", "TRUE", "yes", "Y", "1"] {
            assert_eq!(
                parse_setup_value(&descriptor, raw),
                Ok(SafeSetupValue::Bool(true)),
                "{raw}"
            );
        }
        for raw in ["false", "FALSE", "no", "N", "0"] {
            assert_eq!(
                parse_setup_value(&descriptor, raw),
                Ok(SafeSetupValue::Bool(false)),
                "{raw}"
            );
        }
        assert_eq!(
            message(parse_setup_value(&descriptor, "maybe").unwrap_err()),
            "setup field `example` must be true or false"
        );
    }
}
