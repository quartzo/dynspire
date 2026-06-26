//! Recursive-descent parser for `.dspi` files.
//!
//! Consumes a token stream from [`crate::lexer`] and produces an [`Interface`]
//! AST. Errors carry line/column for actionable messages.

use std::fmt;

use crate::ast::*;
use crate::lexer::{Lexer, Token, TokenKind};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

// Convenience: peek / consume / expect

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn current(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos];
        if !self.at_eof() {
            self.pos += 1;
        }
        tok
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        let tok = self.current();
        ParseError {
            line: tok.line,
            col: tok.col,
            msg: msg.into(),
        }
    }

    fn err_at(&self, tok: &Token, msg: impl Into<String>) -> ParseError {
        ParseError {
            line: tok.line,
            col: tok.col,
            msg: msg.into(),
        }
    }

    /// Consume the current token if it matches `expected`, otherwise error.
    fn eat(&mut self, expected: TokenKind) -> Result<()> {
        if *self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(self.err(format!(
                "expected {}, found {}",
                expected.kind_name(),
                self.peek().kind_name(),
            )))
        }
    }

    /// Like [`eat`][Self::eat] but returns the consumed token's span info.
    fn expect(&mut self, expected: TokenKind) -> Result<Token> {
        if *self.peek() == expected {
            let tok = self.current().clone();
            self.advance();
            Ok(tok)
        } else {
            Err(self.err(format!(
                "expected {}, found {}",
                expected.kind_name(),
                self.peek().kind_name(),
            )))
        }
    }

    /// If the current token matches `expected`, consume it and return `true`.
    fn check_eat(&mut self, expected: &TokenKind) -> bool {
        if self.peek() == expected {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Read an identifier, or error.
    fn expect_ident(&mut self) -> Result<String> {
        match self.peek() {
            TokenKind::Ident(s) => {
                let name = s.clone();
                self.advance();
                Ok(name)
            }
            other => Err(self.err(format!(
                "expected identifier, found {}",
                other.kind_name(),
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Grammar
// ---------------------------------------------------------------------------

impl Parser {
    fn parse_includes(&mut self) -> Result<Vec<String>> {
        let mut includes = Vec::new();
        while *self.peek() == TokenKind::Include {
            self.advance(); // consume 'include'
            let path = match self.peek() {
                TokenKind::Str(s) => {
                    let s = s.clone();
                    self.advance();
                    s
                }
                other => {
                    return Err(self.err(format!(
                        "expected string literal after `include`, found {}",
                        other.kind_name(),
                    )));
                }
            };
            self.eat(TokenKind::Semicolon)?;
            includes.push(path);
        }
        Ok(includes)
    }

    fn parse_interface(&mut self) -> Result<Interface> {
        self.eat(TokenKind::Interface)?;
        let name = self.expect_ident()?;
        self.eat(TokenKind::LBrace)?;

        let mut types = Vec::new();
        let mut methods = Vec::new();

        while !self.check_eat(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.err("unexpected end of file, missing `}`"));
            }
            match self.peek() {
                TokenKind::Struct => types.push(self.parse_struct()?),
                TokenKind::Enum => types.push(self.parse_enum()?),
                TokenKind::Opaque => types.push(self.parse_opaque()?),
                TokenKind::Fn => methods.push(self.parse_method()?),
                other => {
                    return Err(self.err(format!(
                        "expected type declaration or `fn`, found {}",
                        other.kind_name(),
                    )));
                }
            }
        }

        if methods.is_empty() {
            return Err(self.err(format!(
                "interface `{name}` has no methods",
            )));
        }

        Ok(Interface { name, includes: Vec::new(), types, methods })
    }

    // --- Type declarations ---

    fn parse_struct(&mut self) -> Result<TypeDecl> {
        self.eat(TokenKind::Struct)?;
        let name = self.expect_ident()?;
        self.eat(TokenKind::LBrace)?;

        let mut fields = Vec::new();
        loop {
            if self.check_eat(&TokenKind::RBrace) {
                break;
            }
            let field_name = self.expect_ident()?;
            self.eat(TokenKind::Colon)?;
            let field_ty = self.parse_type()?;
            fields.push((field_name, field_ty));
            if !self.check_eat(&TokenKind::Comma) {
                self.eat(TokenKind::RBrace)?;
                break;
            }
        }

        Ok(TypeDecl::Struct(StructDecl { name, fields }))
    }

    fn parse_enum(&mut self) -> Result<TypeDecl> {
        self.eat(TokenKind::Enum)?;
        let name = self.expect_ident()?;
        self.eat(TokenKind::LBrace)?;

        let mut variants = Vec::new();
        loop {
            if self.check_eat(&TokenKind::RBrace) {
                break;
            }
            let var_name = self.expect_ident()?;
            let mut fields = Vec::new();
            if self.check_eat(&TokenKind::LParen) {
                loop {
                    if self.check_eat(&TokenKind::RParen) {
                        break;
                    }
                    let ty = self.parse_type()?;
                    fields.push(ty);
                    if !self.check_eat(&TokenKind::Comma) {
                        self.eat(TokenKind::RParen)?;
                        break;
                    }
                }
            }
            variants.push(EnumVariant { name: var_name, fields });
            if !self.check_eat(&TokenKind::Comma) {
                self.eat(TokenKind::RBrace)?;
                break;
            }
        }

        Ok(TypeDecl::Enum(EnumDecl { name, variants }))
    }

    fn parse_opaque(&mut self) -> Result<TypeDecl> {
        self.eat(TokenKind::Opaque)?;
        self.eat(TokenKind::Struct)?;
        let name = self.expect_ident()?;
        self.eat(TokenKind::Semicolon)?;
        Ok(TypeDecl::Opaque(OpaqueDecl { name }))
    }

    // --- Methods ---

    fn parse_method(&mut self) -> Result<Method> {
        self.eat(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.eat(TokenKind::LParen)?;

        let mut params = Vec::new();
        loop {
            if self.check_eat(&TokenKind::RParen) {
                break;
            }
            let param_name = self.expect_ident()?;
            self.eat(TokenKind::Colon)?;
            let param_ty = self.parse_type()?;
            params.push(Param { name: param_name, ty: param_ty });
            if !self.check_eat(&TokenKind::Comma) {
                self.eat(TokenKind::RParen)?;
                break;
            }
        }

        self.eat(TokenKind::Arrow)?;
        let return_type = self.parse_type()?;
        self.eat(TokenKind::Semicolon)?;

        Ok(Method { name, params, return_type })
    }

    // --- Types ---

    fn parse_borrow_type(&mut self) -> Result<FieldType> {
        // We already consumed '&'
        match self.peek() {
            TokenKind::LBracket => {
                // &[u8]
                self.advance(); // [
                self.expect_primitive("u8")?;
                self.eat(TokenKind::RBracket)?;
                Ok(FieldType::U8Slice)
            }
            TokenKind::Ident(s) if s == "str" => {
                self.advance();
                Ok(FieldType::Str)
            }
            TokenKind::Mut => {
                // &mut Vec<u8>
                self.advance(); // mut
                let name = self.expect_ident()?;
                if name != "Vec" {
                    return Err(self.err(
                        "expected `Vec<u8>` after `&mut`",
                    ));
                }
                self.eat(TokenKind::Lt)?;
                self.expect_primitive("u8")?;
                self.eat(TokenKind::Gt)?;
                Ok(FieldType::OutU8Vec)
            }
            other => Err(self.err(format!(
                "expected `[`, `str`, or `mut` after `&`, found {}",
                other.kind_name(),
            ))),
        }
    }

    fn parse_paren_type(&mut self) -> Result<FieldType> {
        // We're at '('
        let paren_tok = self.expect(TokenKind::LParen)?;

        // Empty tuple = unit
        if self.check_eat(&TokenKind::RParen) {
            return Ok(FieldType::Unit);
        }

        let first = self.parse_type()?;

        // Single element in parens with no comma: treat as that type (like Rust)
        if self.check_eat(&TokenKind::RParen) {
            return Ok(first);
        }

        // Must be a tuple: consume comma, parse remaining
        let mut elems = vec![first];
        loop {
            self.eat(TokenKind::Comma)?;
            // Allow trailing comma: (A, B,)
            if self.check_eat(&TokenKind::RParen) {
                break;
            }
            elems.push(self.parse_type()?);
            if self.check_eat(&TokenKind::RParen) {
                break;
            }
        }

        if elems.len() < 2 {
            return Err(self.err_at(&paren_tok, "tuples must have at least 2 elements"));
        }
        if elems.len() > 8 {
            return Err(self.err_at(&paren_tok, "tuples support at most 8 elements (slot limit)"));
        }
        Ok(FieldType::Tuple(elems))
    }

    /// Resolve an identifier into a FieldType.
    /// Handles primitives, Vec<T>, Option<T>, String, and named types.
    fn type_from_ident(&self, name: &str) -> FieldType {
        match name {
            "bool" => FieldType::Bool,
            "u8" => FieldType::U8,
            "u16" => FieldType::U16,
            "u32" => FieldType::U32,
            "u64" => FieldType::U64,
            "i8" => FieldType::I8,
            "i16" => FieldType::I16,
            "i32" => FieldType::I32,
            "i64" => FieldType::I64,
            "f32" => FieldType::F32,
            "f64" => FieldType::F64,
            "String" => FieldType::String,
            "Vec" | "Option" => {
                // These need generic args, handled separately by caller.
                // If we reach here without parsing <...>, treat as Named.
                // This is a fallback — the proper path is parse_generic_type.
                FieldType::Named(name.to_string())
            }
            other => FieldType::Named(other.to_string()),
        }
    }

    /// Check that the current token is the named primitive and consume it.
    fn expect_primitive(&mut self, expected: &str) -> Result<()> {
        match self.peek() {
            TokenKind::Ident(s) if s == expected => {
                self.advance();
                Ok(())
            }
            other => Err(self.err(format!(
                "expected `{expected}`, found {}",
                other.kind_name(),
            ))),
        }
    }
}

// The generic type parsing (Vec<T>, Option<T>) needs to happen before
// type_from_ident is called for those names. Let me restructure: parse_type
// should check for Vec/Option/ident and handle generics inline.

// ---------------------------------------------------------------------------
// parse_type: the unified type parser (handles generics)
// ---------------------------------------------------------------------------

impl Parser {
    fn parse_type(&mut self) -> Result<FieldType> {
        // Borrow forms: &[...], &str, &mut Vec<u8>
        if *self.peek() == TokenKind::Amp {
            self.advance();
            return self.parse_borrow_type();
        }

        // Tuple / unit: ( ... )
        if *self.peek() == TokenKind::LParen {
            return self.parse_paren_type();
        }

        let name = self.expect_ident()?;

        match name.as_str() {
            "Vec" => {
                self.eat(TokenKind::Lt)?;
                let inner = self.parse_type()?;
                self.eat(TokenKind::Gt)?;
                Ok(FieldType::Vec(Box::new(inner)))
            }
            "Option" => {
                self.eat(TokenKind::Lt)?;
                let inner = self.parse_type()?;
                self.eat(TokenKind::Gt)?;
                Ok(FieldType::Option(Box::new(inner)))
            }
            other => Ok(self.type_from_ident(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a `.dspi` source string into an [`Interface`] AST.
///
/// Does **not** validate named type references — call [`validate`] after
/// include resolution so that included types are visible.
pub fn parse(src: &str) -> Result<Interface> {
    let tokens = Lexer::new(src)
        .tokenize()
        .map_err(|e| ParseError {
            line: e.line,
            col: e.col,
            msg: e.msg,
        })?;

    let mut parser = Parser::new(tokens);
    let includes = parser.parse_includes()?;
    let mut interface = parser.parse_interface()?;
    interface.includes = includes;

    if !parser.at_eof() {
        return Err(parser.err("trailing tokens after interface definition"));
    }

    Ok(interface)
}

/// Parse a type fragment file (no `interface` wrapper) into types and includes.
///
/// A fragment contains only `struct`, `enum`, and `opaque struct` declarations
/// plus optional `include` directives. Method declarations are rejected.
pub fn parse_type_fragment(src: &str) -> Result<(Vec<TypeDecl>, Vec<String>)> {
    let tokens = Lexer::new(src)
        .tokenize()
        .map_err(|e| ParseError {
            line: e.line,
            col: e.col,
            msg: e.msg,
        })?;

    let mut parser = Parser::new(tokens);
    let includes = parser.parse_includes()?;

    let mut types = Vec::new();
    while !parser.at_eof() {
        match parser.peek() {
            TokenKind::Struct => types.push(parser.parse_struct()?),
            TokenKind::Enum => types.push(parser.parse_enum()?),
            TokenKind::Opaque => types.push(parser.parse_opaque()?),
            TokenKind::Fn => {
                return Err(parser.err(
                    "method declarations (`fn`) are not allowed in type fragments",
                ));
            }
            other => {
                return Err(parser.err(format!(
                    "expected type declaration in fragment, found {}",
                    other.kind_name(),
                )));
            }
        }
    }

    Ok((types, includes))
}

pub fn validate(iface: &Interface) -> Result<()> {
    let declared: std::collections::HashSet<&str> = iface.types.iter()
        .map(|t| match t {
            TypeDecl::Struct(s) => s.name.as_str(),
            TypeDecl::Enum(e) => e.name.as_str(),
            TypeDecl::Opaque(o) => o.name.as_str(),
        })
        .collect();

    for m in &iface.methods {
        for p in &m.params {
            check_named_types(&p.ty, &declared)?;
        }
        check_named_types(&m.return_type, &declared)?;
    }

    for ty in &iface.types {
        match ty {
            TypeDecl::Struct(s) => {
                for (_, ft) in &s.fields {
                    check_named_types(ft, &declared)?;
                }
            }
            TypeDecl::Enum(e) => {
                for v in &e.variants {
                    for ft in &v.fields {
                        check_named_types(ft, &declared)?;
                    }
                }
            }
            TypeDecl::Opaque(_) => {}
        }
    }

    Ok(())
}

fn check_named_types(ty: &FieldType, declared: &std::collections::HashSet<&str>) -> Result<()> {
    match ty {
        FieldType::Named(name) => {
            if !declared.contains(name.as_str()) {
                return Err(ParseError {
                    line: 0,
                    col: 0,
                    msg: format!("undeclared type reference: {}", name),
                });
            }
            Ok(())
        }
        FieldType::Vec(inner) | FieldType::Option(inner) => check_named_types(inner, declared),
        FieldType::Tuple(elems) => {
            for e in elems {
                check_named_types(e, declared)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const RLE_DSPI: &str = r#"
interface Rle {
  struct CompressionReport {
    original_size: u64,
    compressed_size: u64,
    ratio: f64,
    runs: u64,
  }

  enum Tone {
    Quiet,
    Normal,
    Loud(u8),
  }

  fn compress(data: &[u8]) -> Vec<u8>;
  fn decompress(data: &[u8]) -> Vec<u8>;
  fn compress_into(data: &[u8], out: &mut Vec<u8>) -> ();
  fn stats(data: &[u8]) -> (u64, u64);
  fn analyze(data: &[u8]) -> CompressionReport;
  fn report_summary(report: CompressionReport) -> String;
  fn run_labels(data: &[u8]) -> Vec<String>;
  fn split_runs(data: &[u8]) -> Vec<Vec<u8>>;
  fn compress_into_checked(data: &[u8], out: &mut Vec<u8>) -> bool;
  fn first_byte(data: &[u8]) -> Option<u8>;
  fn classify(data: &[u8]) -> Tone;
  fn describe_tone(tone: Tone) -> String;
  fn delay(ms: u64) -> ();
}
"#;

    #[test]
    fn test_parse_full_interface() {
        let iface = parse(RLE_DSPI).unwrap();
        assert_eq!(iface.name, "Rle");
        assert_eq!(iface.types.len(), 2);
        assert_eq!(iface.methods.len(), 13);
    }

    #[test]
    fn test_struct_decl() {
        let iface = parse(RLE_DSPI).unwrap();
        match &iface.types[0] {
            TypeDecl::Struct(s) => {
                assert_eq!(s.name, "CompressionReport");
                assert_eq!(s.fields.len(), 4);
                assert_eq!(s.fields[0].0, "original_size");
                assert_eq!(s.fields[0].1, FieldType::U64);
                assert_eq!(s.fields[2].1, FieldType::F64);
            }
            other => panic!("expected Struct, got {other:?}"),
        }
    }

    #[test]
    fn test_enum_decl() {
        let iface = parse(RLE_DSPI).unwrap();
        match &iface.types[1] {
            TypeDecl::Enum(e) => {
                assert_eq!(e.name, "Tone");
                assert_eq!(e.variants.len(), 3);
                assert!(e.variants[0].fields.is_empty());
                assert_eq!(e.variants[2].name, "Loud");
                assert_eq!(e.variants[2].fields, vec![FieldType::U8]);
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn test_method_types() {
        let iface = parse(RLE_DSPI).unwrap();

        // compress(data: &[u8]) -> Vec<u8>
        let compress = &iface.methods[0];
        assert_eq!(compress.name, "compress");
        assert_eq!(compress.params[0].name, "data");
        assert_eq!(compress.params[0].ty, FieldType::U8Slice);
        assert_eq!(compress.return_type, FieldType::Vec(Box::new(FieldType::U8)));

        // compress_into(data: &[u8], out: &mut Vec<u8>) -> ()
        let compress_into = &iface.methods[2];
        assert_eq!(compress_into.params[1].name, "out");
        assert_eq!(compress_into.params[1].ty, FieldType::OutU8Vec);
        assert_eq!(compress_into.return_type, FieldType::Unit);

        // stats -> (u64, u64)
        let stats = &iface.methods[3];
        assert_eq!(
            stats.return_type,
            FieldType::Tuple(vec![FieldType::U64, FieldType::U64]),
        );

        // first_byte -> Option<u8>
        let first_byte = &iface.methods[9];
        assert_eq!(
            first_byte.return_type,
            FieldType::Option(Box::new(FieldType::U8)),
        );

        // classify -> Tone (named)
        let classify = &iface.methods[10];
        assert_eq!(classify.return_type, FieldType::Named("Tone".into()));

        // split_runs -> Vec<Vec<u8>>
        let split = &iface.methods[7];
        assert_eq!(
            split.return_type,
            FieldType::Vec(Box::new(FieldType::Vec(Box::new(FieldType::U8)))),
        );
    }

    #[test]
    fn test_canonical_sig() {
        let iface = parse(RLE_DSPI).unwrap();
        let sig = iface.canonical_sig();
        assert!(sig.starts_with("Rle|"));
        assert!(sig.contains("compress(&[u8])->Vec<u8>"));
        assert!(sig.contains("stats(&[u8])->(u64,u64)"));
    }

    #[test]
    fn test_hash_deterministic() {
        let iface = parse(RLE_DSPI).unwrap();
        let h1 = fnv1a_64(iface.canonical_sig().as_bytes());
        let h2 = fnv1a_64(iface.canonical_sig().as_bytes());
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_opaque_struct() {
        let src = "interface Foo {
            opaque struct Handle;
            fn get(h: Handle) -> ();
        }";
        let iface = parse(src).unwrap();
        assert_eq!(iface.types.len(), 1);
        match &iface.types[0] {
            TypeDecl::Opaque(o) => assert_eq!(o.name, "Handle"),
            other => panic!("expected Opaque, got {other:?}"),
        }
    }

    #[test]
    fn test_empty_interface_errors() {
        let err = parse("interface Empty {}").unwrap_err();
        assert!(err.msg.contains("no methods"));
    }

    #[test]
    fn test_missing_brace_errors() {
        let err = parse("interface Foo {\nfn a() -> ()").unwrap_err();
        assert!(err.msg.contains("end of file") || err.msg.contains("`}`"));
    }

    #[test]
    fn test_trailing_tokens_errors() {
        let err = parse("interface Foo { fn a() -> (); } extra").unwrap_err();
        assert!(err.msg.contains("trailing tokens"));
    }

    #[test]
    fn test_unknown_type_in_borrow() {
        let err = parse("interface Foo { fn a(x: &[u32]) -> (); }").unwrap_err();
        assert!(err.msg.contains("`u8`"));
    }

    #[test]
    fn test_invalid_outvec() {
        let err = parse("interface Foo { fn a(x: &mut Vec<u32>) -> (); }").unwrap_err();
        assert!(err.msg.contains("`Vec<u8>`") || err.msg.contains("`u8`"));
    }

    #[test]
    fn test_trailing_comma_in_struct() {
        let src = "interface Foo {
            struct Bar { x: u64, y: u64, }
            fn a(b: Bar) -> ();
        }";
        let iface = parse(src).unwrap();
        match &iface.types[0] {
            TypeDecl::Struct(s) => assert_eq!(s.fields.len(), 2),
            other => panic!("expected Struct, got {other:?}"),
        }
    }

    #[test]
    fn test_trailing_comma_in_enum() {
        let src = "interface Foo {
            enum Bar { A, B, }
            fn a(b: Bar) -> ();
        }";
        let iface = parse(src).unwrap();
        match &iface.types[0] {
            TypeDecl::Enum(e) => assert_eq!(e.variants.len(), 2),
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn test_nested_generics() {
        let src = "interface Foo {
            fn a(x: Vec<Option<Vec<u8>>>) -> Vec<Vec<Vec<u8>>>;
        }";
        let iface = parse(src).unwrap();
        let m = &iface.methods[0];
        assert_eq!(
            m.params[0].ty,
            FieldType::Vec(Box::new(FieldType::Option(Box::new(
                FieldType::Vec(Box::new(FieldType::U8))
            )))),
        );
        assert_eq!(
            m.return_type,
            FieldType::Vec(Box::new(FieldType::Vec(Box::new(
                FieldType::Vec(Box::new(FieldType::U8))
            )))),
        );
    }

    #[test]
    fn test_trailing_comma_in_tuple() {
        let src = "interface Foo { fn a() -> (u64, u64,); }";
        let iface = parse(src).unwrap();
        assert_eq!(
            iface.methods[0].return_type,
            FieldType::Tuple(vec![FieldType::U64, FieldType::U64]),
        );
    }

    #[test]
    fn test_undeclared_type_in_return() {
        let iface = parse("interface Foo { fn a() -> MissingType; }").unwrap();
        let err = validate(&iface).unwrap_err();
        assert!(err.msg.contains("undeclared type reference: MissingType"));
    }

    #[test]
    fn test_undeclared_type_in_param() {
        let iface = parse("interface Foo { fn a(x: MissingType) -> (); }").unwrap();
        let err = validate(&iface).unwrap_err();
        assert!(err.msg.contains("undeclared type reference: MissingType"));
    }

    #[test]
    fn test_undeclared_type_in_vec() {
        let iface = parse("interface Foo { fn a() -> Vec<MissingType>; }").unwrap();
        let err = validate(&iface).unwrap_err();
        assert!(err.msg.contains("undeclared type reference: MissingType"));
    }

    #[test]
    fn test_undeclared_type_in_enum_field() {
        let src = "interface Foo {
            enum Bar { Variant(MissingType) }
            fn a() -> Bar;
        }";
        let iface = parse(src).unwrap();
        let err = validate(&iface).unwrap_err();
        assert!(err.msg.contains("undeclared type reference: MissingType"));
    }

    #[test]
    fn test_declared_type_passes() {
        let src = "interface Foo {
            enum Bar { A, B(u64) }
            fn a() -> Bar;
        }";
        let iface = parse(src).unwrap();
        assert!(validate(&iface).is_ok());
    }

    #[test]
    fn test_parse_includes() {
        let src = r#"
            include "types.dspi";
            include "more.dspi";

            interface Foo {
                fn a() -> ();
            }
        "#;
        let iface = parse(src).unwrap();
        assert_eq!(iface.includes, vec!["types.dspi", "more.dspi"]);
    }

    #[test]
    fn test_no_includes() {
        let src = "interface Foo { fn a() -> (); }";
        let iface = parse(src).unwrap();
        assert!(iface.includes.is_empty());
    }

    #[test]
    fn test_include_missing_string() {
        let err = parse("include not_a_string;\ninterface Foo { fn a() -> (); }").unwrap_err();
        assert!(err.msg.contains("expected string literal after `include`"));
    }

    #[test]
    fn test_type_fragment_basic() {
        let src = r#"
            opaque struct Handle;
            struct Config { path: String, }
            enum Status { Ok, Err(u32), }
        "#;
        let (types, includes) = parse_type_fragment(src).unwrap();
        assert_eq!(types.len(), 3);
        assert!(includes.is_empty());
    }

    #[test]
    fn test_type_fragment_with_includes() {
        let src = r#"
            include "nested.dspi";

            opaque struct Handle;
        "#;
        let (types, includes) = parse_type_fragment(src).unwrap();
        assert_eq!(types.len(), 1);
        assert_eq!(includes, vec!["nested.dspi"]);
    }

    #[test]
    fn test_type_fragment_rejects_methods() {
        let src = "opaque struct Handle;\nfn do_stuff() -> ();";
        let err = parse_type_fragment(src).unwrap_err();
        assert!(err.msg.contains("not allowed in type fragments"));
    }

    #[test]
    fn test_type_fragment_rejects_interface() {
        let src = "interface Foo { fn a() -> (); }";
        let err = parse_type_fragment(src).unwrap_err();
        assert!(err.msg.contains("expected type declaration in fragment"));
    }

    #[test]
    fn test_type_fragment_empty() {
        let (types, includes) = parse_type_fragment("").unwrap();
        assert!(types.is_empty());
        assert!(includes.is_empty());
    }
}
