//! Narrow bincode 1 compatibility surface for the vendored syntect crate.

use std::io::{Read, Write};

use bincode_reloaded::{
    config,
    error::{DecodeError, EncodeError},
};
use serde::{de::DeserializeOwned, Serialize};

/// Error compatibility types exposed by bincode 1.
pub mod error {
    use std::{error::Error as StdError, fmt, io, str::Utf8Error};

    /// The result of a serialization or deserialization operation.
    pub type Result<T> = std::result::Result<T, Error>;

    /// An error that can be produced during serialization or deserialization.
    pub type Error = Box<ErrorKind>;

    /// The kind of error produced during serialization or deserialization.
    #[derive(Debug)]
    pub enum ErrorKind {
        /// The reader or writer returned an I/O error.
        Io(io::Error),
        /// A decoded string was not valid UTF-8.
        InvalidUtf8Encoding(Utf8Error),
        /// A decoded bool was not encoded as zero or one.
        InvalidBoolEncoding(u8),
        /// A decoded char was not valid.
        InvalidCharEncoding,
        /// A decoded enum tag was outside the expected range.
        InvalidTagEncoding(usize),
        /// Serde requested self-describing deserialization.
        DeserializeAnyNotSupported,
        /// The configured size limit was reached.
        SizeLimit,
        /// A sequence did not report its length before serialization.
        SequenceMustHaveLength,
        /// Another codec error represented as text.
        Custom(String),
    }

    impl fmt::Display for ErrorKind {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Io(error) => write!(formatter, "io error: {error}"),
                Self::InvalidUtf8Encoding(error) => {
                    write!(formatter, "string is not valid utf8: {error}")
                }
                Self::InvalidBoolEncoding(value) => {
                    write!(
                        formatter,
                        "invalid u8 while decoding bool, expected 0 or 1, found {value}"
                    )
                }
                Self::InvalidCharEncoding => formatter.write_str("char is not valid"),
                Self::InvalidTagEncoding(tag) => {
                    write!(formatter, "tag for enum is not valid, found {tag}")
                }
                Self::DeserializeAnyNotSupported => formatter.write_str(
                    "Bincode does not support the serde::Deserializer::deserialize_any method",
                ),
                Self::SizeLimit => formatter.write_str("the size limit has been reached"),
                Self::SequenceMustHaveLength => formatter.write_str(
                    "Bincode can only encode sequences and maps that have a knowable size ahead of time",
                ),
                Self::Custom(message) => formatter.write_str(message),
            }
        }
    }

    impl StdError for ErrorKind {
        // bincode 1.3.3 implemented the deprecated compatibility methods but
        // not `source`; preserving that distinction is part of this shim's API.
        #[allow(deprecated)]
        fn description(&self) -> &str {
            match self {
                Self::Io(error) => StdError::description(error),
                Self::InvalidUtf8Encoding(_) => "string is not valid utf8",
                Self::InvalidBoolEncoding(_) => "invalid u8 while decoding bool",
                Self::InvalidCharEncoding => "char is not valid",
                Self::InvalidTagEncoding(_) => "tag for enum is not valid",
                Self::SequenceMustHaveLength => {
                    "Bincode can only encode sequences and maps that have a knowable size ahead of time"
                }
                Self::DeserializeAnyNotSupported => {
                    "Bincode doesn't support serde::Deserializer::deserialize_any"
                }
                Self::SizeLimit => "the size limit has been reached",
                Self::Custom(message) => message,
            }
        }

        #[allow(deprecated)]
        fn cause(&self) -> Option<&dyn StdError> {
            match self {
                Self::Io(error) => Some(error),
                _ => None,
            }
        }
    }

    impl From<io::Error> for Error {
        fn from(error: io::Error) -> Self {
            Box::new(ErrorKind::Io(error))
        }
    }
}

pub use error::{Error, ErrorKind, Result};

impl serde::de::Error for Error {
    fn custom<T: std::fmt::Display>(description: T) -> Self {
        Box::new(ErrorKind::Custom(description.to_string()))
    }
}

impl serde::ser::Error for Error {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Box::new(ErrorKind::Custom(message.to_string()))
    }
}

impl From<EncodeError> for Error {
    fn from(error: EncodeError) -> Self {
        let kind = match error {
            EncodeError::Io { inner, .. } => ErrorKind::Io(inner),
            EncodeError::Serde(bincode_reloaded::serde::EncodeError::SequenceMustHaveLength) => {
                ErrorKind::SequenceMustHaveLength
            }
            other => ErrorKind::Custom(other.to_string()),
        };
        Box::new(kind)
    }
}

impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Self {
        let kind = match error {
            DecodeError::Io { inner, .. } => ErrorKind::Io(inner),
            DecodeError::Utf8 { inner } => ErrorKind::InvalidUtf8Encoding(inner),
            DecodeError::InvalidBooleanValue(value) => ErrorKind::InvalidBoolEncoding(value),
            DecodeError::InvalidCharEncoding(_) => ErrorKind::InvalidCharEncoding,
            DecodeError::UnexpectedVariant { found, .. } => {
                ErrorKind::InvalidTagEncoding(found as usize)
            }
            DecodeError::LimitExceeded => ErrorKind::SizeLimit,
            DecodeError::Serde(
                bincode_reloaded::serde::DecodeError::AnyNotSupported
                | bincode_reloaded::serde::DecodeError::IgnoredAnyNotSupported,
            ) => ErrorKind::DeserializeAnyNotSupported,
            DecodeError::UnexpectedEnd { .. } => ErrorKind::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )),
            other => ErrorKind::Custom(other.to_string()),
        };
        Box::new(kind)
    }
}

