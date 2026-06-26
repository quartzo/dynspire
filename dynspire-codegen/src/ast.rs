//! AST for `.dspi` interface definitions.
//!
//! Parsed by [`crate::parser::parse`], consumed by the code generators.

use std::fmt;

// ---------------------------------------------------------------------------
// Interface
// ---------------------------------------------------------------------------

/// A complete interface definition parsed from a `.dspi` file.
///
/// `interface Rle { ... }` → `Interface { name: "Rle", ... }`
#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    pub name: String,
    pub includes: Vec<String>,
    pub types: Vec<TypeDecl>,
    pub methods: Vec<Method>,
}

// ---------------------------------------------------------------------------
// Type declarations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum TypeDecl {
    Struct(StructDecl),
    Enum(EnumDecl),
    Opaque(OpaqueDecl),
}

/// `struct CompressionReport { original_size: u64, ... }`
#[derive(Debug, Clone, PartialEq)]
pub struct StructDecl {
    pub name: String,
    pub fields: Vec<(String, FieldType)>,
}

/// `enum Tone { Quiet, Normal, Loud(u8) }`
#[derive(Debug, Clone, PartialEq)]
pub struct EnumDecl {
    pub name: String,
    pub variants: Vec<EnumVariant>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Vec<FieldType>,
}

/// `opaque struct ExternalHandle;`
#[derive(Debug, Clone, PartialEq)]
pub struct OpaqueDecl {
    pub name: String,
}

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Method {
    pub name: String,
    pub params: Vec<Param>,
    /// The `Ok` variant type. `Result<_, String>` is implicit.
    pub return_type: FieldType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: FieldType,
}

// ---------------------------------------------------------------------------
// FieldType — closed set of transport categories
// ---------------------------------------------------------------------------

/// Every type that can appear in a `.dspi` method signature or struct/enum
/// field. Each variant maps 1:1 to a slot encoding strategy in the runtime.
///
/// **Not** an open Rust type system — the grammar is closed.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Unit,
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    /// `&str` — zero-copy borrow (ptr, len).
    Str,
    /// `&[u8]` — zero-copy borrow (ptr, len).
    U8Slice,
    /// `&mut Vec<u8>` — out-param (raw ptr to caller's Vec).
    OutU8Vec,
    /// Owned `String`.
    String,
    /// Owned `Vec<T>`.
    Vec(Box<FieldType>),
    /// `Option<T>`.
    Option(Box<FieldType>),
    /// `(A, B, C)` — 2+ elements only.
    Tuple(Vec<FieldType>),
    /// `[u8; N]` — fixed-size byte array.
    Array(Box<FieldType>, usize),
    /// Reference to a declared type (struct, enum, or opaque).
    Named(String),
}

