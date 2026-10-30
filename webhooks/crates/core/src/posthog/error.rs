use std::fmt::{Display, Formatter};

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(msg) => write!(f, "Connection Error: {msg}"),
            Self::Serialization(msg) => write!(f, "Serialization Error: {msg}"),
            Self::AlreadyInitialized => write!(f, "Client already initialized"),
            Self::NotInitialized => write!(f, "Client not initialized"),
            Self::InvalidTimestamp(msg) => write!(f, "Invalid Timestamp: {msg}"),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Connection(String),
    Serialization(String),
    AlreadyInitialized,
    NotInitialized,
    InvalidTimestamp(String),
}
