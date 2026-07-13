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
    /// `DStr` — non-owning view (`dynspire::managed::DStr`). Zero-copy.
    DStr,
    /// `DSlice<T>` — non-owning view (`dynspire::managed::DSlice<T>`). Zero-copy.
    DSlice(Box<FieldType>),
    /// `DString` — owned managed string (`dynspire::managed::DString`).
    DString,
    /// `DVec<T>` — owned managed vec (`dynspire::managed::DVec<T>`).
    DVec(Box<FieldType>),
    /// `DOption<T>` — managed option (`dynspire::managed::DOption<T>`).
    DOption(Box<FieldType>),
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
            FieldType::DStr => "DStr".into(),
            FieldType::DSlice(t) => format!("DSlice<{}>", t.canonical()),
            FieldType::DString => "DString".into(),
            FieldType::DVec(t) => format!("DVec<{}>", t.canonical()),
            FieldType::DOption(t) => format!("DOption<{}>", t.canonical()),
            FieldType::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| t.canonical()).collect();
                format!("({})", parts.join(","))
            }
            FieldType::Array(inner, len) => format!("[{};{}]", inner.canonical(), len),
            FieldType::Named(n) => n.clone(),
        }
    }

    /// Rust source type for a **by-value** appearance of this type (struct
    /// fields, and DType inputs, which are non-owning `Copy` views).
    pub fn rust_value_type(&self) -> String {
        match self {
            FieldType::DStr => "dynspire::managed::DStr".into(),
            FieldType::DSlice(t) => format!("dynspire::managed::DSlice<{}>", t.rust_type()),
            FieldType::DString => "dynspire::managed::DString".into(),
            FieldType::DVec(t) => format!("dynspire::managed::DVec<{}>", t.rust_type()),
            FieldType::DOption(t) => format!("dynspire::managed::DOption<{}>", t.rust_type()),
            other => other.rust_type(),
        }
    }

    /// Rust source type for a method **input parameter**. DTypes are passed
    /// by value as `Copy` views (the spier reads them without releasing).
    pub fn rust_input_type(&self) -> String {
        self.rust_value_type()
    }

    /// Rust source type for a method **return value**. Owned `DVec`/`DString`
    /// are wrapped in an `OwnedDVec`/`OwnedDString` guard so the receiver
    /// releases the buffer exactly once; views/inline DTypes are returned by
    /// value.
    pub fn rust_output_type(&self) -> String {
        match self {
            FieldType::DVec(t) => format!("dynspire::managed::OwnedDVec<{}>", t.rust_type()),
            FieldType::DString => "dynspire::managed::OwnedDString".into(),
            other => other.rust_value_type(),
        }
    }

    /// Whether this return type is wrapped in an owning guard (`OwnedDVec` /
    /// `OwnedDString`) on the spier side, requiring `into_raw` before writing
    /// slots.
    pub fn is_guarded_return(&self) -> bool {
        matches!(self, FieldType::DVec(_) | FieldType::DString)
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
            FieldType::DStr => "dynspire::managed::DStr".into(),
            FieldType::DSlice(t) => format!("dynspire::managed::DSlice<{}>", t.rust_type()),
            FieldType::DString => "dynspire::managed::DString".into(),
            FieldType::DVec(t) => format!("dynspire::managed::DVec<{}>", t.rust_type()),
            FieldType::DOption(t) => format!("dynspire::managed::DOption<{}>", t.rust_type()),
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
        matches!(
            self,
            FieldType::Str | FieldType::U8Slice | FieldType::OutU8Vec | FieldType::DStr | FieldType::DSlice(_)
        )
    }

    /// Rust source type for a **struct/enum field** in `repr(C)` layout.
    /// Translates IDL types to their C-stable DType equivalents:
    /// `String` → `DString`, `Vec<T>` → `DVec<T>`, `Option<T>` → `DOption<T>`,
    /// `&str` → `DStr`, `&[u8]` → `DSlice<u8>`. Primitives and named types
    /// pass through unchanged.
    pub fn rust_field_type(&self) -> String {
        match self {
            FieldType::String => "dynspire::managed::DString".into(),
            FieldType::Vec(inner) => format!("dynspire::managed::DVec<{}>", inner.rust_field_type()),
            FieldType::Option(inner) => format!("dynspire::managed::DOption<{}>", inner.rust_field_type()),
            FieldType::Str => "dynspire::managed::DStr".into(),
            FieldType::U8Slice => "dynspire::managed::DSlice<u8>".into(),
            FieldType::DStr => "dynspire::managed::DStr".into(),
            FieldType::DSlice(inner) => format!("dynspire::managed::DSlice<{}>", inner.rust_field_type()),
            FieldType::DString => "dynspire::managed::DString".into(),
            FieldType::DVec(inner) => format!("dynspire::managed::DVec<{}>", inner.rust_field_type()),
            FieldType::DOption(inner) => format!("dynspire::managed::DOption<{}>", inner.rust_field_type()),
            other => other.rust_type(),
        }
    }

    /// Whether this type (as a struct/enum field) contains heap-allocated
    /// buffers that require a `drop_fn` to release.  True for `DString`,
    /// `DVec<T>`, and for any struct/enum containing them.
    pub fn has_dynamic_fields(&self) -> bool {
        matches!(
            self,
            FieldType::DString | FieldType::DVec(_)
        )
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
