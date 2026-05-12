//! Lexer output: tokens with source spans.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// Bare integer literal — `42`, `1000000`, or with explicit
    /// `i64` suffix. Arbitrary precision (`int` is `BigInt`).
    Int(num_bigint::BigInt),
    /// `42u` literal — arbitrary-precision non-negative integer.
    UInt(num_bigint::BigInt),
    /// `3.14` / `1.5e10` / `1e-3` literal — IEEE-754 double.
    Float(crate::value::F64Bits),
    I32(i32),
    U32(u32),
    U64(u64),
    U128(u128),
    Str(String),
    Ident(String),

    // Keywords
    Fn,
    Let,
    Return,
    If,
    Else,
    True,
    False,
    Import,
    State,
    Entry,
    Struct,
    /// `group <name> { fields }` — inside a struct body, declares
    /// that the listed fields share a single storage cell named
    /// `<name>`. Groups control storage granularity: ungrouped
    /// fields get their own cells, grouped fields are read/written
    /// together as a blob.
    Group,
    /// `pub struct ...` — marks the declaration as visible to other
    /// modules. Bare `struct` stays private to its declaring module.
    /// Used together with the `module::Type` reference syntax to
    /// hand a typed value across the module boundary (e.g. an event
    /// emitted from one module that a handler in another listens
    /// for).
    Pub,
    Module,
    Emit,
    Nore,
    Modifier,
    Enum,
    Match,
    Const,
    Cap,
    /// `view fn ...` — declares the function may read state but
    /// never writes or emits. Verified by typeck against the
    /// effects classifier.
    View,
    /// `pure fn ...` — declares the function neither reads nor
    /// writes state, has no host calls, no events. A deterministic
    /// transform of its arguments.
    Pure,
    /// `interface Name { method sigs }` declares an abstract API
    /// surface that a value can be bound to. See `Dollar` for
    /// the dynamic-dispatch call notation.
    Interface,
    /// `$ident` prefix for dynamic-dispatch calls:
    /// `$token::transfer(...)` resolves at runtime via the
    /// interface value's bound module.
    Dollar,
    While,
    For,
    In,
    Break,
    Continue,
    Set,
    Dict,
    Index,
    UniqueIndex,
    On,
    Delete,

    // Punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Colon,
    Arrow,
    FatArrow,
    DotDot,
    DotDotEq,
    Eq,
    Amp,
    Caret,
    Shl,
    Shr,
    PipeBar,
    Dot,
    ColonColon,
    PipeArrow,
    DollarDollar,

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    BangEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AmpAmp,
    PipePipe,
    Bang,

    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
}
