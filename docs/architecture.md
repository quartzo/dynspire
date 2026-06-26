# DynSpire Architecture

This document covers the internals of the DynSpire plugin framework: the slot system, FFI conventions, IDL schema export, DSL codegen, and the Python PyO3 adapter.

## Table of Contents

- [Overview](#overview)
- [Slot System](#slot-system)
- [IDL Schema Export](#idl-schema-export)
- [DSL Codegen](#dsl-codegen)
- [Tower Client](#tower-client)
- [Python PyO3 Adapter](#python-pyo3-adapter)
- [Path Resolution](#path-resolution)

---

## Overview

DynSpire is a plugin architecture with three roles:

> **Read this first — DynSpire is an in-process plugin ABI, not an RPC framework.** A spier is a `cdylib` loaded into the host via `dlopen`; host and spier run in **the same process and the same address space**. Crossing the boundary means a **C ABI call with a flat `u64[]` slot convention** — there is no network, no IPC, no wire format. Consequences that follow from this and *do not* hold under an RPC mental model:
>
> - **Opaque pointers are valid across the boundary.** DSL-declared structs pass `Box::into_raw` → 1 slot (the raw address); the receiver does `Box::from_raw` or dereferences directly. The struct is **not serialized** — it stays live, with whatever state machine, lock, or inner reference it holds. Both sides alias the same memory.
> - **"IDL" / "schema" = in-process ABI contract + runtime type table**, exposed via `dynspire_idl_schema()`. Not a serialization descriptor, not a protobuf schema. The IDL hash gates **binary/link compatibility** between two `.so`s compiled against the same trait — not message-level versioning.
> - **The boundary is compile/link, not process.** Borrows (`&[u8]`, `&str`), out-params (`&mut Vec<u8>`), and ownership-transfer (`Vec<T>` via `Box::into_raw`) are all sound precisely because caller and callee share one heap. None of this is possible across a process boundary; all of it is routine here.

| Role | Who | What |
|------|-----|------|
| **IDL** | `.dspi` file | A `.dspi` file is the single source of truth. `build.rs` invokes `dynspire_codegen::build()` to generate the trait, types, Op enum, IDL hash, type table, schema, tower client wrapper, and spier dispatch macro. |
| **Spier** | `cdylib` crate | Compiles the `.dspi` independently. Implements the generated trait. Invokes `impl_{name}_spier!($state, init, "name")` — a generated `macro_rules!` that produces all C-ABI entry points (`dynspire_create`, `dynspire_dispatch_{method}`, `dynspire_destroy`, etc.). No proc macros. |
| **Host** | Binary or script | Compiles the same `.dspi` independently (or depends on a shared IDL crate — see below). Uses the generated `DynSpire{Name}` client wrapper directly. One import, no boilerplate. Python hosts skip codegen entirely and read the schema at runtime. |

> **The IDL hash is the contract, not the crate dependency.** The spier and host
> can each compile the `.dspi` independently — both sides produce the same hash
> from the same interface signature, so `connect()` accepts the spier. A shared
> IDL crate is a convenience (prevents version skew by construction), not a
> requirement. The FFI boundary uses raw pointers and slots, never Rust type
> identity, so independently-compiled types from the same `.dspi` are
> layout-compatible.

```
┌─────────────────────────────────────────────┐
│  Host (Rust binary or Python script)        │
│                                             │
│  DynSpireClient / SpierHandle               │
│    .dispatch(Op, in_slots, out_slots)        │
│    → SlotWriter encodes args as u64[]       │
│    → dispatch via FFI                       │
│    → SlotReader decodes response u64[]      │
└──────────────────┬──────────────────────────┘
                   │ libloading / PyO3
                   ▼
┌─────────────────────────────────────────────┐
│  Spier .so (cdylib)                         │
│                                             │
│  dynspire_create(config) → *mut State       │
│  dynspire_dispatch_method(state, in, out)   │
│    → SlotReader decodes args from u64[]     │
│    → calls trait method                     │
│    → SlotWriter encodes Result<T> to u64[]  │
│  dynspire_destroy(state)                    │
│                                             │
│  dynspire_idl_hash() → u64                  │
│  dynspire_idl_schema() → &DynSpireIdl       │
└─────────────────────────────────────────────┘
```

---

## Slot System

The slot system is the core FFI calling convention. Every argument and return value is encoded as a flat array of `u64` values ("slots"). This avoids heap allocation on the FFI boundary and works with any C-compatible caller.

The codegen inlines all slot encoding and decoding directly into each method's tower wrapper (host side) and dispatch function (spier side), using only `SlotWriter::write_u64()` and `SlotReader::read_u64()`. There are no trait dispatch points — the generated code calls the slot primitives directly, which eliminates the duplicate-trait-impl problem when multiple interfaces share type fragments.

### Core primitives

```rust
pub struct SlotWriter { /* accumulates u64 values */ }
impl SlotWriter {
    pub fn write_u64(&mut self, val: u64);
    pub fn as_slice(&self) -> &[u64];
    pub fn len(&self) -> usize;
}

pub struct SlotReader<'a> { /* reads u64 values from a slice */ }
impl<'a> SlotReader<'a> {
    pub fn read_u64(&mut self) -> u64;
}
```

`SlotWriter` accumulates `u64` values into an inline array (up to `MAX_IN_SLOTS = 16`), spilling to heap only if exceeded. `SlotReader` reads values sequentially from a slice. Both are used exclusively by generated code — no manual slot manipulation is needed.

### Supported Types

| Type | Slots | Encoding |
|------|-------|----------|
| `()`, `bool` | 0–1 | Unit = no slots. Bool = 0 or 1. |
| `u8`..`u64`, `i8`..`i64`, `f64` | 1 | Cast to `u64` (floats via `to_bits`). |
| `[u8; N]` | N/8 | Little-endian u64 halves. N must be a multiple of 8. Runtime support: N = 16. |
| `&str`, `&[u8]`, `String`, `Vec<u8>` | 2 | `(ptr, len)` — zero-copy borrow for refs, clone for owned. |
| `&mut Vec<u8>` | 1 | Raw pointer to the `Vec` struct. Spier writes directly into the caller's allocation. |
| `Vec<T: Clone>` (input) | 2 | `(ptr, len)` borrow — spier clones elements from caller's memory. Works for any `T: Clone` including `Vec<String>`, `Vec<Vec<u8>>`, nested Vecs. Rust→Rust only; Python cannot construct Rust memory layouts. |
| `Vec<T>` (output) | 2 | `(ptr, len)` with ownership transfer via `Box::into_raw` / `Box::from_raw`. |
| `Option<T>` | 1 + T | Tag (0=None, 1=Some) followed by T's slots. |
| `(A, B)` | A + B | Concatenation of A's and B's slots. |
| `Result<T, E>` | 1 + T or E | Tag (0=Ok, 1=Err) followed by the variant's slots. |
| Enums (DSL `enum`) | 1 + fields | Discriminant + each field's slots. |
| Structs (DSL `struct` / `opaque struct`) | 1 | Opaque boxed pointer (`Box::into_raw` / `Box::from_raw`). Rust dereferences directly; Python receives an opaque handle. |

### Key Design Points

- **Borrows are zero-copy**: `&[u8]` passes `(raw_ptr, len)` — the spier reads the host's memory directly. No copy, no allocation.
- **Out-params via `&mut Vec<u8>`**: the host passes a raw pointer to its `Vec`. The spier pushes data into it. Changes are visible to the host immediately. This is the `IDL_OUT_VEC` pattern.
- **Ownership transfer on returns**: `Vec<T>` is moved across the boundary via `Box::into_raw` / `forget` on the spier side, `Box::from_raw` on the host side. No copy.
- **`Vec<T: Clone>` input**: the caller sends `(ptr, len)` pointing to its own heap memory. The spier clones elements via `slice::from_raw_parts(ptr, len).to_vec()`. Elements are never individually slot-encoded — always 2 slots regardless of count or element complexity. Rust→Rust only; Python callers serialize complex Vecs to `&[u8]`.
- **`Result<T, String>` is universal**: every dispatch method returns `Result<T, String>`. The tag slot (0/1) distinguishes Ok from Err. The generated tower wrapper decodes the tag and returns `Result<R, String>` — the outer layer is transport errors, the inner is the spier's application error.

---

## IDL Schema Export

Every spier exports a static `DynSpireIdl` struct via `dynspire_idl_schema()`:

```rust
#[repr(C)]
pub struct DynSpireIdl {
    pub name: *const u8,
    pub name_len: usize,
    pub hash: u64,
    pub type_table: *const IdlTypeNode,
    pub type_count: usize,
    pub methods: *const IdlMethod,
    pub method_count: usize,
    pub enum_table: *const *const EnumDescriptor,
    pub enum_count: usize,
}
```

This allows any caller (Rust or Python) to discover at runtime:
- The IDL hash (for compatibility verification)
- All methods, their names, parameter names/types, and return types
- All enum variants with their field types

The type table uses a node-based representation:

```rust
#[repr(C)]
pub struct IdlTypeNode {
    pub kind: u8,           // IDL_U8, IDL_SLICE, IDL_VEC, IDL_TUPLE, ...
    pub child_count: u8,    // Number of valid children (0-8)
    pub size: u32,          // Array length (for IDL_ARRAY)
    pub children: [i32; 8], // Indices into type_table, or -1
}
```

Composite types are built recursively: `Vec<u8>` is `IDL_VEC` with `children[0]` pointing to `IDL_U8`. `Option<(String, u64)>` is `IDL_OPTION` with `children[0]` pointing to `IDL_TUPLE(String, U64)`. Tuples of up to 8 elements are supported (matching the 8-slot FFI limit).

---

## DSL Codegen

The `.dspi` file is the contract. `dynspire-codegen` parses it and generates all Rust code via `build.rs`. No proc macros, no `syn`-based type parsing — the grammar is closed and each production maps 1:1 to a slot encoding strategy.

### `.dspi` grammar

```
include "shared_types.dspi";

interface Rle {
  struct CompressionReport {
    original_size: u64,
    ratio: f64,
  }

  enum Tone {
    Quiet,
    Loud(u8),
  }

  opaque struct ExternalHandle;

  fn compress(data: &[u8]) -> Vec<u8>;
  fn compress_into(data: &[u8], out: &mut Vec<u8>) -> ();
  fn analyze(data: &[u8]) -> CompressionReport;
}
```

`include` directives appear before `interface` and import types from **type fragment** files — files containing only `struct`, `enum`, and `opaque struct` declarations (no `interface` wrapper, no `fn`). Included types are merged into the interface before hashing, so the IDL hash is automatically compositional. Paths resolve relative to the including file. Circular includes are an error; diamond includes (same fragment via different paths) are deduplicated.

Return types are the `Ok` variant — `Result<_, String>` is implicit. The type grammar is a closed set:

| DSL syntax | Rust | Slots | Type table node |
|---|---|---|---|
| `u8`..`u64`, `i8`..`i64`, `f32`, `f64`, `bool` | same | 1 | `IDL_U8`/`U32`/`U64`/`F32`/`F64`/`BOOL` |
| `&[u8]` | `&[u8]` | 2 (ptr,len) | `IDL_SLICE<U8>` |
| `&str` | `&str` | 2 (ptr,len) | `IDL_STR` |
| `&mut Vec<u8>` | `&mut Vec<u8>` | 1 (raw ptr) | `IDL_OUT_VEC` |
| `String` | `String` | 2 (owned) | `IDL_STRING` |
| `Vec<T>` | `Vec<T>` | 2 (owned) | `IDL_VEC<T>` |
| `Option<T>` | `Option<T>` | 1+T | `IDL_OPTION<T>` |
| `(A, B, ...)` | `(A, B, ...)` | sum of elements | `IDL_TUPLE<A,B,...>` (up to 8) |
| Named struct/enum/opaque | same | boxed ptr (1) or disc+fields | `IDL_STRUCT`/`IDL_ENUM` |

### Grammar rules

The parser enforces these constraints:

- **Keywords**: `interface`, `struct`, `enum`, `opaque`, `fn`, `mut`, `include` — cannot be used as identifiers.
- **Comments**: `//` line comments only. No `/* */` block comments.
- **Interface**: exactly one per file. Must contain at least one method. Trailing tokens after the closing `}` are an error.
- **Includes**: `include "path";` directives before `interface` import types from fragment files. Fragments contain only type declarations (no `fn` or `interface`). Paths resolve relative to the including file. Fragments can include other fragments (recursive). Circular includes are an error. Diamond includes are deduplicated. Conflict (same type name from different sources) is an error. Each included file triggers `cargo:rerun-if-changed`.
- **Tuples**: 2–8 elements. Single-element parens `(X)` collapse to `X` (not a tuple).
- **Borrow constraints**: `&[` accepts only `u8`; `&mut` accepts only `Vec<u8>`. Other borrow types are parse errors.
- **Named type references**: every named type used in params, returns, struct fields, enum variants, or inside `Vec`/`Option`/`Tuple` must be declared in the same interface or included from a fragment. Undeclared references are a parse error.
- **Trailing commas**: allowed in struct fields, enum variants, and tuple elements.

### Generated artifacts

The `build.rs` calls `dynspire_codegen::build("src/my.dspi")`, which writes `OUT_DIR/my_idl.rs`. This single file contains:

| Output | Used by |
|--------|---------|
| `pub trait {Name}Engine: Send + Sync` | Spier + Host |
| `pub struct {Type}` / `pub enum {Type}` | Spier + Host |
| `pub enum {Name}Op` + `impl SpierOp` | Host |
| `pub const {NAME}_IDL_HASH: u64` | Spier + Host |
| `pub static IDL: IdlDescriptor` | Host |
| `pub fn idl_schema()` + `DynSpireIdl` + `dynspire_free()` | Spier (export) + Python (read) |
| `pub struct DynSpire{Name}` + `impl {Name}Engine` | Host |
| `#[macro_export] macro_rules! impl_{name}_spier!` | Spier |

Symbol names are derived from the interface name:

| Interface name | Generated symbol | Example (`interface My`) |
|---|---|---|
| `interface {N}` | `pub trait {N}Engine` | `MyEngine` |
| | `pub enum {N}Op` | `MyOp` |
| | `pub struct DynSpire{N}` | `DynSpireMy` |
| | `pub const {N_UPPER}_IDL_HASH: u64` | `MY_IDL_HASH` |
| | `macro_rules! impl_{n_lower}_spier!` | `impl_my_spier!` |
| | output file: `{n_lower}_idl.rs` | `my_idl.rs` |

### Codegen API

The `dynspire-codegen` crate exposes:

```rust
// build.rs entry point — reads file, parses, generates, writes to OUT_DIR.
// Emits cargo:rerun-if-changed for the .dspi file. Panics on error.
pub fn build(dspi_path: &str);

// Shared context for deduplicating types across multiple build() calls.
// When two .dspi files include the same type fragment, BuildContext
// skips the duplicate definition (same name + same canonical signature).
// Conflicting types (same name, different content) are a hard error.
pub struct BuildContext { /* ... */ }
impl BuildContext {
    pub fn new() -> Self;
    pub fn build(&mut self, dspi_path: &str);
}

// AST → full Rust source string (for testing or custom build scripts).
pub fn generate(iface: &Interface) -> String;

// Source text → AST (for tooling, tests, IDE integration).
pub fn parse(src: &str) -> Result<Interface, ParseError>;
```

**Single interface** — backward compatible:

```rust
// build.rs
fn main() { dynspire_codegen::build("src/my.dspi"); }
```

**Multiple interfaces sharing types** — use `BuildContext`:

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build("src/a.dspi");   // generates SharedHandle
    ctx.build("src/b.dspi");   // skips SharedHandle (already emitted, same content)
}
```

The AST types (`Interface`, `Method`, `FieldType`, `TypeDecl`, etc.) are re-exported via `dynspire_codegen::ast::*`. The crate has no dependency on the `dynspire` runtime — generated code references `dynspire::*`, but the codegen itself only produces strings.

### Spier dispatch macro

The generated `macro_rules!` takes `$state:ty`, `$init:path`, and `$name:literal`:

```rust
// In the spier crate:
impl RleEngine for RleState {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String> { /* business logic */ }
    // ...
}

rle_idl::impl_rle_spier!(RleState, init, "rle");
```

The macro expands to all `dynspire_dispatch_{method}` functions (decoding args from slots, calling the trait method, encoding the result), plus `dynspire_create`, `dynspire_destroy`, `dynspire_idl_hash`, `dynspire_spier_name`, and `dynspire_idl_schema`.

### Application errors: use IDL-declared enums

Every method return is wrapped in `Result<T, String>` — this is the **transport layer** (null handle, buffer overflow, init failure). Application errors need a separate mechanism.

The idiomatic pattern is a **custom Result enum declared in the `.dspi` file**:

```
interface Parser {
    enum ParseError {
        InvalidFormat,
        TooLarge(u64),
    }

    enum ParseResult {
        Ok(u64),
        Err(ParseError),
    }

    fn parse(data: &[u8]) -> ParseResult;
}
```

This generates `fn parse(&self, data: &[u8]) -> Result<ParseResult, String>`. The slot layout is `[transport_tag, enum_discriminant, ...field_slots]`:

- `transport_tag = 1` → transport error (the `String` from `Result<_, String>`)
- `transport_tag = 0, discriminant = 0` → `ParseResult::Ok(value)`
- `transport_tag = 0, discriminant = 1` → `ParseResult::Err(error)`

On the host side, `?` handles transport errors; `match` handles application errors:

```rust
match client.parse(data)? {
    ParseResult::Ok(value) => println!("parsed: {value}"),
    ParseResult::Err(ParseError::TooLarge(max)) => println!("too large (max {max})"),
    ParseResult::Err(ParseError::InvalidFormat) => println!("invalid"),
}
```

Python sees the enum natively via schema reflection:

```python
result = h.parse(data)
if result.variant == "Ok":
    print(result.fields[0])
elif result.variant == "Err":
    err = result.fields[0]
    # err is itself a SpierEnumValue
```

**Why not `Result<T, E>` in the DSL?** The implicit `Result<T, String>` wrapping is always present (transport). If the DSL supported `-> Result<T, E>`, it would generate `Result<Result<T, E>, String>` — nesting that's mechanically correct but semantically confusing. Custom enums avoid the nesting entirely: the transport `Result<_, String>` wraps the user's enum, and the enum's discriminant IS the application-level Ok/Err tag.

Additional benefits: enums support 3+ variants (`Ok`, `Err`, `Partial`), are fully reflected in the schema, and don't mix with the application's internal `Result` types — the IDL enum is the boundary contract.

---

## Tower Client

The tower client (`DynSpireClient`) is the Rust host-side API. The generated `DynSpire{Name}` wrapper uses it internally:

```rust
// One-line setup — generated wrapper, no handwritten boilerplate
let client = DynSpireRle::connect("rle_spier", &config)?;

// Type-safe dispatch via the generated trait
let result: Vec<u8> = client.compress(&input[..])?;
```

`connect()` performs:
1. `DynSpireLib::find(name)` — resolves `.so` path
2. `libloading::Library::new(path)` — `dlopen`
3. Hash verification — calls `dynspire_idl_hash()`, compares with expected
4. Symbol resolution — loads `dynspire_dispatch_{method}` for each method
5. Instance creation — calls `dynspire_create(config)`

The `SpierOp` trait ensures `dispatch()` only accepts the generated Op enum — raw integers are a compile-time error.

---

## Python PyO3 Adapter

The Python adapter (crate `dynspire-py`, package `dynspire`) is a compiled PyO3 extension that provides the same capabilities as the Rust tower client. Decode runs in Rust — owned `Vec`/`String` reconstruction uses `Box::from_raw` natively, so there is no stride arithmetic and no per-call `dynspire_free` for data returns.

### Schema Reflection

```python
lib = load_spier("rle_spier", lib_dir="target/debug")
schema = lib.schema()

# Methods — returns SpierMethod objects with .name, .params, .return_type, .index
for m in schema.methods:
    print(schema.method_sig(m))
    # compress(data: Slice<U8>) -> Result<Vec<U8>, String>

# Detailed introspection
m = schema.method("compress")        # SpierMethod
p = m.params[0]                      # SpierParam (.name, .type_idx)
ti = schema.type_at(p.type_idx)      # SpierTypeInfo (.kind_name)
enum = schema.enum_by_name("Tone")   # SpierEnumSchema (.name, .variant_names)
EnumCls = enum.create_enum_class()   # SpierEnumClass — VariantName(payload) → SpierEnumValue

assert lib.idl_hash() == schema.hash
```

The engine reads the `DynSpireIdl` C struct via `libloading`, walking the type table and method table to build an in-memory `SchemaData` (method descriptors, type nodes, enum descriptors). All schema objects (`SpierMethod`, `SpierParam`, `SpierTypeInfo`, `SpierEnumSchema`) are lightweight snapshots constructed on demand from this shared `Arc<SchemaData>`.

### Calling Convention

```python
with lib.create_handle() as h:
    # Attribute access (recommended)
    compressed = h.compress(input_data)

    # Escape hatch for dynamic method names
    compressed = h.call("compress", input_data)

    # Out-vec methods (&mut Vec<u8>) auto-return (ret_val, list[bytes])
    ok, outs = h.compress_into_checked(input_data)
```

Attribute access (`h.compress(data)`) returns a `BoundMethod` that dispatches through the same unified path as `h.call("compress", data)`. The engine auto-detects out-vec parameters from the schema — no separate `call_with_outs` is needed. When a method has `&mut Vec<u8>` params, the call returns `(ret_val, list[bytes])`; otherwise it returns the decoded value directly.

The engine handles:

- **Borrows** (`&[u8]`, `&str`): borrows Python memory directly (GIL is held during the call, so the borrow is sound)
- **Owned returns** (`Vec<u8>`, `String`, tuples, enums): reconstructed in Rust via `Box::from_raw` and converted to Python objects; dropped normally
- **Opaque struct returns**: wrapped in `OpaqueHandle` (holds the boxed pointer, frees via `dynspire_free` on GC; can be passed back as an input)
- **OutVec** (`&mut Vec<u8>`): creates a Rust `Vec` via `dynspire_vec_create()`, passes the handle, snapshots contents via `dynspire_vec_view()`, frees via `dynspire_vec_free()`
- **Enums** (DSL `enum`): decoded to `SpierEnumValue` (variant name + payload tuple); can be passed back as an input
- **Result<T, String>**: reads the tag slot, decodes the Ok value or raises `RuntimeError` with the error string

### Lifecycle

```python
with lib.create_handle() as h:
    h.compress(data)
    # destroy() called automatically on __exit__
```

`SpierHandle` implements `Drop` — calls `dynspire_destroy()` if the `with` block wasn't used. The `.so` stays mapped (`Arc<Library>`) until the last handle, `OpaqueHandle`, or `BoundMethod` referencing it is dropped, so `f = h.compress; del h; f(data)` is safe.

---

## Path Resolution

Both Rust and Python follow the same three-tier resolution:

| Priority | Rust | Python |
|----------|------|--------|
| 1 | Explicit path in `DynSpireLib::load(path)` | `lib_dir=` parameter |
| 2 | `DYNSPIRE_LIB_DIR` env var → constructs full path | `DYNSPIRE_LIB_DIR` env var → constructs full path |
| 3 | Bare filename → `dlopen` resolves via `LD_LIBRARY_PATH` | Bare filename → `dlopen` resolves via `LD_LIBRARY_PATH` |

Levels 1 and 2 construct a full filesystem path before calling `dlopen` — no side effects on the system's dynamic linker. Level 3 delegates entirely to `dlopen`, which searches `LD_LIBRARY_PATH`, `/usr/lib`, `/usr/local/lib`, etc.

This means spiers installed in standard library paths work out of the box, while project-specific spiers can be isolated via `DYNSPIRE_LIB_DIR` without polluting the global library search path.
