use serde::Serialize;
use serde_json::Value;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize)]
pub struct Error {
    pub code: u8,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial: Option<Value>,
}

impl Error {
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            partial: None,
        }
    }
    pub fn input(message: impl Into<String>) -> Self {
        Self::new(2, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(7, message)
    }
    pub fn partial(mut self, value: Value) -> Self {
        self.partial = Some(value);
        self
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::new(1, "Ошибка ввода-вывода")
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::new(1, "Ошибка локального хранилища")
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::input("Некорректный JSON")
    }
}
