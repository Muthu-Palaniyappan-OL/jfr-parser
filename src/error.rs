//! Error type shared by the whole crate.

use std::fmt;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong while reading a recording.
///
/// Malformed or truncated input always surfaces as an `Err`; the parser does
/// not panic on bad data.
#[derive(Debug)]
pub enum Error {
    /// Underlying I/O failure.
    Io(std::io::Error),
    /// The file does not start with the `FLR\0` magic.
    BadMagic,
    /// Major/minor version is not one this parser understands.
    UnsupportedVersion {
        /// Major version found in the chunk header.
        major: u16,
        /// Minor version found in the chunk header.
        minor: u16,
    },
    /// A read ran past the end of the available bytes.
    Truncated {
        /// What was being read.
        what: &'static str,
        /// Offset, relative to the buffer being parsed.
        at: u64,
    },
    /// Structurally invalid data.
    Corrupt {
        /// What is wrong.
        what: &'static str,
        /// Offset, relative to the buffer being parsed.
        at: u64,
    },
    /// An id referenced a type that the metadata does not define.
    UnknownType(i64),
    /// A type name was looked up and not found.
    NoSuchType(String),
    /// A field name was looked up and not found.
    NoSuchField {
        /// The type (or `FROM` clause) that was searched.
        type_name: String,
        /// The missing field.
        field: String,
    },
    /// A query could not be parsed or evaluated.
    Query(String),
    /// Valid input that this implementation does not support.
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::BadMagic => write!(f, "not a JFR file (bad magic)"),
            Error::UnsupportedVersion { major, minor } => {
                write!(f, "unsupported JFR version {major}.{minor}")
            }
            Error::Truncated { what, at } => write!(f, "truncated data while reading {what} at offset {at}"),
            Error::Corrupt { what, at } => write!(f, "corrupt data: {what} at offset {at}"),
            Error::UnknownType(id) => write!(f, "type id {id} is not defined in the metadata"),
            Error::NoSuchType(n) => write!(f, "no such type: {n}"),
            Error::NoSuchField { type_name, field } => write!(f, "type {type_name} has no field {field}"),
            Error::Query(m) => write!(f, "query error: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), e.to_string())),
            Error::BadMagic => Error::BadMagic,
            Error::UnsupportedVersion { major, minor } => Error::UnsupportedVersion { major: *major, minor: *minor },
            Error::Truncated { what, at } => Error::Truncated { what, at: *at },
            Error::Corrupt { what, at } => Error::Corrupt { what, at: *at },
            Error::UnknownType(i) => Error::UnknownType(*i),
            Error::NoSuchType(s) => Error::NoSuchType(s.clone()),
            Error::NoSuchField { type_name, field } => {
                Error::NoSuchField { type_name: type_name.clone(), field: field.clone() }
            }
            Error::Query(s) => Error::Query(s.clone()),
            Error::Unsupported(s) => Error::Unsupported(s.clone()),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

#[cold]
#[inline(never)]
pub(crate) fn truncated(what: &'static str, at: usize) -> Error {
    Error::Truncated { what, at: at as u64 }
}

#[cold]
#[inline(never)]
pub(crate) fn corrupt(what: &'static str, at: usize) -> Error {
    Error::Corrupt { what, at: at as u64 }
}
