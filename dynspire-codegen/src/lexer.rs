//! Tokenizer for `.dspi` files.
//!
//! Produces a flat token stream with line/column positions for error reporting.

use std::fmt;

// ---------------------------------------------------------------------------
// Token
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Interface,
    Struct,
    Enum,
    Opaque,
    Fn,
    Mut,
    // Punctuation
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Lt,
    Gt,
    Amp,
    Comma,
    Colon,
    Semicolon,
    Arrow,
    // Identifiers
    Ident(String),
    Eof,
}

impl TokenKind {
    pub fn kind_name(&self) -> &'static str {
        match self {
            TokenKind::Interface => "keyword `interface`",
            TokenKind::Struct => "keyword `struct`",
            TokenKind::Enum => "keyword `enum`",
            TokenKind::Opaque => "keyword `opaque`",
            TokenKind::Fn => "keyword `fn`",
            TokenKind::Mut => "keyword `mut`",
            TokenKind::LBrace => "`{`",
            TokenKind::RBrace => "`}`",
            TokenKind::LParen => "`(`",
            TokenKind::RParen => "`)`",
            TokenKind::LBracket => "`[`",
            TokenKind::RBracket => "`]`",
            TokenKind::Lt => "`<`",
            TokenKind::Gt => "`>`",
            TokenKind::Amp => "`&`",
            TokenKind::Comma => "`,`",
            TokenKind::Colon => "`:`",
            TokenKind::Semicolon => "`;`",
            TokenKind::Arrow => "`->`",
            TokenKind::Ident(_) => "identifier",
            TokenKind::Eof => "end of file",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub col: usize,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LexError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for LexError {}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

const KEYWORDS: &[(&str, TokenKind)] = &[
    ("interface", TokenKind::Interface),
    ("struct", TokenKind::Struct),
    ("enum", TokenKind::Enum),
    ("opaque", TokenKind::Opaque),
    ("fn", TokenKind::Fn),
    ("mut", TokenKind::Mut),
];

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            let token = self.next_token()?;
            let is_eof = token.kind == TokenKind::Eof;
            tokens.push(token);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') => {
                    self.advance();
                }
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.advance();
                    }
                }
                _ => break,
            }
        }
    }

    fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_whitespace_and_comments();

        let line = self.line;
        let col = self.col;

        let b = match self.peek() {
            Some(b) => b,
            None => {
                return Ok(Token { kind: TokenKind::Eof, line, col });
            }
        };

        let kind = match b {
            b'{' => { self.advance(); TokenKind::LBrace }
            b'}' => { self.advance(); TokenKind::RBrace }
            b'(' => { self.advance(); TokenKind::LParen }
            b')' => { self.advance(); TokenKind::RParen }
            b'[' => { self.advance(); TokenKind::LBracket }
            b']' => { self.advance(); TokenKind::RBracket }
            b'<' => { self.advance(); TokenKind::Lt }
            b'>' => { self.advance(); TokenKind::Gt }
            b'&' => { self.advance(); TokenKind::Amp }
            b',' => { self.advance(); TokenKind::Comma }
            b':' => { self.advance(); TokenKind::Colon }
            b';' => { self.advance(); TokenKind::Semicolon }
            b'-' if self.peek_at(1) == Some(b'>') => {
                self.advance();
                self.advance();
                TokenKind::Arrow
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.read_ident(),
            _ => {
                return Err(LexError {
                    line,
                    col,
                    msg: format!("unexpected character `{}`", b as char),
                });
            }
        };

        Ok(Token { kind, line, col })
    }

    fn read_ident(&mut self) -> TokenKind {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
            self.advance();
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();

        for (kw, kind) in KEYWORDS {
            if text == *kw {
                return kind.clone();
            }
        }
        TokenKind::Ident(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<TokenKind> {
        Lexer::new(src).tokenize().unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn test_basic_punctuation() {
        assert_eq!(
            lex("{ } ( ) [ ] < > & , : ; ->"),
            vec![
                TokenKind::LBrace, TokenKind::RBrace,
                TokenKind::LParen, TokenKind::RParen,
                TokenKind::LBracket, TokenKind::RBracket,
                TokenKind::Lt, TokenKind::Gt,
                TokenKind::Amp,
                TokenKind::Comma, TokenKind::Colon, TokenKind::Semicolon,
                TokenKind::Arrow,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_keywords() {
        assert_eq!(
            lex("interface struct enum opaque fn mut"),
            vec![
                TokenKind::Interface, TokenKind::Struct, TokenKind::Enum,
                TokenKind::Opaque, TokenKind::Fn, TokenKind::Mut,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_identifiers() {
        assert_eq!(
            lex("Rle compress u8 Vec Option String"),
            vec![
                TokenKind::Ident("Rle".into()),
                TokenKind::Ident("compress".into()),
                TokenKind::Ident("u8".into()),
                TokenKind::Ident("Vec".into()),
                TokenKind::Ident("Option".into()),
                TokenKind::Ident("String".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_comments() {
        assert_eq!(
            lex("// comment\nfn"),
            vec![TokenKind::Fn, TokenKind::Eof],
        );
    }

    #[test]
    fn test_arrow_vs_minus() {
        assert_eq!(
            lex("->"),
            vec![TokenKind::Arrow, TokenKind::Eof],
        );
    }

    #[test]
    fn test_unexpected_char() {
        let err = Lexer::new("@").tokenize().unwrap_err();
        assert!(err.msg.contains("unexpected character"));
        assert_eq!(err.line, 1);
        assert_eq!(err.col, 1);
    }

    #[test]
    fn test_line_col_tracking() {
        let tokens = Lexer::new("interface\n  fn").tokenize().unwrap();
        assert_eq!(tokens[0].line, 1);
        assert_eq!(tokens[0].col, 1);
        assert_eq!(tokens[1].line, 2);
        assert_eq!(tokens[1].col, 3);
    }
}
