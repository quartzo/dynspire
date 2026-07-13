# DynSpire Architecture

This document covers the internals of the DynSpire plugin framework: the slot system, FFI conventions, DSL codegen, and the Python ctypes client.

## Table of Contents

- [Overview](#overview)
- [Slot System](#slot-system)
- [DSL Codegen](#dsl-codegen)
- [Tower Client](#tower-client)
- [Python Client](#python-client)
- [Path Resolution](#path-resolution)

---

## Overview

DynSpire is a plugin architecture with three roles:

> **Read this first — DynSpire is an in-process plugin ABI, not an RPC framework.** A spier is a `cdylib` loaded into the host via `dlopen`; host and spier run in **the same process and the same address space**. Crossing the boundary means a **C ABI call with a flat `u64[]` slot convention** — there is no network, no IPC, no wire format. Consequences that follow from this and *do not* hold under an RPC mental model:
>
> - **Opaque pointers are valid across the boundary.** DSL-declared structs pass `Box::into_raw` → 1 slot (the raw address); the receiver does `Box::from_raw` or dereferences directly. The struct is **not serialized** — it stays live, with whatever state machine, lock, or inner reference it holds. Both sides alias the same memory.
> - **"IDL" / "schema" = in-process ABI contract.** Not a serialization descriptor, not a protobuf schema. The IDL hash gates **binary/link compatibility** between two `.so`s compiled against the same interface — not message-level versioning.
> - **The boundary is compile/link, not process.** Borrows (`&[u8]`, `&str`), out-params (`&mut Vec<u8>`), and ownership-transfer (`Vec<T>` via `Box::into_raw`) are all sound precisely because caller and callee share one heap. None of this is possible across a process boundary; all of it is routine here.

| Role | Who | What |
|------|-----|------|
| **IDL** | `.dspi` file | A `.dspi` file is the single source of truth. The spier crate owns the `.dspi` and calls `build_spier()` in its `build.rs`. The host crate compiles the same `.dspi` by path and calls `build_host()`. Each side gets only the code it needs. |
| **Spier** | `cdylib` crate | Owns the `.dspi`. Calls `dynspire_codegen::build_spier("src/my.dspi")` in `build.rs`. Implements the generated trait. Invokes `impl_{name}_spier!($state, init, "name")` — a generated `macro_rules!` that produces all C-ABI entry points (`dynspire_create`, `dynspire_dispatch_{method}`, `dynspire_destroy`, etc.) and the IDL schema. No proc macros. |
| **Host** | Binary or script | Compiles the same `.dspi` by path. Calls `dynspire_codegen::build_host("../my-spier/src/my.dspi")` in `build.rs`. Uses the generated `DynSpire{Name}` client wrapper directly. One import, no boilerplate. Python hosts skip codegen entirely and read the schema at runtime. |

> **The IDL hash is the contract, not the crate dependency.** The spier and host
> can each compile the `.dspi` independently — both sides produce the same hash
> from the same interface signature, so `connect()` accepts the spier. The FFI
> boundary uses raw pointers and slots, never Rust type identity, so
> independently-compiled types from the same `.dspi` are layout-compatible.

```
┌─────────────────────────────────────────────┐
│  Host (Rust binary or Python script)        │
│                                             │
│  DynSpireClient                              │
│    .dispatch(Op, in_slots, out_slots)        │
│    → SlotWriter encodes args as u64[]       │
│    → dispatch via FFI                       │
│    → SlotReader decodes response u64[]      │
└──────────────────┬──────────────────────────┘
                   │ libloading / ctypes
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

The spier and host each compile the `.dspi` with a different `build.rs` entry point. Each side gets only the code it needs:

**Spier side** — `dynspire_codegen::build_spier("src/my.dspi")` writes `OUT_DIR/my_spier.rs`:

| Output | Used by |
|--------|---------|
| `pub trait {Name}Engine: Send + Sync` | Spier (implement) |
| `pub struct {Type}` / `pub enum {Type}` | Spier |
| `pub enum {Name}Op` | Spier (dispatch index) |
| `pub const {NAME}_IDL_HASH: u64` | Spier |
| `#[macro_export] macro_rules! impl_{name}_spier!` | Spier — expands to dispatch functions, `dynspire_create`/`destroy`, `dynspire_free()`, and `dynspire_idl_hash` |

**Host side** — `dynspire_codegen::build_host("../my-spier/src/my.dspi")` writes `OUT_DIR/my_host.rs`:

| Output | Used by |
|--------|---------|
| `pub trait {Name}Engine: Send + Sync` | Host (trait bounds) |
| `pub struct {Type}` / `pub enum {Type}` | Host |
| `pub enum {Name}Op` + `impl SpierOp` | Host |
| `pub const {NAME}_IDL_HASH: u64` | Host |
| `pub static IDL: IdlDescriptor` | Host (connect) |
| `pub struct DynSpire{Name}` + `impl {Name}Engine` | Host |

Symbol names are derived from the interface name:

| Interface name | Generated symbol | Example (`interface My`) |
|---|---|---|
| `interface {N}` | `pub trait {N}Engine` | `MyEngine` |
| | `pub enum {N}Op` | `MyOp` |
| | `pub struct DynSpire{N}` | `DynSpireMy` |
| | `pub const {N_UPPER}_IDL_HASH: u64` | `MY_IDL_HASH` |
| | `macro_rules! impl_{n_lower}_spier!` | `impl_my_spier!` |
| | output file (spier): `{n_lower}_spier.rs` | `my_spier.rs` |
| | output file (host): `{n_lower}_host.rs` | `my_host.rs` |

### Codegen API

The `dynspire-codegen` crate exposes:

```rust
// Spier side — reads file, parses, generates spier code, writes to OUT_DIR.
// Emits cargo:rerun-if-changed for the .dspi file. Panics on error.
impl BuildContext {
    pub fn build_spier(&mut self, dspi_path: &str);
}

// Host side — same, but generates host code (IDL + tower, no spier macro).
impl BuildContext {
    pub fn build_host(&mut self, dspi_path: &str);
}

// Legacy — generates both sides in a single file (backward compatible).
impl BuildContext {
    pub fn build(&mut self, dspi_path: &str);
}

// Shared context for deduplicating types across multiple build calls.
// When two .dspi files include the same type fragment, BuildContext
// skips the duplicate definition (same name + same canonical signature).
// Conflicting types (same name, different content) are a hard error.
pub struct BuildContext { /* ... */ }
impl BuildContext {
    pub fn new() -> Self;
    pub fn build(&mut self, dspi_path: &str);          // both sides
    pub fn build_spier(&mut self, dspi_path: &str);    // spier side only
    pub fn build_host(&mut self, dspi_path: &str);     // host side only
}

// Source text → AST (for tooling, tests, IDE integration).
pub fn parse(src: &str) -> Result<Interface, ParseError>;
```

**Spier crate** — the `.dspi` lives here:

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_spier("src/my.dspi");
}
```

**Host crate** — compiles the same `.dspi` by path:

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_host("../my-spier/src/my.dspi");
}
```

**Multiple interfaces sharing types** — use `BuildContext`:

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_spier("src/a.dspi");   // generates SharedHandle
    ctx.build_spier("src/b.dspi");   // skips SharedHandle (already emitted, same content)
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

impl_rle_spier!(RleState, init, "rle");
```

The macro expands to all `dynspire_dispatch_{method}` functions (decoding args from slots, calling the trait method, encoding the result), plus `dynspire_create`, `dynspire_destroy`, `dynspire_spier_name`, `dynspire_idl_hash`, and `dynspire_free()`. All symbols are scoped inside the macro — no module-scope conflicts when two `.dspi` files are compiled in the same crate.

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
let client = DynSpireRle::connect("rle_spier", &config, false)?;

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

## Python Client

The spier's `build.rs` emits a self-contained Python ctypes client (`.py`) alongside the Rust code. The generated module inlines all runtime primitives (`SlotWriter`, `SpierClient`, `OpaqueHandle`, out-vec helpers) — no external package dependency, only `ctypes` from the Python stdlib. Python users import the generated module directly; no PyO3, no maturin, no Rust toolchain required on the consuming side.

### Calling Convention

```python
from rle import Rle, Tone, CompressionReport

with Rle("target/debug/librle_spier.so") as c:
    compressed = c.compress(b"AAAABBBBCCCC")

    # Out-vec methods (&mut Vec<u8>) return (ret_val, list[bytes])
    ok, outs = c.compress_into_checked(b"AAAABBBBCCCC")

    # Enums are typed Python objects
    tone = c.classify(b"AAAABBBBCCCC")     # Tone.Loud(71)
    desc = c.describe_tone(Tone.Quiet())    # "silence"

    # Structs with fields are typed Python objects (not opaque handles)
    report = c.analyze(b"AAAABBBBCCCC")     # CompressionReport
    print(report.original_size)             # field accessor
    summary = c.report_summary(report)      # pass back to spier
```

The generated client handles:

- **Borrows** (`&[u8]`, `&str`): copies Python bytes into a ctypes array pinned for the call duration, passes `(ptr, len)` slots
- **Owned returns** (`Vec<u8>`, `String`): copied into Python `bytes`/`str` via `ctypes.string_at`, then released via `dynspire_release`
- **Struct returns** (`CompressionReport`, `NamedRun`, etc.): decoded via `{Name}._from_ptr()` — a classmethod that copies from the raw pointer into a ctypes mirror buffer, then constructs a wrapper with `@property` accessors, `__repr__`, `__eq__`, `__hash__`. Released via `dynspire_release` on `__del__` (triggers `drop_fn` for structs with dynamic fields like `DString`/`DVec`)
- **Opaque struct returns** (no fields): wrapped in bare `OpaqueHandle` (boxed pointer, freed on GC)
- **OutVec** (`&mut Vec<u8>`): the host passes a `DVec<u8>` (allocator + ptr/len/cap) backed by the host allocator; the spier fills it via `dynspire_realloc` and the host copies the bytes back, then releases the buffer via `dynspire_release`
- **Enums** (DSL `enum`): decoded to typed Python classes generated at codegen time; can be passed back as an input
- **Result<T, String>**: reads the tag slot, decodes the Ok value or raises `RuntimeError` with the error string

### Struct Codegen

Structs declared in the IDL with fields generate typed Python classes with full field access — not just opaque handles.

For each struct `Foo { name: DString, count: u64 }`, the codegen emits:

1. **`FooCtypes(ctypes.Structure)`** — ctypes mirror class with `_fields_` mapping each IDL field to its ctypes equivalent (`DString`, `c_uint64`, etc.)
2. **`Foo(OpaqueHandle)`** — wrapper class with:

| Feature | Description |
|---------|-------------|
| `__init__(self, name, count)` | Each field is a positional parameter; builds ctypes buffer from values |
| `_from_ptr(cls, client, ptr)` | Classmethod: copies from raw pointer via `ctypes.memmove`, reads field values from the buffer |
| `_default(cls)` | Classmethod: constructs with all-default values (used for nested struct defaults) |
| `@property` accessors | Read-only properties for each field (e.g., `obj.name`, `obj.count`) |
| `__repr__` | Shows all field values: `Foo(name='hello', count=5)` |
| `__eq__` / `__hash__` | Structural equality and hashing based on all fields |

Opaque structs (no fields) generate a bare `class Foo(OpaqueHandle): pass` — no field access.

```python
from rle import NamedRun

# Construct from field values
run = NamedRun(label="A", count=42)

# Access fields via properties
print(run.label)   # "A"
print(run.count)   # 42

# Value semantics
run2 = NamedRun(label="A", count=42)
assert run == run2
assert hash(run) == hash(run2)

# repr
repr(run)  # "NamedRun(label='A', count=42)"

# Passed to spier methods — write_opaque uses the ctypes pointer
result = c.make_named_run("B", 10)
print(result.label)  # "B"
```

Internally, `_from_ptr` is used by the generated decode logic when the spier returns a struct. The ctypes buffer holds the wire data (pointer `u64` for dynamic fields like `DString`/`DVec`), and the wrapper's `@property` accessors read from the buffer fields directly. `__del__` releases the backing pointer via `dynspire_release` (triggering `drop_fn` for structs with dynamic fields).

### Lifecycle

```python
with Rle("target/debug/librle_spier.so") as c:
    c.compress(data)
    # dynspire_destroy() called automatically on __exit__
```

`SpierClient` is its own context manager — calls `dynspire_destroy()` on `__exit__` or `__del__`. The `.so` stays mapped via `ctypes.CDLL` for the lifetime of the client object (ctypes does not call `dlclose`).

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
