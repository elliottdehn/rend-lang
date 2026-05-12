//! Recursive-descent parser with Pratt-style operator precedence.

use crate::ast::*;
use crate::error::{Error, ErrorKind};
use crate::token::{Span, Spanned, Token};

pub struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
    /// When true, primary-expression parsing won't consume `Ident { ... }` as
    /// a struct literal. Set in if/while-style condition contexts so the
    /// trailing `{` is reserved for the body block.
    no_struct_literal: bool,
    /// Set of names known to be struct types (collected before expressions
    /// are parsed). Lets `Name {` be recognized as a struct literal only
    /// when `Name` is actually a struct.
    struct_names: std::collections::HashSet<String>,
    /// Set of enum-type names. Distinguishes `Foo::Bar(args)` as
    /// either an enum constructor (when `Foo` is an enum) or a
    /// cross-module call.
    enum_names: std::collections::HashSet<String>,
    /// Set of interface-type names. Used in `parse_type` so
    /// `let t: IFoo = ...;` resolves IFoo as `Type::Interface`,
    /// not as an unknown struct.
    interface_names: std::collections::HashSet<String>,
}

impl Parser {
    pub fn new(tokens: Vec<Spanned>) -> Self {
        Self::with_extra_iface_names(tokens, std::collections::HashSet::new())
    }

    /// Like `new`, but seeds the interface-name set with names
    /// declared in dependency artifacts. The pre-scan still adds
    /// any interfaces declared in the source itself; the seed
    /// just makes referenced interface names from deps parse as
    /// type identifiers.
    pub fn with_extra_iface_names(
        tokens: Vec<Spanned>,
        extra_iface_names: std::collections::HashSet<String>,
    ) -> Self {
        Self {
            tokens,
            pos: 0,
            no_struct_literal: false,
            struct_names: std::collections::HashSet::new(),
            enum_names: std::collections::HashSet::new(),
            interface_names: extra_iface_names,
        }
    }

    pub fn parse_module(mut self) -> Result<Module, Error> {
        // Optional `module <name>;` preamble. Must be the very first
        // top-level item if present; lets the engine group multi-module
        // sources without an out-of-band name table.
        let name = if matches!(self.peek_token(), Token::Module) {
            self.advance();
            let n = self.expect_ident()?;
            self.expect(Token::Semi, "expected ';' after module name")?;
            Some(n)
        } else {
            None
        };

        // First pass: collect struct + enum + cap + interface names
        // so the second pass can disambiguate `Name { ... }` (struct
        // / cap literal) and `Name::Variant(...)` (enum ctor vs
        // cross-module call). Interface names are recognized in
        // type position; the rules differ from struct in typeck.
        let saved_pos = self.pos;
        while !self.is_eof() {
            if matches!(self.peek_token(),
                Token::Struct | Token::Enum | Token::Cap | Token::Interface
            ) {
                let kw = self.peek_token().clone();
                self.advance();
                if let Token::Ident(name) = self.peek_token().clone() {
                    match kw {
                        Token::Struct | Token::Cap => { self.struct_names.insert(name); }
                        Token::Enum => { self.enum_names.insert(name); }
                        Token::Interface => { self.interface_names.insert(name); }
                        _ => {}
                    }
                }
                // skip past the body — best-effort lookahead
                let mut depth = 0i32;
                while !self.is_eof() {
                    match self.peek_token() {
                        Token::LBrace => { depth += 1; self.advance(); }
                        Token::RBrace => {
                            self.advance();
                            depth -= 1;
                            if depth == 0 { break; }
                        }
                        _ => self.advance(),
                    }
                }
            } else {
                self.advance();
            }
        }
        self.pos = saved_pos;

        let mut imports = Vec::new();
        let mut states = Vec::new();
        let mut functions = Vec::new();
        let mut structs = Vec::new();
        let mut events = Vec::new();
        let mut modifiers = Vec::new();
        let mut enums = Vec::new();
        let mut consts = Vec::new();
        let mut caps = Vec::new();
        let mut interfaces = Vec::new();
        let mut indexes = Vec::new();
        while !self.is_eof() {
            match self.peek_token() {
                Token::Import => imports.push(self.parse_import()?),
                Token::State => states.push(self.parse_state()?),
                Token::Struct => structs.push(self.parse_struct_decl()?),
                Token::Event => events.push(self.parse_event_decl()?),
                Token::Modifier => modifiers.push(self.parse_modifier_decl()?),
                Token::Enum => enums.push(self.parse_enum_decl()?),
                Token::Const => consts.push(self.parse_const_decl()?),
                Token::Cap => caps.push(self.parse_cap_decl()?),
                Token::Interface => interfaces.push(self.parse_interface_decl()?),
                Token::Index => indexes.push(self.parse_index_decl(IndexKind::Multi)?),
                Token::UniqueIndex => indexes.push(self.parse_index_decl(IndexKind::Unique)?),
                Token::Entry | Token::Nore | Token::View | Token::Pure => {
                    let mut is_entry = false;
                    let mut is_nore = false;
                    let mut is_view = false;
                    let mut is_pure = false;
                    while matches!(self.peek_token(),
                        Token::Entry | Token::Nore | Token::View | Token::Pure
                    ) {
                        match self.peek_token() {
                            Token::Entry => { is_entry = true; self.advance(); }
                            Token::Nore  => { is_nore  = true; self.advance(); }
                            Token::View  => { is_view  = true; self.advance(); }
                            Token::Pure  => { is_pure  = true; self.advance(); }
                            _ => unreachable!(),
                        }
                    }
                    if is_view && is_pure {
                        let span = self.cur_span();
                        return Err(Error::new(
                            ErrorKind::Parse,
                            "function cannot be both `view` and `pure`; pure already implies view",
                            span,
                        ));
                    }
                    functions.push(self.parse_fn_with_modifiers(is_entry, is_nore, is_view, is_pure)?);
                }
                Token::Fn => functions.push(self.parse_fn()?),
                Token::Module => {
                    let span = self.cur_span();
                    return Err(Error::new(
                        ErrorKind::Parse,
                        "`module <name>;` must be the first item in the file",
                        span,
                    ));
                }
                other => {
                    let span = self.cur_span();
                    return Err(Error::new(
                        ErrorKind::Parse,
                        format!(
                            "expected 'fn', 'entry fn', 'struct', 'cap', 'event', 'import', 'state', or 'module' at top level, got {other:?}",
                        ),
                        span,
                    ));
                }
            }
        }
        Ok(Module { name, imports, states, functions, structs, events, modifiers, enums, consts, caps, interfaces, indexes })
    }

    /// `index NAME on STATE.field1.field2;` (multi: pmap<F, [K]>) or
    /// `unique_index NAME on STATE.field1.field2;` (unique: pmap<F, K>).
    /// The index slot itself is a regular `pmap` declared via `state`;
    /// this decl tells the compiler to auto-maintain it on writes to
    /// the `STATE` primary. The leading keyword is consumed by the
    /// caller; `kind` records which variant fired.
    fn parse_index_decl(&mut self, kind: IndexKind) -> Result<IndexDecl, Error> {
        let start = self.cur_span().start;
        // Eat whichever index keyword the caller dispatched on.
        match self.peek_token() {
            Token::Index | Token::UniqueIndex => self.advance(),
            other => return Err(Error::new(
                ErrorKind::Parse,
                format!("expected 'index' or 'unique_index', got {other:?}"),
                self.cur_span(),
            )),
        }
        let name = self.expect_ident()?;
        self.expect(Token::On, "expected 'on' after index name")?;
        let on_state = self.expect_ident()?;
        self.expect(Token::Dot, "expected '.' before projected field")?;
        let mut projection = vec![self.expect_ident()?];
        while matches!(self.peek_token(), Token::Dot) {
            self.advance();
            projection.push(self.expect_ident()?);
        }
        let semi = self.cur_span();
        self.expect(Token::Semi, "expected ';' after index decl")?;
        Ok(IndexDecl {
            name,
            on_state,
            projection,
            kind,
            span: Span { start, end: semi.end },
        })
    }

