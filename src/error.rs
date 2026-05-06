use crate::token::Span;

#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrorKind,
    pub msg: String,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Lex,
    Parse,
    Type,
    Runtime,
}

impl Error {
    pub fn new(kind: ErrorKind, msg: impl Into<String>, span: Span) -> Self {
        Self { kind, msg: msg.into(), span }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} error at {}..{}: {}",
            self.kind, self.span.start, self.span.end, self.msg
        )
    }
}

impl std::error::Error for Error {}
