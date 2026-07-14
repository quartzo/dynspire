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
///
/// The IDL is **language-agnostic**: only DynSpire managed types (`D*`),
/// primitives, tuples, arrays, and named types cross the boundary. Borrow
/// semantics (`&T` for shared views, `&mut T` for out-params) are
/// orthogonal to the underlying type via [`FieldType::Ref`] /
/// [`FieldType::RefMut`].
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
    /// Owned managed string (`dynspire::managed::DString`). RC-aware.
    DString,
    /// Owned managed vec (`dynspire::managed::DVec<T>`). RC-aware.
    DVec(Box<FieldType>),
    /// Managed option (`dynspire::managed::DOption<T>`).
    DOption(Box<FieldType>),
    /// `&T` — shared borrow view. Wire format: 2 slots `(ptr, len)` when
    /// `T` is `DString` / `DVec<_>` (zero-copy view of the underlying
    /// buffer); for primitives/named types the codegen emits the same
    /// wire as a pass-by-value (the borrow is a Rust-side hint).
    Ref(Box<FieldType>),
    /// `&mut T` — mutable borrow (out-param). The caller owns `T` and
    /// the callee fills it via realloc/copy-back through the host
    /// allocator. Generalizes the previous `&mut Vec<u8>` to any
    /// `&mut DVec<T>` (and potentially `&mut DString`).
    RefMut(Box<FieldType>),
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
            FieldType::DString => "DString".into(),
            FieldType::DVec(t) => format!("DVec<{}>", t.canonical()),
            FieldType::DOption(t) => format!("DOption<{}>", t.canonical()),
            FieldType::Ref(t) => format!("&{}", t.canonical()),
            FieldType::RefMut(t) => format!("&mut {}", t.canonical()),
            FieldType::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| t.canonical()).collect();
                format!("({})", parts.join(","))
            }
            FieldType::Array(inner, len) => format!("[{};{}]", inner.canonical(), len),
            FieldType::Named(n) => n.clone(),
        }
    }

    /// Rust source type for a **by-value** appearance of this type (struct
    /// fields, and DType inputs, which are RC-aware but passed by value).
    pub fn rust_value_type(&self) -> String {
        match self {
            FieldType::DString => "dynspire::managed::DString".into(),
            FieldType::DVec(t) => format!("dynspire::managed::DVec<{}>", t.rust_type()),
            FieldType::DOption(t) => format!("dynspire::managed::DOption<{}>", t.rust_type()),
            other => other.rust_type(),
        }
    }

    /// Rust source type for a method **input parameter**. Owned DTypes are
    /// passed by value (the receiver takes implicit ownership of the RC
    /// slot); borrowed types (`Ref`/`RefMut`) use the corresponding view
    /// types — `&DVec<T>` in the IDL becomes `DSlice<T>` in Rust (a
    /// 2-field ptr+len view), `&DString` becomes `DStr`, and `&mut DVec<T>`
    /// becomes `&mut DVec<T>` (mutable borrow for out-params).
    pub fn rust_input_type(&self) -> String {
        match self {
            // `&DVec<T>` IDL → DSlice<T> in Rust (view type, no allocator).
            FieldType::Ref(inner) => match inner.as_ref() {
                FieldType::DString => "dynspire::managed::DStr".into(),
                FieldType::DVec(elem) => {
                    format!("dynspire::managed::DSlice<{}>", elem.rust_type())
                }
                other => other.rust_value_type(),
            },
            FieldType::RefMut(inner) => format!("&mut {}", inner.rust_value_type()),
            other => other.rust_value_type(),
        }
    }

    /// Rust source type for a method **return value**. Owned DTypes
    /// (`DVec`/`DString`) are returned by value — the type's own `Drop`
    /// releases the buffer when the receiver lets it go out of scope.
    pub fn rust_output_type(&self) -> String {
        self.rust_value_type()
    }

    /// Whether this return type is wrapped in an owning guard on the spier
    /// side, requiring `into_raw` before writing slots. After the
    /// `OwnedDVec`/`OwnedDString` elimination, `DVec`/`DString` themselves
    /// are RC-aware — `into_raw` lives on the type directly. This helper
    /// flags returns that need the `into_raw` step in the codegen.
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
            FieldType::DString => "dynspire::managed::DString".into(),
            FieldType::DVec(t) => format!("dynspire::managed::DVec<{}>", t.rust_type()),
            FieldType::DOption(t) => format!("dynspire::managed::DOption<{}>", t.rust_type()),
            // `&T` (IDL borrow) maps to the view type in Rust: DSlice<T> for
            // `&DVec<T>`, DStr for `&DString`. For primitives/named types it
            // recurses on the inner (the borrow is just a Rust hint).
            FieldType::Ref(t) => match t.as_ref() {
                FieldType::DString => "dynspire::managed::DStr".into(),
                FieldType::DVec(elem) => {
                    format!("dynspire::managed::DSlice<{}>", elem.rust_type())
                }
                other => other.rust_type(),
            },
            FieldType::RefMut(t) => format!("&mut {}", t.rust_type()),
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
        matches!(self, FieldType::Ref(_) | FieldType::RefMut(_))
    }

    /// Rust source type for a **struct/enum field** in `repr(C)` layout.
    /// DTypes map to their C-stable managed equivalents; primitives and
    /// named types pass through unchanged. Borrow types are not allowed
    /// as struct fields (validated elsewhere).
    pub fn rust_field_type(&self) -> String {
        match self {
            FieldType::DString => "dynspire::managed::DString".into(),
            FieldType::DVec(inner) => format!("dynspire::managed::DVec<{}>", inner.rust_field_type()),
            FieldType::DOption(inner) => {
                format!("dynspire::managed::DOption<{}>", inner.rust_field_type())
            }
            other => other.rust_type(),
        }
    }

    /// Whether this type (as a struct/enum field) contains heap-allocated
    /// buffers that require a `drop_fn` to release.  True for `DString`,
    /// `DVec<T>`, and for any struct/enum containing them.
    pub fn has_dynamic_fields(&self) -> bool {
        matches!(self, FieldType::DString | FieldType::DVec(_))
    }

    /// Convert an IDL type to its C-stable DType equivalent at the AST level.
    ///
    /// After the native-type elimination, the IDL is already expressed in
    /// DTypes — this function is now an identity for the wire-format
    /// types (DString/DVec/DOption). Used by enum variant codecs to ensure
    /// reader/writer functions use the same DType wire format as
    /// `rust_field_type` produces in the enum definition.
    pub fn to_field_dtype(&self) -> FieldType {
        match self {
            FieldType::DVec(inner) => FieldType::DVec(Box::new(inner.to_field_dtype())),
            FieldType::DOption(inner) => FieldType::DOption(Box::new(inner.to_field_dtype())),
            other => other.clone(),
        }
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