impl FieldType {
    /// Canonical string form used for hash computation and error messages.
    pub fn canonical(&self) -> String {
        match self {
            FieldType::Unit => "()".into(),
            FieldType::Bool => "bool".into(),
            FieldType::U8 => "u8".into(),
            FieldType::U16 => "u16".into(),
            FieldType::U32 => "u32".into(),
            FieldType::U64 => "u64".into(),
            FieldType::I8 => "i8".into(),
            FieldType::I16 => "i16".into(),
            FieldType::I32 => "i32".into(),
            FieldType::I64 => "i64".into(),
            FieldType::F32 => "f32".into(),
            FieldType::F64 => "f64".into(),
            FieldType::Str => "&str".into(),
            FieldType::U8Slice => "&[u8]".into(),
            FieldType::OutU8Vec => "&mut Vec<u8>".into(),
            FieldType::String => "String".into(),
            FieldType::Vec(t) => format!("Vec<{}>", t.canonical()),
            FieldType::Option(t) => format!("Option<{}>", t.canonical()),
            FieldType::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| t.canonical()).collect();
                format!("({})", parts.join(","))
            }
            FieldType::Array(inner, len) => format!("[{};{}]", inner.canonical(), len),
            FieldType::Named(n) => n.clone(),
        }
    }

    /// Rust source type as it should appear in the generated trait signature.
    pub fn rust_type(&self) -> String {
        match self {
            FieldType::Unit => "()".into(),
            FieldType::Bool => "bool".into(),
            FieldType::U8 => "u8".into(),
            FieldType::U16 => "u16".into(),
            FieldType::U32 => "u32".into(),
            FieldType::U64 => "u64".into(),
            FieldType::I8 => "i8".into(),
            FieldType::I16 => "i16".into(),
            FieldType::I32 => "i32".into(),
            FieldType::I64 => "i64".into(),
            FieldType::F32 => "f32".into(),
            FieldType::F64 => "f64".into(),
            FieldType::Str => "&str".into(),
            FieldType::U8Slice => "&[u8]".into(),
            FieldType::OutU8Vec => "&mut Vec<u8>".into(),
            FieldType::String => "String".into(),
            FieldType::Vec(t) => format!("Vec<{}>", t.rust_type()),
            FieldType::Option(t) => format!("Option<{}>", t.rust_type()),
            FieldType::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| t.rust_type()).collect();
                format!("({})", parts.join(", "))
            }
            FieldType::Array(inner, len) => format!("[{}; {}]", inner.rust_type(), len),
            FieldType::Named(n) => n.clone(),
        }
    }

    /// Whether this type is passed by reference (borrow) as an input parameter.
    /// The spier decode path differs for borrows vs owned.
    pub fn is_borrow(&self) -> bool {
        matches!(self, FieldType::Str | FieldType::U8Slice | FieldType::OutU8Vec)
    }
}

// ---------------------------------------------------------------------------
// Canonical signature for hash
// ---------------------------------------------------------------------------

impl Interface {
    /// Canonical signature string for hash computation.
    ///
    /// Format: `Name|type1;type2;...|method(params)->ret,...`
    ///
    /// Deterministic — depends only on the AST, never on source formatting
    /// or Rust syntax representation.
    pub fn canonical_sig(&self) -> String {
        let types: Vec<String> = self.types.iter().map(|t| t.canonical()).collect();
        let methods: Vec<String> = self
            .methods
            .iter()
            .map(|m| {
                let params: Vec<String> = m.params.iter().map(|p| p.ty.canonical()).collect();
                format!("{}({})->{}", m.name, params.join(","), m.return_type.canonical())
            })
            .collect();
        format!("{}|{}|{}", self.name, types.join(";"), methods.join(","))
    }
}

impl TypeDecl {
    pub fn name(&self) -> &str {
        match self {
            TypeDecl::Struct(s) => &s.name,
            TypeDecl::Enum(e) => &e.name,
            TypeDecl::Opaque(o) => &o.name,
        }
    }

    pub fn canonical(&self) -> String {
        match self {
            TypeDecl::Struct(s) => {
                let fields: Vec<String> =
                    s.fields.iter().map(|(n, t)| format!("{}:{}", n, t.canonical())).collect();
                format!("struct {}{{{}}}", s.name, fields.join(","))
            }
            TypeDecl::Enum(e) => {
                let variants: Vec<String> = e
                    .variants
                    .iter()
                    .map(|v| {
                        if v.fields.is_empty() {
                            v.name.clone()
                        } else {
                            let fields: Vec<String> =
                                v.fields.iter().map(|t| t.canonical()).collect();
                            format!("{}({})", v.name, fields.join(","))
                        }
                    })
                    .collect();
                format!("enum {}{{{}}}", e.name, variants.join(","))
            }
            TypeDecl::Opaque(o) => format!("opaque {}", o.name),
        }
    }
}

// ---------------------------------------------------------------------------
// FNV-1a 64-bit — used for IDL hash computation
// ---------------------------------------------------------------------------

pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Display for FieldType (used in error messages)
// ---------------------------------------------------------------------------

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.rust_type())
    }
}
