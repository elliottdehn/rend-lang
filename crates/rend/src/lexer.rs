use crate::error::{Error, ErrorKind};
use crate::token::{Span, Spanned, Token};

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self { src, bytes: src.as_bytes(), pos: 0 }
    }

    pub fn tokenize(mut self) -> Result<Vec<Spanned>, Error> {
        let mut out = Vec::new();
        loop {
            self.skip_ws_and_comments();
            let start = self.pos;
            if self.pos >= self.bytes.len() {
                out.push(Spanned {
                    token: Token::Eof,
                    span: Span { start, end: start },
                });
                return Ok(out);
            }
            let c = self.bytes[self.pos];
            let token = match c {
                b'(' => { self.pos += 1; Token::LParen }
                b')' => { self.pos += 1; Token::RParen }
                b'{' => { self.pos += 1; Token::LBrace }
                b'}' => { self.pos += 1; Token::RBrace }
                b'[' => { self.pos += 1; Token::LBracket }
                b']' => { self.pos += 1; Token::RBracket }
                b',' => { self.pos += 1; Token::Comma }
                b';' => { self.pos += 1; Token::Semi }
                b':' => self.two_or_one(b':', Token::ColonColon, Token::Colon),
                b'.' => {
                    self.pos += 1;
                    if self.peek_byte() == Some(b'.') {
                        self.pos += 1;
                        if self.peek_byte() == Some(b'=') {
                            self.pos += 1;
                            Token::DotDotEq
                        } else {
                            Token::DotDot
                        }
                    } else {
                        Token::Dot
                    }
                }
                b'+' => { self.pos += 1; Token::Plus }
                b'-' => self.two_or_one(b'>', Token::Arrow, Token::Minus),
                b'*' => { self.pos += 1; Token::Star }
                b'/' => { self.pos += 1; Token::Slash }
                b'%' => { self.pos += 1; Token::Percent }
                b'=' => {
                    self.pos += 1;
                    match self.peek_byte() {
                        Some(b'=') => { self.pos += 1; Token::EqEq }
                        Some(b'>') => { self.pos += 1; Token::FatArrow }
                        _ => Token::Eq,
                    }
                }
                b'!' => self.two_or_one(b'=', Token::BangEq, Token::Bang),
                b'<' => {
                    self.pos += 1;
                    match self.peek_byte() {
                        Some(b'=') => { self.pos += 1; Token::LtEq }
                        Some(b'<') => { self.pos += 1; Token::Shl }
                        _ => Token::Lt,
                    }
                }
                b'>' => {
                    self.pos += 1;
                    match self.peek_byte() {
                        Some(b'=') => { self.pos += 1; Token::GtEq }
                        Some(b'>') => { self.pos += 1; Token::Shr }
                        _ => Token::Gt,
                    }
                }
                b'&' => self.two_or_one(b'&', Token::AmpAmp, Token::Amp),
                b'^' => { self.pos += 1; Token::Caret }
                b'|' => {
                    self.pos += 1;
                    if self.peek_byte() == Some(b'|') {
                        self.pos += 1;
                        Token::PipePipe
                    } else if self.peek_byte() == Some(b'>') {
                        self.pos += 1;
                        Token::PipeArrow
                    } else {
                        Token::PipeBar
                    }
                }
                b'$' => {
                    self.pos += 1;
                    if self.peek_byte() == Some(b'$') {
                        self.pos += 1;
                        Token::DollarDollar
                    } else {
                        // `$ident::method(...)` — dynamic-dispatch
                        // call notation. The lexer just emits the
                        // single-`$` token; the parser handles the
                        // surrounding shape.
                        Token::Dollar
                    }
                }
                b'"' => self.lex_string(start)?,
                d if d.is_ascii_digit() => self.lex_int(start)?,
                a if a.is_ascii_alphabetic() || a == b'_' => self.lex_ident(start),
                other => {
                    return Err(Error::new(
                        ErrorKind::Lex,
                        format!("unexpected character {:?}", other as char),
                        Span { start, end: start + 1 },
                    ));
                }
            };
            let end = self.pos;
            out.push(Spanned { token, span: Span { start, end } });
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_byte_at(&self, off: usize) -> Option<u8> {
        self.bytes.get(self.pos + off).copied()
    }

    fn two_or_one(&mut self, second: u8, both: Token, single: Token) -> Token {
        self.pos += 1;
        if self.peek_byte() == Some(second) {
            self.pos += 1;
            both
        } else {
            single
        }
    }

    fn lex_string(&mut self, start: usize) -> Result<Token, Error> {
        // current byte is the opening '"'
        self.pos += 1;
        let mut s = String::new();
        loop {
            match self.peek_byte() {
                None => {
                    return Err(Error::new(
                        ErrorKind::Lex,
                        "unterminated string literal",
                        Span { start, end: self.pos },
                    ));
                }
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(Token::Str(s));
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek_byte() {
                        Some(b'"') => { s.push('"'); self.pos += 1; }
                        Some(b'\\') => { s.push('\\'); self.pos += 1; }
                        Some(b'n') => { s.push('\n'); self.pos += 1; }
                        Some(b't') => { s.push('\t'); self.pos += 1; }
                        Some(b'r') => { s.push('\r'); self.pos += 1; }
                        Some(b'0') => { s.push('\0'); self.pos += 1; }
                        Some(other) => {
                            return Err(Error::new(
                                ErrorKind::Lex,
                                format!("unknown escape \\{}", other as char),
                                Span { start, end: self.pos },
                            ));
                        }
                        None => {
                            return Err(Error::new(
                                ErrorKind::Lex,
                                "string ended after backslash",
                                Span { start, end: self.pos },
                            ));
                        }
                    }
                }
                Some(_) => {
                    // append the next UTF-8 codepoint as-is, advancing by its byte length
                    let rest = &self.src[self.pos..];
                    let ch = rest.chars().next().expect("non-empty");
                    s.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn lex_int(&mut self, start: usize) -> Result<Token, Error> {
        while let Some(c) = self.peek_byte() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let digits_end = self.pos;
        let digits = &self.src[start..digits_end];

        // If the next chars look like float syntax (`.digit`, `e`,
        // `E`), consume them and return a `Float` token. `.foo` and
        // `.0` (tuple-index) are *not* floats — the `.` has to be
        // followed by a digit. `1e10` and `1e-3` are floats even
        // without a `.`.
        //
        // Disambiguation for tuple-index chains: in `p.0.0`, the
        // bytes after `p.` are `0.0`, which would otherwise parse
        // as `Float(0.0)`. Suppress float mode whenever the byte
        // immediately *before* the leading digit is itself `.` —
        // that means we're in `<expr>.<digits>` land (tuple index),
        // not a number-literal position.
        let prev_byte_is_dot = start > 0 && self.bytes[start - 1] == b'.';
        let saw_dot = !prev_byte_is_dot
            && self.peek_byte() == Some(b'.')
            && self.peek_byte_at(1).map(|c| c.is_ascii_digit()).unwrap_or(false);
        let saw_exp = !prev_byte_is_dot
            && matches!(self.peek_byte(), Some(b'e') | Some(b'E'));
        if saw_dot || saw_exp {
            if saw_dot {
                self.pos += 1; // .
                while let Some(c) = self.peek_byte() {
                    if c.is_ascii_digit() { self.pos += 1; } else { break; }
                }
            }
            if matches!(self.peek_byte(), Some(b'e') | Some(b'E')) {
                self.pos += 1;
                if matches!(self.peek_byte(), Some(b'+') | Some(b'-')) { self.pos += 1; }
                let exp_digits = self.pos;
                while let Some(c) = self.peek_byte() {
                    if c.is_ascii_digit() { self.pos += 1; } else { break; }
                }
                if self.pos == exp_digits {
                    return Err(Error::new(
                        ErrorKind::Lex,
                        "expected digits in float exponent",
                        Span { start, end: self.pos },
                    ));
                }
            }
            let text = &self.src[start..self.pos];
            let f: f64 = text.parse().map_err(|_| Error::new(
                ErrorKind::Lex,
                format!("invalid float literal '{text}'"),
                Span { start, end: self.pos },
            ))?;
            return Ok(Token::Float(crate::value::F64Bits(f)));
        }

        // Optional suffix: i32 / u32 / u64 / u128 / i64 / u
        let suffix_start = self.pos;
        while let Some(c) = self.peek_byte() {
            if c.is_ascii_alphanumeric() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let suffix = &self.src[suffix_start..self.pos];

        let span = Span { start, end: self.pos };
        match suffix {
            "" | "i64" => digits.parse::<num_bigint::BigInt>().map(Token::Int).map_err(|_| {
                Error::new(ErrorKind::Lex, format!("invalid int literal '{digits}'"), span)
            }),
            // `42u` → arbitrary-precision non-negative integer. The
            // digits are already parsed as a non-negative literal
            // (the leading `-` is a separate Minus token), so the
            // BigInt is naturally non-negative.
            "u" => digits.parse::<num_bigint::BigInt>().map(Token::UInt).map_err(|_| {
                Error::new(ErrorKind::Lex, format!("invalid uint literal '{digits}'"), span)
            }),
            "i32" => digits.parse::<i32>().map(Token::I32).map_err(|_| {
                Error::new(ErrorKind::Lex, format!("invalid i32 literal '{digits}'"), span)
            }),
            "u32" => digits.parse::<u32>().map(Token::U32).map_err(|_| {
                Error::new(ErrorKind::Lex, format!("invalid u32 literal '{digits}'"), span)
            }),
            "u64" => digits.parse::<u64>().map(Token::U64).map_err(|_| {
                Error::new(ErrorKind::Lex, format!("invalid u64 literal '{digits}'"), span)
            }),
            "u128" => digits.parse::<u128>().map(Token::U128).map_err(|_| {
                Error::new(ErrorKind::Lex, format!("invalid u128 literal '{digits}'"), span)
            }),
            other => Err(Error::new(
                ErrorKind::Lex,
                format!("unknown integer suffix '{other}'"),
                span,
            )),
        }
    }

    fn lex_ident(&mut self, start: usize) -> Token {
        while let Some(c) = self.peek_byte() {
            if c.is_ascii_alphanumeric() || c == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = &self.src[start..self.pos];
        match s {
            "fn" => Token::Fn,
            "let" => Token::Let,
            "if" => Token::If,
            "else" => Token::Else,
            "return" => Token::Return,
            "true" => Token::True,
            "false" => Token::False,
            "import" => Token::Import,
            "state" => Token::State,
            "entry" => Token::Entry,
            "struct" => Token::Struct,
            "group" => Token::Group,
            "module" => Token::Module,
            "pub" => Token::Pub,
            "emit" => Token::Emit,
            "nore" => Token::Nore,
            "modifier" => Token::Modifier,
            "enum" => Token::Enum,
            "match" => Token::Match,
            "const" => Token::Const,
            "cap" => Token::Cap,
            "view" => Token::View,
            "pure" => Token::Pure,
            "interface" => Token::Interface,
            "while" => Token::While,
            "parallel" => Token::Parallel,
            "for" => Token::For,
            "in" => Token::In,
            "break" => Token::Break,
            "continue" => Token::Continue,
            "set" => Token::Set,
            "dict" => Token::Dict,
            "index" => Token::Index,
            "unique_index" => Token::UniqueIndex,
            "ASC" => Token::Asc,
            "DESC" => Token::Desc,
            "on" => Token::On,
            "delete" => Token::Delete,
            _ => Token::Ident(s.to_string()),
        }
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while let Some(c) = self.peek_byte() {
                if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.peek_byte() == Some(b'/') && self.peek_byte_at(1) == Some(b'/') {
                while let Some(c) = self.peek_byte() {
                    if c == b'\n' {
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }
}

pub fn tokenize(src: &str) -> Result<Vec<Spanned>, Error> {
    Lexer::new(src).tokenize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Token> {
        tokenize(src).unwrap().into_iter().map(|s| s.token).collect()
    }

    #[test]
    fn empty_source_is_just_eof() {
        assert_eq!(toks(""), vec![Token::Eof]);
    }

    #[test]
    fn whitespace_only_is_eof() {
        assert_eq!(toks("   \n\t  "), vec![Token::Eof]);
    }

    #[test]
    fn integer_literal() {
        assert_eq!(toks("42"), vec![Token::Int(num_bigint::BigInt::from(42i64)), Token::Eof]);
    }

    #[test]
    fn identifier_and_keyword_distinction() {
        assert_eq!(
            toks("let x"),
            vec![Token::Let, Token::Ident("x".into()), Token::Eof],
        );
        assert_eq!(
            toks("letx"),
            vec![Token::Ident("letx".into()), Token::Eof],
        );
    }

    #[test]
    fn two_char_operators_are_greedy() {
        assert_eq!(
            toks("== != <= >= && ||"),
            vec![
                Token::EqEq, Token::BangEq, Token::LtEq, Token::GtEq,
                Token::AmpAmp, Token::PipePipe, Token::Eof,
            ],
        );
    }

    #[test]
    fn single_eq_is_not_eqeq() {
        assert_eq!(toks("="), vec![Token::Eq, Token::Eof]);
    }

    #[test]
    fn line_comment_consumes_to_newline() {
        assert_eq!(
            toks("// hello\n42"),
            vec![Token::Int(num_bigint::BigInt::from(42i64)), Token::Eof],
        );
    }

    #[test]
    fn bare_ampersand_is_bitwise_and() {
        let toks = tokenize("&").unwrap();
        assert_eq!(toks[0].token, Token::Amp);
    }

    #[test]
    fn rejects_unknown_char() {
        assert!(tokenize("@").is_err());
    }

    #[test]
    fn arrow_is_two_chars() {
        assert_eq!(toks("->"), vec![Token::Arrow, Token::Eof]);
        assert_eq!(toks("- >"), vec![Token::Minus, Token::Gt, Token::Eof]);
    }

    #[test]
    fn colon_is_a_token() {
        assert_eq!(
            toks("a: i64"),
            vec![Token::Ident("a".into()), Token::Colon, Token::Ident("i64".into()), Token::Eof],
        );
    }

    #[test]
    fn boolean_keywords() {
        assert_eq!(
            toks("true false"),
            vec![Token::True, Token::False, Token::Eof],
        );
    }

    #[test]
    fn span_covers_token() {
        let toks = tokenize("  42  ").unwrap();
        assert_eq!(toks[0].token, Token::Int(num_bigint::BigInt::from(42i64)));
        assert_eq!(toks[0].span.start, 2);
        assert_eq!(toks[0].span.end, 4);
    }
}
