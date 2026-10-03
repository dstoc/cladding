#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("{context} failed (exit code {code})")]
    CommandFailed { context: &'static str, code: i32 },
    #[error("{context} failed (exit code {code}){diagnostics}")]
    CommandFailedWithOutput {
        context: &'static str,
        code: i32,
        diagnostics: String,
    },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::CommandFailed { code, .. } | Error::CommandFailedWithOutput { code, .. } => {
                *code
            }
            _ => 1,
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Error::Message(message.into())
    }
}
