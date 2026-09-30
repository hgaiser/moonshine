/// Simple error type that might be expanded on later.
#[derive(Debug, Clone)]
pub enum InputtinoError {
    /// Any error type, represented by an error message.
    Generic(String),
}

// TODO: When this becomes more complex, consider using `thiserror`.
impl std::fmt::Display for InputtinoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputtinoError::Generic(error_message) => write!(f, "{}", error_message),
        }
    }
}

impl std::error::Error for InputtinoError { }
