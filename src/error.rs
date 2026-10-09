use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub struct DevError(pub String);

impl Display for DevError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for DevError {}

impl From<std::io::Error> for DevError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<toml::de::Error> for DevError {
    fn from(error: toml::de::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<toml::ser::Error> for DevError {
    fn from(error: toml::ser::Error) -> Self {
        Self(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, DevError>;