/// Serializes a value with the bincode 1 wire configuration.
pub fn serialize_into<W, T>(writer: W, value: &T) -> Result<()>
where
    W: Write,
    T: ?Sized + Serialize,
{
    let mut writer = writer;
    bincode_reloaded::serde::encode_into_std_write(value, &mut writer, config::legacy())
        .map(|_| ())
        .map_err(Into::into)
}

/// Deserializes a value with the bincode 1 wire configuration.
pub fn deserialize_from<R, T>(reader: R) -> Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut reader = reader;
    bincode_reloaded::serde::decode_from_std_read(&mut reader, config::legacy()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, io};

    use super::{Error, ErrorKind};

    fn assert_error_behavior(
        error: ErrorKind,
        display: &str,
        description: &str,
        cause: Option<&str>,
    ) {
        assert_eq!(error.to_string(), display);
        #[allow(deprecated)]
        {
            assert_eq!(error.description(), description);
            assert_eq!(error.cause().map(ToString::to_string).as_deref(), cause);
        }
        assert!(error.source().is_none());
    }

    #[test]
    fn legacy_wire_bytes_round_trip() {
        let value = vec!["rust".to_owned(), "json".to_owned(), "shell".to_owned()];
        let mut encoded = Vec::new();
        super::serialize_into(&mut encoded, &value).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&3_u64.to_le_bytes());
        for item in ["rust", "json", "shell"] {
            expected.extend_from_slice(&(item.len() as u64).to_le_bytes());
            expected.extend_from_slice(item.as_bytes());
        }
        assert_eq!(encoded, expected);
        assert_eq!(
            super::deserialize_from::<_, Vec<String>>(&encoded[..]).unwrap(),
            value
        );
    }

    #[test]
    fn bincode_1_3_3_error_behavior_is_golden() {
        let invalid_bytes = vec![0xff];
        let invalid_utf8 = std::str::from_utf8(&invalid_bytes).unwrap_err();

        let io_error = io::Error::other("boom");
        #[allow(deprecated)]
        let io_description = io_error.description().to_owned();
        assert_error_behavior(
            ErrorKind::Io(io_error),
            "io error: boom",
            &io_description,
            Some("boom"),
        );
        assert_error_behavior(
            ErrorKind::InvalidUtf8Encoding(invalid_utf8),
            "string is not valid utf8: invalid utf-8 sequence of 1 bytes from index 0",
            "string is not valid utf8",
            None,
        );
        assert_error_behavior(
            ErrorKind::InvalidBoolEncoding(2),
            "invalid u8 while decoding bool, expected 0 or 1, found 2",
            "invalid u8 while decoding bool",
            None,
        );
        assert_error_behavior(
            ErrorKind::InvalidCharEncoding,
            "char is not valid",
            "char is not valid",
            None,
        );
        assert_error_behavior(
            ErrorKind::InvalidTagEncoding(7),
            "tag for enum is not valid, found 7",
            "tag for enum is not valid",
            None,
        );
        assert_error_behavior(
            ErrorKind::DeserializeAnyNotSupported,
            "Bincode does not support the serde::Deserializer::deserialize_any method",
            "Bincode doesn't support serde::Deserializer::deserialize_any",
            None,
        );
        assert_error_behavior(
            ErrorKind::SizeLimit,
            "the size limit has been reached",
            "the size limit has been reached",
            None,
        );
        assert_error_behavior(
            ErrorKind::SequenceMustHaveLength,
            "Bincode can only encode sequences and maps that have a knowable size ahead of time",
            "Bincode can only encode sequences and maps that have a knowable size ahead of time",
            None,
        );
        assert_error_behavior(
            ErrorKind::Custom("custom".to_owned()),
            "custom",
            "custom",
            None,
        );
    }

    #[test]
    fn serde_custom_and_reloaded_errors_map_to_bincode_1_kinds() {
        let serialize: Error = <Error as serde::ser::Error>::custom("serialize");
        assert!(matches!(*serialize, ErrorKind::Custom(ref value) if value == "serialize"));
        let deserialize: Error = <Error as serde::de::Error>::custom("deserialize");
        assert!(matches!(*deserialize, ErrorKind::Custom(ref value) if value == "deserialize"));

        let sequence: Error = bincode_reloaded::error::EncodeError::Serde(
            bincode_reloaded::serde::EncodeError::SequenceMustHaveLength,
        )
        .into();
        assert!(matches!(*sequence, ErrorKind::SequenceMustHaveLength));

        let boolean: Error = bincode_reloaded::error::DecodeError::InvalidBooleanValue(3).into();
        assert!(matches!(*boolean, ErrorKind::InvalidBoolEncoding(3)));

        let any: Error = bincode_reloaded::error::DecodeError::Serde(
            bincode_reloaded::serde::DecodeError::AnyNotSupported,
        )
        .into();
        assert!(matches!(*any, ErrorKind::DeserializeAnyNotSupported));

        let end: Error =
            bincode_reloaded::error::DecodeError::UnexpectedEnd { additional: 8 }.into();
        assert!(
            matches!(*end, ErrorKind::Io(ref error) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
        assert_eq!(end.to_string(), "io error: failed to fill whole buffer");
    }
}