    /// `const NAME: T = expr;` at module level.
    fn parse_const_decl(&mut self) -> Result<ConstDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Const, "expected 'const'")?;
        let name = self.expect_ident()?;
        self.expect(Token::Colon, "expected ':' after const name")?;
        let ty = self.parse_type()?;
        self.expect(Token::Eq, "expected '=' in const decl")?;
        let value = self.parse_expr()?;
        let semi = self.cur_span();
        self.expect(Token::Semi, "expected ';' after const decl")?;
        Ok(ConstDecl { name, ty, value, span: Span { start, end: semi.end } })
    }

    /// `enum Name { Unit, Tup(T1, T2), ... }`
    fn parse_enum_decl(&mut self) -> Result<EnumDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Enum, "expected 'enum'")?;
        let name = self.expect_ident()?;
        // Add to struct_names so the existing `Name { ... }` literal
        // path keeps working — but enums never use that form, so the
        // overlap is harmless. We do NOT add here; rely on parser to
        // disambiguate by `::`.
        self.expect(Token::LBrace, "expected '{' after enum name")?;
        let mut variants = Vec::new();
        while !self.peek_is(&Token::RBrace) {
            let vstart = self.cur_span().start;
            let vname = self.expect_ident()?;
            let payload = if self.peek_is(&Token::LParen) {
                self.advance();
                let mut tys = Vec::new();
                while !self.peek_is(&Token::RParen) {
                    tys.push(self.parse_type()?);
                    if !self.peek_is(&Token::RParen) {
                        self.expect(Token::Comma, "expected ',' between variant payload types")?;
                    }
                }
                self.expect(Token::RParen, "expected ')'")?;
                tys
            } else {
                Vec::new()
            };
            let vend = self.cur_span().start;
            variants.push(EnumVariant {
                name: vname,
                payload,
                span: Span { start: vstart, end: vend },
            });
            if !self.peek_is(&Token::RBrace) {
                self.expect(Token::Comma, "expected ',' between variants")?;
            }
        }
        let close = self.cur_span();
        self.expect(Token::RBrace, "expected '}'")?;
        Ok(EnumDecl { name, variants, span: Span { start, end: close.end } })
    }

    /// `modifier Name(p1: T1, ...) { stmts; _; stmts; }`
    fn parse_modifier_decl(&mut self) -> Result<ModifierDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Modifier, "expected 'modifier'")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "expected '(' after modifier name")?;
        let mut params = Vec::new();
        while !self.peek_is(&Token::RParen) {
            let pstart = self.cur_span().start;
            let pname = self.expect_ident()?;
            self.expect(Token::Colon, "expected ':' after modifier param name")?;
            let pty = self.parse_type()?;
            let pend = self.cur_span().start;
            params.push(Param {
                name: pname,
                ty: pty,
                span: Span { start: pstart, end: pend },
            });
            if !self.peek_is(&Token::RParen) {
                self.expect(Token::Comma, "expected ',' between modifier params")?;
            }
        }
        self.expect(Token::RParen, "expected ')'")?;
        let body = self.parse_block()?;
        let end = body.span.end;
        Ok(ModifierDecl { name, params, body, span: Span { start, end } })
    }

    /// `event Foo(p1: T1, p2: T2, ...);`
    fn parse_event_decl(&mut self) -> Result<EventDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Event, "expected 'event'")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "expected '(' after event name")?;
        let mut params = Vec::new();
        while !self.peek_is(&Token::RParen) {
            let pstart = self.cur_span().start;
            let pname = self.expect_ident()?;
            self.expect(Token::Colon, "expected ':' after event param name")?;
            let pty = self.parse_type()?;
            let pend = self.cur_span().start;
            params.push(Param {
                name: pname,
                ty: pty,
                span: Span { start: pstart, end: pend },
            });
            if !self.peek_is(&Token::RParen) {
                self.expect(Token::Comma, "expected ',' between event params")?;
            }
        }
        self.expect(Token::RParen, "expected ')' to close event params")?;
        let semi = self.cur_span();
        self.expect(Token::Semi, "expected ';' after event declaration")?;
        Ok(EventDecl { name, params, span: Span { start, end: semi.end } })
    }

    fn parse_struct_decl(&mut self) -> Result<StructDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Struct, "expected 'struct'")?;
        let name = self.expect_ident()?;
        self.expect(Token::LBrace, "expected '{' after struct name")?;
        let mut fields = Vec::new();
        while !self.peek_is(&Token::RBrace) {
            let fstart = self.cur_span().start;
            let fname = self.expect_ident()?;
            self.expect(Token::Colon, "expected ':' after field name")?;
            let fty = self.parse_type()?;
            let fend = self.cur_span().start;
            fields.push(StructField {
                name: fname,
                ty: fty,
                span: Span { start: fstart, end: fend },
            });
            if !self.peek_is(&Token::RBrace) {
                self.expect(Token::Comma, "expected ',' between struct fields")?;
            }
        }
        let close = self.cur_span();
        self.expect(Token::RBrace, "expected '}'")?;
        Ok(StructDecl { name, fields, span: Span { start, end: close.end } })
    }

    /// `interface Name { method-sig; ... }` — declares an abstract
    /// API surface. Each method-sig is parsed like a function
    /// signature (modifiers, params, return type) but without a
    /// body. Used together with `$value::method(args)` for
    /// dynamic dispatch.
    fn parse_interface_decl(&mut self) -> Result<InterfaceDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Interface, "expected 'interface'")?;
        let name = self.expect_ident()?;
        self.expect(Token::LBrace, "expected '{' after interface name")?;
        let mut methods = Vec::new();
        while !self.peek_is(&Token::RBrace) {
            methods.push(self.parse_interface_method()?);
        }
        let close = self.cur_span();
        self.expect(Token::RBrace, "expected '}' to close interface")?;
        Ok(InterfaceDecl { name, methods, span: Span { start, end: close.end } })
    }

    fn parse_interface_method(&mut self) -> Result<InterfaceMethod, Error> {
        let start = self.cur_span().start;
        let mut is_view = false;
        let mut is_pure = false;
        // Method modifiers — only `view` / `pure` are meaningful
        // on an interface (we don't carry `entry`/`nore` because
        // every interface method is by definition entry-callable
        // from outside the implementing module).
        loop {
            match self.peek_token() {
                Token::View => { is_view = true; self.advance(); }
                Token::Pure => { is_pure = true; self.advance(); }
                Token::Entry => { self.advance(); } // tolerated, no-op
                _ => break,
            }
        }
        if is_view && is_pure {
            return Err(Error::new(
                ErrorKind::Parse,
                "interface method cannot be both `view` and `pure`",
                self.cur_span(),
            ));
        }
        self.expect(Token::Fn, "expected 'fn' in interface method")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "expected '(' after method name")?;
        let mut params = Vec::new();
        while !self.peek_is(&Token::RParen) {
            let pstart = self.cur_span().start;
            let pname = self.expect_ident()?;
            self.expect(Token::Colon, "expected ':' after parameter name")?;
            let pty = self.parse_type()?;
            let pend = self.cur_span().start;
            params.push(Param {
                name: pname,
                ty: pty,
                span: Span { start: pstart, end: pend },
            });
            if !self.peek_is(&Token::RParen) {
                self.expect(Token::Comma, "expected ',' between parameters")?;
            }
        }
        self.expect(Token::RParen, "expected ')'")?;
        let return_type = if self.peek_is(&Token::Arrow) {
            self.advance();
            self.parse_type()?
        } else {
            Type::Unit
        };
        let semi = self.cur_span();
        self.expect(Token::Semi, "expected ';' after interface method signature")?;
        Ok(InterfaceMethod {
            name, params, return_type,
            is_view, is_pure,
            span: Span { start, end: semi.end },
        })
    }

    /// `cap Name { f: T, ... }` — same syntax as `struct`, different
    /// semantics (non-Copy + privileged construction). The duplicated
    /// loop is two-pass-readable on purpose; the divergent rules live
    /// in typeck, not the parser.
    fn parse_cap_decl(&mut self) -> Result<CapDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Cap, "expected 'cap'")?;
        let name = self.expect_ident()?;
        self.expect(Token::LBrace, "expected '{' after cap name")?;
        let mut fields = Vec::new();
        while !self.peek_is(&Token::RBrace) {
            let fstart = self.cur_span().start;
            let fname = self.expect_ident()?;
            self.expect(Token::Colon, "expected ':' after field name")?;
            let fty = self.parse_type()?;
            let fend = self.cur_span().start;
            fields.push(StructField {
                name: fname,
                ty: fty,
                span: Span { start: fstart, end: fend },
            });
            if !self.peek_is(&Token::RBrace) {
                self.expect(Token::Comma, "expected ',' between cap fields")?;
            }
        }
        let close = self.cur_span();
        self.expect(Token::RBrace, "expected '}'")?;
        Ok(CapDecl { name, fields, span: Span { start, end: close.end } })
    }

    fn parse_state(&mut self) -> Result<StateDecl, Error> {
        let start = self.cur_span().start;
        self.expect(Token::State, "expected 'state'")?;
        let name = self.expect_ident()?;
        self.expect(Token::Colon, "expected ':' after state name")?;
        let ty = self.parse_type()?;
        let semi = self.cur_span();
        self.expect(Token::Semi, "expected ';' after state declaration")?;
        Ok(StateDecl { name, ty, span: Span { start, end: semi.end } })
    }

    fn parse_import(&mut self) -> Result<Import, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Import, "expected 'import'")?;
        let name = self.expect_ident()?;
        self.expect(Token::Colon, "expected ':' after import name")?;
        self.expect(Token::Fn, "expected 'fn' in import signature")?;
        self.expect(Token::LParen, "expected '(' in import signature")?;
        let mut params = Vec::new();
        while !self.peek_is(&Token::RParen) {
            params.push(self.parse_type()?);
            if !self.peek_is(&Token::RParen) {
                self.expect(Token::Comma, "expected ',' between import param types")?;
            }
        }
        self.expect(Token::RParen, "expected ')'")?;
        let return_type = if self.peek_is(&Token::Arrow) {
            self.advance();
            self.parse_type()?
        } else {
            Type::Unit
        };
        let semi_span = self.cur_span();
        self.expect(Token::Semi, "expected ';' after import")?;
        Ok(Import {
            name,
            params,
            return_type,
            span: Span { start, end: semi_span.end },
        })
    }

    fn parse_fn(&mut self) -> Result<FnDef, Error> {
        self.parse_fn_with_modifiers(false, false, false, false)
    }

    fn parse_fn_with_modifiers(
        &mut self,
        is_entry: bool,
        is_nore: bool,
        is_view: bool,
        is_pure: bool,
    ) -> Result<FnDef, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Fn, "expected 'fn'")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "expected '(' after function name")?;
        let mut params = Vec::new();
        while !self.peek_is(&Token::RParen) {
            let pstart = self.cur_span().start;
            let pname = self.expect_ident()?;
            self.expect(Token::Colon, "expected ':' after parameter name")?;
            let pty = self.parse_type()?;
            let pend = self.cur_span().start; // approx — end of type token
            params.push(Param {
                name: pname,
                ty: pty,
                span: Span { start: pstart, end: pend },
            });
            if !self.peek_is(&Token::RParen) {
                self.expect(Token::Comma, "expected ',' between parameters")?;
            }
        }
        self.expect(Token::RParen, "expected ')'")?;
        // Optional modifier list: `[Mod1(args), Mod2, Mod3(arg)]`.
        let modifiers = if self.peek_is(&Token::LBracket) {
            self.advance();
            let mut mods = Vec::new();
            while !self.peek_is(&Token::RBracket) {
                let mname = self.expect_ident()?;
                let margs = if self.peek_is(&Token::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    while !self.peek_is(&Token::RParen) {
                        args.push(self.parse_expr()?);
                        if !self.peek_is(&Token::RParen) {
                            self.expect(Token::Comma, "expected ',' between modifier args")?;
                        }
                    }
                    self.expect(Token::RParen, "expected ')' to close modifier args")?;
                    args
                } else {
                    Vec::new()
                };
                mods.push((mname, margs));
                if !self.peek_is(&Token::RBracket) {
                    self.expect(Token::Comma, "expected ',' between modifiers")?;
                }
            }
            self.expect(Token::RBracket, "expected ']' to close modifier list")?;
            mods
        } else {
            Vec::new()
        };
        let return_type = if self.peek_is(&Token::Arrow) {
            self.advance();
            self.parse_type()?
        } else {
            Type::Unit
        };
        let body = self.parse_block()?;
        let end = body.span.end;
        Ok(FnDef {
            name,
            params,
            return_type,
            body,
            is_entry,
            is_nore,
            is_view,
            is_pure,
            modifiers,
            span: Span { start, end },
        })
    }

    fn parse_type(&mut self) -> Result<Type, Error> {
        let span = self.cur_span();
        match self.peek_token().clone() {
            Token::Set => {
                self.advance();
                self.expect(Token::Lt, "expected '<' after 'set'")?;
                let elem = self.parse_type()?;
                self.expect(Token::Gt, "expected '>' to close set<...>")?;
                Ok(Type::Set(Box::new(elem)))
            }
            Token::Dict => {
                self.advance();
                self.expect(Token::Lt, "expected '<' after 'dict'")?;
                let key = self.parse_type()?;
                self.expect(Token::Comma, "expected ',' between dict key and value")?;
                let value = self.parse_type()?;
                self.expect(Token::Gt, "expected '>' to close dict<...>")?;
                Ok(Type::Dict { key: Box::new(key), value: Box::new(value) })
            }
            Token::Ident(s) => {
                self.advance();
                match s.as_str() {
                    "i64" | "int" => Ok(Type::Int),
                    "uint" => Ok(Type::UInt),
                    "float" => Ok(Type::Float),
                    "i32" => Ok(Type::I32),
                    "u32" => Ok(Type::U32),
                    "u64" => Ok(Type::U64),
                    "u128" => Ok(Type::U128),
                    "bool" => Ok(Type::Bool),
                    "unit" => Ok(Type::Unit),
                    "Resource" => Ok(Type::Resource),
                    "string" | "str" => Ok(Type::String),
                    "Address" => Ok(Type::Address),
                    "bytes" => Ok(Type::Bytes),
                    "json" => Ok(Type::Json),
                    name if self.struct_names.contains(name) => {
                        // unresolved struct ref — typeck inlines fields
                        Ok(Type::Struct { name: name.to_string(), fields: Vec::new() })
                    }
                    name if self.enum_names.contains(name) => {
                        // unresolved enum ref — typeck inlines variants
                        Ok(Type::Enum { name: name.to_string(), variants: Vec::new() })
                    }
                    name if self.interface_names.contains(name) => {
                        // unresolved interface ref — typeck inlines methods
                        Ok(Type::Interface { name: name.to_string(), methods: Vec::new() })
                    }
                    "map" => {
                        self.expect(Token::Lt, "expected '<' after 'map'")?;
                        let key = self.parse_type()?;
                        self.expect(Token::Comma, "expected ',' between map key and value")?;
                        let value = self.parse_type()?;
                        self.expect(Token::Gt, "expected '>' to close map<...>")?;
                        Ok(Type::Map { key: Box::new(key), value: Box::new(value) })
                    }
                    "pmap" => {
                        self.expect(Token::Lt, "expected '<' after 'pmap'")?;
                        let key = self.parse_type()?;
                        self.expect(Token::Comma, "expected ',' between pmap key and value")?;
                        let value = self.parse_type()?;
                        self.expect(Token::Gt, "expected '>' to close pmap<...>")?;
                        Ok(Type::PMap { key: Box::new(key), value: Box::new(value) })
                    }
                    "pbtree" => {
                        self.expect(Token::Lt, "expected '<' after 'pbtree'")?;
                        let key = self.parse_type()?;
                        self.expect(Token::Comma, "expected ',' between pbtree key and value")?;
                        let value = self.parse_type()?;
                        self.expect(Token::Gt, "expected '>' to close pbtree<...>")?;
                        Ok(Type::PBTree { key: Box::new(key), value: Box::new(value) })
                    }
                    "pvec" => {
                        self.expect(Token::Lt, "expected '<' after 'pvec'")?;
                        let elem = self.parse_type()?;
                        self.expect(Token::Gt, "expected '>' to close pvec<...>")?;
                        Ok(Type::PVec { elem: Box::new(elem) })
                    }
                    "set" => {
                        self.expect(Token::Lt, "expected '<' after 'set'")?;
                        let elem = self.parse_type()?;
                        self.expect(Token::Gt, "expected '>' to close set<...>")?;
                        Ok(Type::Set(Box::new(elem)))
                    }
                    "dict" => {
                        self.expect(Token::Lt, "expected '<' after 'dict'")?;
                        let key = self.parse_type()?;
                        self.expect(Token::Comma, "expected ',' between dict key and value")?;
                        let value = self.parse_type()?;
                        self.expect(Token::Gt, "expected '>' to close dict<...>")?;
                        Ok(Type::Dict { key: Box::new(key), value: Box::new(value) })
                    }
                    other => Err(Error::new(
                        ErrorKind::Parse,
                        format!("unknown type '{other}'"),
                        span,
                    )),
                }
            }
            Token::LParen => {
                self.advance();
                if self.peek_is(&Token::RParen) {
                    self.advance();
                    return Ok(Type::Unit);
                }
                // `(T1, T2, ...)` tuple type — at least one comma
                // separates elements; a single `(T)` is just a
                // grouping that returns T (no semantic difference).
                let first = self.parse_type()?;
                if self.peek_is(&Token::Comma) {
                    let mut elems = vec![first];
                    while self.peek_is(&Token::Comma) {
                        self.advance();
                        if self.peek_is(&Token::RParen) { break; }
                        elems.push(self.parse_type()?);
                    }
                    self.expect(Token::RParen, "expected ')' to close tuple type")?;
                    return Ok(Type::Tuple(elems));
                }
                self.expect(Token::RParen, "expected ')'")?;
                Ok(first)
            }
            Token::LBracket => {
                self.advance();
                let elem = self.parse_type()?;
                self.expect(Token::RBracket, "expected ']' to close array type")?;
                Ok(Type::Array(Box::new(elem)))
            }
            other => Err(Error::new(
                ErrorKind::Parse,
                format!("expected type, got {other:?}"),
                span,
            )),
        }
    }

    fn parse_block(&mut self) -> Result<Block, Error> {
        let start = self.cur_span().start;
        self.expect(Token::LBrace, "expected '{'")?;
        let mut stmts = Vec::new();
        let mut tail: Option<Box<Expr>> = None;
        while !self.peek_is(&Token::RBrace) && !self.is_eof() {
            // Keyword-led statements (let/return/if/while/for/...)
            // own their own terminators; defer to parse_stmt for them.
            if Self::starts_keyword_stmt(self.peek_token()) {
                stmts.push(self.parse_stmt()?);
                continue;
            }
            // `_;` placeholder.
            if let Token::Ident(s) = self.peek_token() {
                if s == "_" && matches!(self.peek_at(1), Some(Token::Semi)) {
                    stmts.push(self.parse_stmt()?);
                    continue;
                }
            }
            // Otherwise: parse an expression. What follows decides:
            //   `=` → Stmt::Assign; `;` → Stmt::Expr; `}` → tail.
            let e = self.parse_expr()?;
            if self.peek_is(&Token::Eq) {
                self.advance();
                let value = self.parse_expr()?;
                let semi_span = self.cur_span();
                self.expect(Token::Semi, "expected ';' after assignment")?;
                stmts.push(Stmt::Assign {
                    target: e.clone(),
                    value,
                    span: Span { start: e.span.start, end: semi_span.end },
                });
            } else if self.peek_is(&Token::Semi) {
                let semi_span = self.cur_span();
                self.advance();
                stmts.push(Stmt::Expr(Expr {
                    kind: e.kind.clone(),
                    span: Span { start: e.span.start, end: semi_span.end },
                }));
            } else if self.peek_is(&Token::RBrace) {
                // Tail vs trailing statement-like block-expression
                // (e.g. `if cond { return 1; } else { return 2; }`).
                // If the expression yields a value (at least one
                // tail in its arms), treat as tail. Otherwise it's a
                // statement-shape if/match/block — emit as a Stmt
                // and let the caller close the block.
                if Self::is_expr_with_block(&e.kind)
                    && !Self::expr_with_block_yields_value(&e.kind)
                {
                    match e.kind {
                        ExprKind::If { cond, then, else_branch } => {
                            stmts.push(Stmt::If(IfStmt {
                                cond: *cond, then, else_branch, span: e.span,
                            }));
                        }
                        _ => stmts.push(Stmt::Expr(e)),
                    }
                } else {
                    tail = Some(Box::new(e));
                    break;
                }
            } else if Self::is_expr_with_block(&e.kind) {
                // `if`/`match`/`{}`-shaped expressions in statement
                // position don't need a trailing `;`. The closing
                // `}` itself terminates the statement, Rust-style.
                //
                // For `if` specifically, lower back to `Stmt::If` so
                // every downstream pass that already pattern-matches
                // on `Stmt::If` keeps working. Match and block-as-
                // expr fall through to `Stmt::Expr` (their value is
                // discarded).
                match e.kind {
                    ExprKind::If { cond, then, else_branch } => {
                        stmts.push(Stmt::If(IfStmt {
                            cond: *cond,
                            then,
                            else_branch,
                            span: e.span,
                        }));
                    }
                    _ => stmts.push(Stmt::Expr(e)),
                }
            } else {
                let span = self.cur_span();
                return Err(Error::new(
                    ErrorKind::Parse,
                    format!("expected ';', '=', or '}}' after expression, got {:?}", self.peek_token()),
                    span,
                ));
            }
        }
        let end = self.cur_span().end;
        self.expect(Token::RBrace, "expected '}'")?;
        Ok(Block { stmts, tail, span: Span { start, end } })
    }

    fn starts_keyword_stmt(t: &Token) -> bool {
        // `if` is intentionally NOT here — we always parse it as an
        // expression and let the trailing token (`;` vs `}`) decide
        // stmt-vs-tail. That way a block whose only content is an
        // `if-else` chain treats it as the tail expression.
        matches!(t,
            Token::Let | Token::Return | Token::While
            | Token::For | Token::Break | Token::Continue | Token::Emit
            | Token::Delete
        )
    }

    /// Whether an expression's syntactic form ends in `}` so it can
    /// stand alone as a statement without a trailing `;`. Used to
    /// allow `if cond { ... } let x = ...;` and similar Rust-style
    /// patterns.
    fn is_expr_with_block(kind: &ExprKind) -> bool {
        matches!(kind, ExprKind::If { .. } | ExprKind::Match { .. } | ExprKind::Block(_))
    }

    /// Whether a block-shaped expression's arms produce a tail value.
    /// Used to disambiguate `if cond { return 1; }` (statement-like —
    /// no value) from `if cond { 1 } else { 2 }` (value-yielding).
    fn expr_with_block_yields_value(kind: &ExprKind) -> bool {
        match kind {
            ExprKind::Block(b) => b.tail.is_some(),
            ExprKind::If { then, else_branch, .. } => {
                if then.tail.is_some() { return true; }
                match else_branch {
                    ElseBranch::None => false,
                    ElseBranch::Block(b) => b.tail.is_some(),
                    ElseBranch::If(inner) => Self::expr_with_block_yields_value(&ExprKind::If {
                        cond: Box::new(inner.cond.clone()),
                        then: inner.then.clone(),
                        else_branch: inner.else_branch.clone(),
                    }),
                }
            }
            ExprKind::Match { arms, .. } => {
                // Match always yields a value — the user explicitly
                // chose `match` rather than statement-form dispatch.
                !arms.is_empty()
            }
            _ => false,
        }
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset).map(|s| &s.token)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, Error> {
        match self.peek_token() {
            Token::Let => self.parse_let(),
            Token::Return => self.parse_return(),
            Token::If => Ok(Stmt::If(self.parse_if_stmt()?)),
            Token::While => self.parse_while(),
            Token::For => self.parse_for(),
            Token::Break => {
                let span = self.cur_span();
                self.advance();
                self.expect(Token::Semi, "expected ';' after 'break'")?;
                Ok(Stmt::Break(span))
            }
            Token::Continue => {
                let span = self.cur_span();
                self.advance();
                self.expect(Token::Semi, "expected ';' after 'continue'")?;
                Ok(Stmt::Continue(span))
            }
            Token::Emit => self.parse_emit(),
            Token::Delete => self.parse_delete(),
            // `_;` — modifier-body placeholder. The `_` token is
            // lexed as Ident("_"); recognize it here only when
            // followed by `;` so plain identifiers like `_` (or
            // `let _x = ...`) keep working.
            Token::Ident(s) if s == "_" => {
                let saved = self.pos;
                let span = self.cur_span();
                self.advance();
                if self.peek_is(&Token::Semi) {
                    self.advance();
                    return Ok(Stmt::Placeholder(span));
                }
                self.pos = saved;
                let expr = self.parse_expr()?;
                let semi_span = self.cur_span();
                self.expect(Token::Semi, "expected ';' after expression")?;
                let span = Span { start: expr.span.start, end: semi_span.end };
                Ok(Stmt::Expr(Expr { kind: expr.kind, span }))
            }
            _ => {
                let expr = self.parse_expr()?;
                if self.peek_is(&Token::Eq) {
                    self.advance();
                    let value = self.parse_expr()?;
                    let semi = self.cur_span();
                    self.expect(Token::Semi, "expected ';' after assignment")?;
                    Ok(Stmt::Assign {
                        target: expr.clone(),
                        value,
                        span: Span { start: expr.span.start, end: semi.end },
                    })
                } else {
                    let semi_span = self.cur_span();
                    self.expect(Token::Semi, "expected ';' after expression")?;
                    let span = Span { start: expr.span.start, end: semi_span.end };
                    Ok(Stmt::Expr(Expr { kind: expr.kind, span }))
                }
            }
        }
    }

    /// `match scrut { pattern => arm_expr, ... }` — sum-type
    /// dispatch as an expression. Each arm body is a single
    /// expression; multi-stmt bodies are written `=> { ...; result }`
    /// once we add block-as-expression — for now use a let-bound
    /// helper if you need multiple statements.
    fn parse_match_expr(&mut self) -> Result<Expr, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Match, "expected 'match'")?;
        // The scrutinee may include trailing braces only if they're
        // a struct literal — but the `{` we want to consume is the
        // arms-list brace, not a literal. Suppress struct literals
        // in the scrutinee position so this isn't ambiguous.
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let scrut = self.parse_expr()?;
        self.no_struct_literal = saved;
        self.expect(Token::LBrace, "expected '{' to open match arms")?;
        let mut arms = Vec::new();
        while !self.peek_is(&Token::RBrace) {
            let arm_start = self.cur_span().start;
            let pattern = self.parse_match_pattern()?;
            self.expect(Token::FatArrow, "expected '=>' after match pattern")?;
            let body = self.parse_expr()?;
            let arm_end = self.tokens[self.pos.saturating_sub(1)].span.end;
            arms.push(MatchArm {
                pattern,
                body,
                span: Span { start: arm_start, end: arm_end },
            });
            if !self.peek_is(&Token::RBrace) {
                self.expect(Token::Comma, "expected ',' between match arms")?;
            }
        }
        let close = self.cur_span();
        self.expect(Token::RBrace, "expected '}' to close match")?;
        Ok(Expr {
            kind: ExprKind::Match { scrut: Box::new(scrut), arms },
            span: Span { start, end: close.end },
        })
    }

    fn parse_match_pattern(&mut self) -> Result<MatchPattern, Error> {
        // Wildcard `_`.
        if let Token::Ident(s) = self.peek_token() {
            if s == "_" {
                self.advance();
                return Ok(MatchPattern::Wildcard);
            }
        }
        // `EnumName::Variant` or `EnumName::Variant(b1, b2)`.
        let enum_name = self.expect_ident()?;
        self.expect(Token::ColonColon, "expected '::' in match pattern")?;
        let variant = self.expect_ident()?;
        let bindings = if self.peek_is(&Token::LParen) {
            self.advance();
            let mut names = Vec::new();
            while !self.peek_is(&Token::RParen) {
                names.push(self.expect_ident()?);
                if !self.peek_is(&Token::RParen) {
                    self.expect(Token::Comma, "expected ',' between pattern bindings")?;
                }
            }
            self.expect(Token::RParen, "expected ')'")?;
            names
        } else {
            Vec::new()
        };
        Ok(MatchPattern::EnumVariant { enum_name, variant, bindings })
    }

    /// `delete state[k];` — remove an entry from a pmap/pbtree state.
    /// The target is parsed as an expression so future extensions
    /// (struct field paths, nested state) can plug in without a
    /// syntax change. Typeck enforces the slot kind.
    fn parse_delete(&mut self) -> Result<Stmt, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Delete, "expected 'delete'")?;
        let target = self.parse_expr()?;
        let semi = self.cur_span();
        self.expect(Token::Semi, "expected ';' after delete statement")?;
        Ok(Stmt::Delete { target, span: Span { start, end: semi.end } })
    }

    /// `emit Foo(arg, ...);`
    fn parse_emit(&mut self) -> Result<Stmt, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Emit, "expected 'emit'")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "expected '(' after event name")?;
        let mut args = Vec::new();
        while !self.peek_is(&Token::RParen) {
            args.push(self.parse_expr()?);
            if !self.peek_is(&Token::RParen) {
                self.expect(Token::Comma, "expected ',' between emit args")?;
            }
        }
        self.expect(Token::RParen, "expected ')' to close emit")?;
        let semi = self.cur_span();
        self.expect(Token::Semi, "expected ';' after emit statement")?;
        Ok(Stmt::Emit { name, args, span: Span { start, end: semi.end } })
    }

    fn parse_let(&mut self) -> Result<Stmt, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Let, "expected 'let'")?;
        // `let (a, b, ...) = expr;` — tuple destructuring. Each name
        // is bound to the corresponding tuple index. No type
        // annotations on destructure patterns (yet).
        if self.peek_is(&Token::LParen) {
            self.advance();
            let mut names = Vec::new();
            while !self.peek_is(&Token::RParen) {
                names.push(self.expect_ident()?);
                if !self.peek_is(&Token::RParen) {
                    self.expect(Token::Comma, "expected ',' between destructure names")?;
                }
            }
            self.expect(Token::RParen, "expected ')' to close destructure")?;
            self.expect(Token::Eq, "expected '=' after destructure pattern")?;
            let value = self.parse_expr()?;
            let semi_span = self.cur_span();
            self.expect(Token::Semi, "expected ';'")?;
            return Ok(Stmt::LetTuple {
                names,
                value,
                span: Span { start, end: semi_span.end },
            });
        }
        let name = self.expect_ident()?;
        let ty = if self.peek_is(&Token::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(Token::Eq, "expected '=' after 'let' name")?;
        let value = self.parse_expr()?;
        let semi_span = self.cur_span();
        self.expect(Token::Semi, "expected ';'")?;
        Ok(Stmt::Let {
            name,
            ty,
            value,
            span: Span { start, end: semi_span.end },
        })
    }

    fn parse_return(&mut self) -> Result<Stmt, Error> {
        let start = self.cur_span().start;
        self.expect(Token::Return, "expected 'return'")?;
        let value = if self.peek_is(&Token::Semi) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        let semi_span = self.cur_span();
        self.expect(Token::Semi, "expected ';' after return")?;
        Ok(Stmt::Return { value, span: Span { start, end: semi_span.end } })
    }

    fn parse_while(&mut self) -> Result<Stmt, Error> {
        let start = self.cur_span().start;
        self.expect(Token::While, "expected 'while'")?;
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let cond = self.parse_expr()?;
        self.no_struct_literal = saved;
        let body = self.parse_block()?;
        let end = body.span.end;
        Ok(Stmt::While { cond, body, span: Span { start, end } })
    }

    fn parse_for(&mut self) -> Result<Stmt, Error> {
        let start = self.cur_span().start;
        self.expect(Token::For, "expected 'for'")?;
        let var = self.expect_ident()?;
        self.expect(Token::In, "expected 'in' in for-in loop")?;
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let first = self.parse_expr()?;
        // Range form: `start..end` or `start..=end`. Detected after
        // parsing the first sub-expression; if no `..` follows, the
        // first expression IS the iter (array case).
        if self.peek_is(&Token::DotDot) || self.peek_is(&Token::DotDotEq) {
            let inclusive = self.peek_is(&Token::DotDotEq);
            self.advance();
            let end_expr = self.parse_expr()?;
            self.no_struct_literal = saved;
            let body = self.parse_block()?;
            let end = body.span.end;
            return Ok(Stmt::ForRange {
                var,
                start: first,
                end: end_expr,
                inclusive,
                body,
                span: Span { start, end },
            });
        }
        self.no_struct_literal = saved;
        let body = self.parse_block()?;
        let end = body.span.end;
        Ok(Stmt::For { var, iter: first, body, span: Span { start, end } })
    }

    /// `if cond { ... } else { ... }` as an expression. Both arms
    /// must be present and yield the same type. The body of each arm
    /// is a Block — so multi-statement bodies with a trailing tail
    /// expression work out of the box.
    fn parse_if_expr(&mut self) -> Result<Expr, Error> {
        let start = self.cur_span().start;
        let if_stmt = self.parse_if_stmt()?;
        let span = Span { start, end: if_stmt.span.end };
        Ok(Expr {
            kind: ExprKind::If {
                cond: Box::new(if_stmt.cond),
                then: if_stmt.then,
                else_branch: if_stmt.else_branch,
            },
            span,
        })
    }

    fn parse_if_stmt(&mut self) -> Result<IfStmt, Error> {
        let start = self.cur_span().start;
        self.expect(Token::If, "expected 'if'")?;
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let cond = self.parse_expr()?;
        self.no_struct_literal = saved;
        let then = self.parse_block()?;
        let mut end = then.span.end;
        let else_branch = if self.peek_is(&Token::Else) {
            self.advance();
            if self.peek_is(&Token::If) {
                let inner = self.parse_if_stmt()?;
                end = inner.span.end;
                ElseBranch::If(Box::new(inner))
            } else {
                let b = self.parse_block()?;
                end = b.span.end;
                ElseBranch::Block(b)
            }
        } else {
            ElseBranch::None
        };
        Ok(IfStmt { cond, then, else_branch, span: Span { start, end } })
    }

    // Expression parsing with binding-power based Pratt.

    fn parse_expr(&mut self) -> Result<Expr, Error> {
        self.parse_expr_bp(0)
    }

    /// Parse one or more comprehension clauses. The caller has already
    /// confirmed the next token is `for`. Each `For`/`If` expression is
    /// parsed with struct-literal suppression so trailing braces don't get
    /// swallowed by a struct-literal disambiguation.
    fn parse_comp_clauses(&mut self) -> Result<Vec<CompClause>, Error> {
        let mut clauses = Vec::new();
        loop {
            if self.peek_is(&Token::For) {
                self.advance();
                let var = self.expect_ident()?;
                self.expect(Token::In, "expected 'in' in comprehension")?;
                let saved = self.no_struct_literal;
                self.no_struct_literal = true;
                let iter = self.parse_expr()?;
                self.no_struct_literal = saved;
                clauses.push(CompClause::For { var, iter });
            } else if self.peek_is(&Token::If) {
                self.advance();
                let saved = self.no_struct_literal;
                self.no_struct_literal = true;
                let cond = self.parse_expr()?;
                self.no_struct_literal = saved;
                clauses.push(CompClause::If(cond));
            } else {
                break;
            }
        }
        Ok(clauses)
    }

    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr, Error> {
        let mut lhs = self.parse_prefix()?;
        loop {
            // Pipe `|>` has the lowest precedence: a fully-formed expression
            // sits on each side, no other binop steals the rhs.
            if self.peek_is(&Token::PipeArrow) {
                let lbp = 1u8;
                let rbp = 2u8;
                if lbp < min_bp { break; }
                self.advance();
                let rhs = self.parse_expr_bp(rbp)?;
                let span = Span { start: lhs.span.start, end: rhs.span.end };
                lhs = Expr {
                    kind: ExprKind::Pipe {
                        head: Box::new(lhs),
                        step: Box::new(rhs),
                    },
                    span,
                };
                continue;
            }
            // Precedence ladder, low → high. Bitwise sits between
            // comparison and shift/arithmetic, mirroring Rust/C.
            let (op, lbp, rbp) = match self.peek_token() {
                Token::PipePipe => (BinOp::Or,    1, 2),
                Token::AmpAmp   => (BinOp::And,   3, 4),
                Token::EqEq     => (BinOp::Eq,    5, 6),
                Token::BangEq   => (BinOp::NotEq, 5, 6),
                Token::Lt       => (BinOp::Lt,    7, 8),
                Token::Gt       => (BinOp::Gt,    7, 8),
                Token::LtEq     => (BinOp::LtEq,  7, 8),
                Token::GtEq     => (BinOp::GtEq,  7, 8),
                Token::PipeBar  => (BinOp::BitOr,  9, 10),
                Token::Caret    => (BinOp::BitXor, 11, 12),
                Token::Amp      => (BinOp::BitAnd, 13, 14),
                Token::Shl      => (BinOp::Shl,    15, 16),
                Token::Shr      => (BinOp::Shr,    15, 16),
                Token::Plus     => (BinOp::Add,   17, 18),
                Token::Minus    => (BinOp::Sub,   17, 18),
                Token::Star     => (BinOp::Mul,  19, 20),
                Token::Slash    => (BinOp::Div,  19, 20),
                Token::Percent  => (BinOp::Mod,  19, 20),
                _ => break,
            };
            if lbp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.parse_expr_bp(rbp)?;
            let span = Span { start: lhs.span.start, end: rhs.span.end };
            lhs = Expr {
                kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_prefix(&mut self) -> Result<Expr, Error> {
        let start = self.cur_span().start;
        match self.peek_token() {
            Token::Minus => {
                self.advance();
                let operand = self.parse_prefix()?;
                let end = operand.span.end;
                Ok(Expr {
                    kind: ExprKind::Unary { op: UnOp::Neg, operand: Box::new(operand) },
                    span: Span { start, end },
                })
            }
            Token::Bang => {
                self.advance();
                let operand = self.parse_prefix()?;
                let end = operand.span.end;
                Ok(Expr {
                    kind: ExprKind::Unary { op: UnOp::Not, operand: Box::new(operand) },
                    span: Span { start, end },
                })
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, Error> {
        let mut e = self.parse_primary()?;
        loop {
            if self.peek_is(&Token::LBracket) {
                self.advance();
                let saved = self.no_struct_literal;
                self.no_struct_literal = false;
                let key = self.parse_expr()?;
                self.no_struct_literal = saved;
                let close_span = self.cur_span();
                self.expect(Token::RBracket, "expected ']'")?;
                let span = Span { start: e.span.start, end: close_span.end };
                e = Expr {
                    kind: ExprKind::Index {
                        target: Box::new(e),
                        key: Box::new(key),
                    },
                    span,
                };
            } else if self.peek_is(&Token::Arrow) {
                // JSON path access: `expr -> ident`, `expr -> "lit"`,
                // or `expr -> [n]`. Desugars to builtin calls so the
                // type system, compile pass, and VM all reuse the
                // existing function-call machinery.
                self.advance();
                let start = e.span.start;
                if self.peek_is(&Token::LBracket) {
                    self.advance();
                    let idx = self.parse_expr()?;
                    let close = self.cur_span();
                    self.expect(Token::RBracket, "expected ']' after JSON array index")?;
                    let span = Span { start, end: close.end };
                    e = Expr {
                        kind: ExprKind::Call {
                            module: None,
                            name: "json_get_index".to_string(),
                            args: vec![e, idx],
                        },
                        span,
                    };
                } else {
                    // Either a quoted string literal or a bare ident
                    // (treated as a string key). Computed-key access
                    // goes through `json_get_field(j, key_expr)` directly.
                    let (key_str, end) = match self.peek_token().clone() {
                        Token::Str(s) => {
                            let sp = self.cur_span();
                            self.advance();
                            (s, sp.end)
                        }
                        Token::Ident(name) => {
                            let sp = self.cur_span();
                            self.advance();
                            (name, sp.end)
                        }
                        other => return Err(Error::new(
                            ErrorKind::Parse,
                            format!(
                                "expected ident, string literal, or '[' after '->', got {other:?}",
                            ),
                            self.cur_span(),
                        )),
                    };
                    let key_expr = Expr {
                        kind: ExprKind::Str(key_str),
                        span: Span { start, end },
                    };
                    let span = Span { start, end };
                    e = Expr {
                        kind: ExprKind::Call {
                            module: None,
                            name: "json_get_field".to_string(),
                            args: vec![e, key_expr],
                        },
                        span,
                    };
                }
            } else if self.peek_is(&Token::Dot) {
                self.advance();
                // `expr.0` / `expr.1` — tuple index. `expr.name` —
                // struct field. Disambiguated by the next token.
                if let Token::Int(n) = self.peek_token().clone() {
                    use num_traits::ToPrimitive;
                    let idx = match n.to_usize() {
                        Some(v) => v,
                        None => return Err(Error::new(
                            ErrorKind::Parse,
                            "tuple index out of range or negative",
                            self.cur_span(),
                        )),
                    };
                    self.advance();
                    let end = self.tokens[self.pos.saturating_sub(1)].span.end;
                    let span = Span { start: e.span.start, end };
                    e = Expr {
                        kind: ExprKind::TupleIndex {
                            target: Box::new(e),
                            index: idx,
                        },
                        span,
                    };
                    continue;
                }
                let field = self.expect_ident()?;
                let end = self.tokens[self.pos.saturating_sub(1)].span.end;
                let span = Span { start: e.span.start, end };
                e = Expr {
                    kind: ExprKind::Field { target: Box::new(e), name: field },
                    span,
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, Error> {
        let span = self.cur_span();
        match self.peek_token().clone() {
            Token::Int(n) => {
                self.advance();
                Ok(Expr { kind: ExprKind::Int(n), span })
            }
            Token::UInt(n) => {
                self.advance();
                Ok(Expr { kind: ExprKind::UInt(n), span })
            }
            Token::Float(n) => {
                self.advance();
                Ok(Expr { kind: ExprKind::Float(n), span })
            }
            Token::I32(n) => {
                self.advance();
                Ok(Expr { kind: ExprKind::I32(n), span })
            }
            Token::U32(n) => {
                self.advance();
                Ok(Expr { kind: ExprKind::U32(n), span })
            }
            Token::U64(n) => {
                self.advance();
                Ok(Expr { kind: ExprKind::U64(n), span })
            }
            Token::U128(n) => {
                self.advance();
                Ok(Expr { kind: ExprKind::U128(n), span })
            }
            Token::DollarDollar => {
                self.advance();
                Ok(Expr { kind: ExprKind::Prev, span })
            }
            Token::Dollar => {
                // `$ident::method(args)` — dynamic dispatch
                // through an interface value bound to `ident`.
                let start = span.start;
                self.advance();
                let target_ident = self.expect_ident()?;
                self.expect(Token::ColonColon, "expected '::' after '$<ident>' in dynamic dispatch")?;
                let method = self.expect_ident()?;
                self.expect(Token::LParen, "expected '(' in dynamic dispatch args")?;
                let mut args = Vec::new();
                while !self.peek_is(&Token::RParen) {
                    args.push(self.parse_expr()?);
                    if !self.peek_is(&Token::RParen) {
                        self.expect(Token::Comma, "expected ',' between args")?;
                    }
                }
                let close = self.cur_span();
                self.expect(Token::RParen, "expected ')'")?;
                Ok(Expr {
                    kind: ExprKind::DynCall {
                        target_ident, method, args,
                        // Conservative defaults — typeck fills
                        // these in once it resolves the interface
                        // method.
                        method_is_view: false,
                        method_is_pure: false,
                    },
                    span: Span { start, end: close.end },
                })
            }
            Token::Set => {
                let start = span.start;
                self.advance();
                self.expect(Token::LBrace, "expected '{' after 'set'")?;
                if self.peek_is(&Token::RBrace) {
                    let close = self.cur_span();
                    self.advance();
                    return Ok(Expr {
                        kind: ExprKind::SetLit(Vec::new()),
                        span: Span { start, end: close.end },
                    });
                }
                let saved = self.no_struct_literal;
                self.no_struct_literal = false;
                let first = self.parse_expr()?;
                if self.peek_is(&Token::For) {
                    let clauses = self.parse_comp_clauses()?;
                    let close = self.cur_span();
                    self.expect(Token::RBrace, "expected '}' to close set comprehension")?;
                    self.no_struct_literal = saved;
                    return Ok(Expr {
                        kind: ExprKind::SetComp {
                            mapper: Box::new(first),
                            clauses,
                        },
                        span: Span { start, end: close.end },
                    });
                }
                let mut elems = vec![first];
                while self.peek_is(&Token::Comma) {
                    self.advance();
                    if self.peek_is(&Token::RBrace) { break; }
                    elems.push(self.parse_expr()?);
                }
                let close = self.cur_span();
                self.expect(Token::RBrace, "expected '}' to close set literal")?;
                self.no_struct_literal = saved;
                Ok(Expr {
                    kind: ExprKind::SetLit(elems),
                    span: Span { start, end: close.end },
                })
            }
            Token::Dict => {
                let start = span.start;
                self.advance();
                self.expect(Token::LBrace, "expected '{' after 'dict'")?;
                if self.peek_is(&Token::RBrace) {
                    let close = self.cur_span();
                    self.advance();
                    return Ok(Expr {
                        kind: ExprKind::DictLit(Vec::new()),
                        span: Span { start, end: close.end },
                    });
                }
                let saved = self.no_struct_literal;
                self.no_struct_literal = false;
                // Parse first key
                let first_key = self.parse_expr()?;
                self.expect(Token::Colon, "expected ':' between dict key and value")?;
                let first_val = self.parse_expr()?;
                if self.peek_is(&Token::For) {
                    let clauses = self.parse_comp_clauses()?;
                    let close = self.cur_span();
                    self.expect(Token::RBrace, "expected '}' to close dict comprehension")?;
                    self.no_struct_literal = saved;
                    return Ok(Expr {
                        kind: ExprKind::DictComp {
                            key: Box::new(first_key),
                            value: Box::new(first_val),
                            clauses,
                        },
                        span: Span { start, end: close.end },
                    });
                }
                let mut pairs = vec![(first_key, first_val)];
                while self.peek_is(&Token::Comma) {
                    self.advance();
                    if self.peek_is(&Token::RBrace) { break; }
                    let k = self.parse_expr()?;
                    self.expect(Token::Colon, "expected ':' between dict key and value")?;
                    let v = self.parse_expr()?;
                    pairs.push((k, v));
                }
                let close = self.cur_span();
                self.expect(Token::RBrace, "expected '}' to close dict literal")?;
                self.no_struct_literal = saved;
                Ok(Expr {
                    kind: ExprKind::DictLit(pairs),
                    span: Span { start, end: close.end },
                })
            }
            Token::Str(s) => {
                self.advance();
                Ok(Expr { kind: ExprKind::Str(s), span })
            }
            Token::True => {
                self.advance();
                Ok(Expr { kind: ExprKind::Bool(true), span })
            }
            Token::False => {
                self.advance();
                Ok(Expr { kind: ExprKind::Bool(false), span })
            }
            Token::Match => self.parse_match_expr(),
            Token::LBrace => {
                // Block-as-expression: `{ stmts; tail }`. Distinct
                // from struct literals because those start with an
                // ident (`Foo { ... }`).
                let block_start = self.cur_span().start;
                let block = self.parse_block()?;
                Ok(Expr {
                    kind: ExprKind::Block(block),
                    span: Span { start: block_start, end: self.tokens[self.pos.saturating_sub(1)].span.end },
                })
            }
            Token::If => self.parse_if_expr(),
            Token::Ident(name) => {
                self.advance();
                if self.peek_is(&Token::ColonColon) {
                    self.advance();
                    let callee = self.expect_ident()?;
                    // If `name` is a known enum, parse as a variant
                    // constructor — parens are optional for unit
                    // variants. Otherwise treat as cross-module call,
                    // which still requires parens.
                    if self.enum_names.contains(&name) {
                        let args = if self.peek_is(&Token::LParen) {
                            self.advance();
                            let saved = self.no_struct_literal;
                            self.no_struct_literal = false;
                            let mut args = Vec::new();
                            while !self.peek_is(&Token::RParen) {
                                args.push(self.parse_expr()?);
                                if !self.peek_is(&Token::RParen) {
                                    self.expect(Token::Comma, "expected ',' between variant args")?;
                                }
                            }
                            self.expect(Token::RParen, "expected ')'")?;
                            self.no_struct_literal = saved;
                            args
                        } else {
                            Vec::new()
                        };
                        let end = self.tokens[self.pos.saturating_sub(1)].span.end;
                        return Ok(Expr {
                            kind: ExprKind::EnumCtor {
                                enum_name: name,
                                variant: callee,
                                args,
                            },
                            span: Span { start: span.start, end },
                        });
                    }
                    self.expect(Token::LParen, "expected '(' after '::name'")?;
                    let saved = self.no_struct_literal;
                    self.no_struct_literal = false;
                    let mut args = Vec::new();
                    while !self.peek_is(&Token::RParen) {
                        args.push(self.parse_expr()?);
                        if !self.peek_is(&Token::RParen) {
                            self.expect(Token::Comma, "expected ',' between args")?;
                        }
                    }
                    let end = self.cur_span().end;
                    self.expect(Token::RParen, "expected ')'")?;
                    self.no_struct_literal = saved;
                    Ok(Expr {
                        kind: ExprKind::Call {
                            module: Some(name),
                            name: callee,
                            args,
                        },
                        span: Span { start: span.start, end },
                    })
                } else if self.peek_is(&Token::LParen) {
                    let saved = self.no_struct_literal;
                    self.no_struct_literal = false;
                    self.advance();
                    let mut args = Vec::new();
                    while !self.peek_is(&Token::RParen) {
                        args.push(self.parse_expr()?);
                        if !self.peek_is(&Token::RParen) {
                            self.expect(Token::Comma, "expected ',' between args")?;
                        }
                    }
                    let end = self.cur_span().end;
                    self.expect(Token::RParen, "expected ')'")?;
                    self.no_struct_literal = saved;
                    Ok(Expr {
                        kind: ExprKind::Call { module: None, name, args },
                        span: Span { start: span.start, end },
                    })
                } else if self.peek_is(&Token::LBrace)
                    && !self.no_struct_literal
                    && self.struct_names.contains(&name)
                {
                    let saved = self.no_struct_literal;
                    self.no_struct_literal = false;
                    self.advance();
                    let mut fields = Vec::new();
                    while !self.peek_is(&Token::RBrace) {
                        let fname = self.expect_ident()?;
                        self.expect(Token::Colon, "expected ':' in struct literal")?;
                        let fval = self.parse_expr()?;
                        fields.push((fname, fval));
                        if !self.peek_is(&Token::RBrace) {
                            self.expect(Token::Comma, "expected ',' between struct fields")?;
                        }
                    }
                    let close = self.cur_span();
                    self.expect(Token::RBrace, "expected '}'")?;
                    self.no_struct_literal = saved;
                    Ok(Expr {
                        kind: ExprKind::StructLit { name, fields },
                        span: Span { start: span.start, end: close.end },
                    })
                } else {
                    Ok(Expr { kind: ExprKind::Ident(name), span })
                }
            }
            Token::LParen => {
                let start = self.cur_span().start;
                self.advance();
                let first = self.parse_expr()?;
                if self.peek_is(&Token::Comma) {
                    // Tuple literal — at least one comma.
                    let mut elems = vec![first];
                    while self.peek_is(&Token::Comma) {
                        self.advance();
                        if self.peek_is(&Token::RParen) { break; }
                        elems.push(self.parse_expr()?);
                    }
                    let close = self.cur_span();
                    self.expect(Token::RParen, "expected ')' to close tuple")?;
                    return Ok(Expr {
                        kind: ExprKind::TupleLit(elems),
                        span: Span { start, end: close.end },
                    });
                }
                self.expect(Token::RParen, "expected ')'")?;
                Ok(first)
            }
            Token::LBracket => {
                let start = span.start;
                self.advance();
                if self.peek_is(&Token::RBracket) {
                    let close = self.cur_span();
                    self.advance();
                    return Ok(Expr {
                        kind: ExprKind::Array(Vec::new()),
                        span: Span { start, end: close.end },
                    });
                }
                let first = self.parse_expr()?;
                // List comprehension: [expr <clause>+] where each clause is
                // `for x in iter` or `if cond`.
                if self.peek_is(&Token::For) {
                    let clauses = self.parse_comp_clauses()?;
                    let close = self.cur_span();
                    self.expect(Token::RBracket, "expected ']' to close list comprehension")?;
                    return Ok(Expr {
                        kind: ExprKind::ListComp {
                            mapper: Box::new(first),
                            clauses,
                        },
                        span: Span { start, end: close.end },
                    });
                }
                // Regular array literal.
                let mut elems = vec![first];
                while self.peek_is(&Token::Comma) {
                    self.advance();
                    if self.peek_is(&Token::RBracket) { break; }
                    elems.push(self.parse_expr()?);
                }
                let close = self.cur_span();
                self.expect(Token::RBracket, "expected ']'")?;
                Ok(Expr {
                    kind: ExprKind::Array(elems),
                    span: Span { start, end: close.end },
                })
            }
            other => Err(Error::new(
                ErrorKind::Parse,
                format!("unexpected token {other:?}"),
                span,
            )),
        }
    }

    // Token cursor helpers.

    fn peek_token(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    fn cur_span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn peek_is(&self, t: &Token) -> bool {
        std::mem::discriminant(self.peek_token()) == std::mem::discriminant(t)
    }

    fn advance(&mut self) {
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn expect(&mut self, t: Token, msg: &str) -> Result<(), Error> {
        if self.peek_is(&t) {
            self.advance();
            Ok(())
        } else {
            let got = self.peek_token().clone();
            Err(Error::new(
                ErrorKind::Parse,
                format!("{msg} (got {got:?})"),
                self.cur_span(),
            ))
        }
    }

    fn expect_ident(&mut self) -> Result<String, Error> {
        match self.peek_token().clone() {
            Token::Ident(s) => {
                self.advance();
                Ok(s)
            }
            other => Err(Error::new(
                ErrorKind::Parse,
                format!("expected identifier, got {other:?}"),
                self.cur_span(),
            )),
        }
    }

    fn is_eof(&self) -> bool {
        matches!(self.peek_token(), Token::Eof)
    }
}

pub fn parse(tokens: Vec<Spanned>) -> Result<Module, Error> {
    Parser::new(tokens).parse_module()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_str(src: &str) -> Result<Module, Error> {
        parse(tokenize(src).unwrap())
    }

    #[test]
    fn empty_module() {
        let m = parse_str("").unwrap();
        assert!(m.functions.is_empty());
    }

    #[test]
    fn empty_function() {
        let m = parse_str("fn main() {}").unwrap();
        assert_eq!(m.functions.len(), 1);
        assert_eq!(m.functions[0].name, "main");
        assert!(m.functions[0].params.is_empty());
        assert_eq!(m.functions[0].return_type, Type::Unit);
        assert!(m.functions[0].body.stmts.is_empty());
    }

    #[test]
    fn function_with_typed_params_and_return() {
        let m = parse_str("fn add(a: i64, b: i64) -> i64 { return a + b; }").unwrap();
        assert_eq!(m.functions[0].params.len(), 2);
        assert_eq!(m.functions[0].params[0].name, "a");
        assert_eq!(m.functions[0].params[0].ty, Type::Int);
        assert_eq!(m.functions[0].params[1].ty, Type::Int);
        assert_eq!(m.functions[0].return_type, Type::Int);
    }

    #[test]
    fn unknown_type_is_parse_error() {
        assert!(parse_str("fn f(x: bogus) {}").is_err());
    }

    #[test]
    fn missing_colon_in_param_is_parse_error() {
        assert!(parse_str("fn f(a) {}").is_err());
    }

    #[test]
    fn precedence_mul_over_add() {
        let m = parse_str("fn f() -> i64 { return 1 + 2 * 3; }").unwrap();
        let stmt = &m.functions[0].body.stmts[0];
        let Stmt::Return { value: Some(e), .. } = stmt else { panic!() };
        let ExprKind::Binary { op: BinOp::Add, lhs, rhs } = &e.kind else { panic!() };
        assert!(matches!(&lhs.kind, ExprKind::Int(n) if n.to_string() == "1"));
        let ExprKind::Binary { op: BinOp::Mul, .. } = &rhs.kind else { panic!() };
    }

    #[test]
    fn parens_override_precedence() {
        let m = parse_str("fn f() -> i64 { return (1 + 2) * 3; }").unwrap();
        let Stmt::Return { value: Some(e), .. } = &m.functions[0].body.stmts[0] else { panic!() };
        let ExprKind::Binary { op: BinOp::Mul, lhs, .. } = &e.kind else { panic!() };
        let ExprKind::Binary { op: BinOp::Add, .. } = &lhs.kind else { panic!() };
    }

    #[test]
    fn unary_minus() {
        let m = parse_str("fn f() -> i64 { return -5; }").unwrap();
        let Stmt::Return { value: Some(e), .. } = &m.functions[0].body.stmts[0] else { panic!() };
        let ExprKind::Unary { op: UnOp::Neg, .. } = &e.kind else { panic!() };
    }

    #[test]
    fn if_else_chain() {
        let src = "fn f(x: i64) -> i64 {
            if x == 0 { return 1; }
            else if x == 1 { return 2; }
            else { return 3; }
        }";
        let m = parse_str(src).unwrap();
        let Stmt::If(top) = &m.functions[0].body.stmts[0] else { panic!() };
        let ElseBranch::If(_) = &top.else_branch else { panic!() };
    }

    #[test]
    fn call_with_args() {
        let m = parse_str("fn f() -> i64 { return add(1, 2 + 3); }").unwrap();
        let Stmt::Return { value: Some(e), .. } = &m.functions[0].body.stmts[0] else { panic!() };
        let ExprKind::Call { name, args, .. } = &e.kind else { panic!() };
        assert_eq!(name, "add");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn missing_semicolon_is_parse_error() {
        assert!(parse_str("fn f() -> i64 { let x = 1 }").is_err());
    }

    #[test]
    fn missing_brace_is_parse_error() {
        assert!(parse_str("fn f() -> i64 return 1;").is_err());
    }
}
