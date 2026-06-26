# DynSpire

A Rust plugin framework for loading native `.so` libraries at runtime — with self-describing IDL schemas, zero-copy FFI, and Python bindings.

> **In-process by design.** A spier is a `.so` loaded into the host via `dlopen` — same process, same address space. Arguments and return values cross the boundary through a flat `u64[]` slot convention over a C ABI: borrows and owned values pass by raw pointer, and opaque structs hand over a boxed pointer (1 slot) rather than a serialized copy, so live objects cross freely.

## Why?

You wrote a Rust library. You want to load it at runtime as a plugin — discover its methods, call them, and get typed results back. Without hand-writing FFI boilerplate. Without stubs.

DynSpire does that — a `.dspi` file is the contract; `build.rs` generates everything.

## In 30 Seconds

Define an interface in a `.dspi` file:

```
interface Rle {

  // Type declarations
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

  // Methods — Result<T, String> is implicit on every return
  fn compress(data: &[u8]) -> Vec<u8>;
  fn decompress(data: &[u8]) -> Vec<u8>;
  fn compress_into(data: &[u8], out: &mut Vec<u8>) -> ();
  fn stats(data: &[u8]) -> (u64, u64);
  fn analyze(data: &[u8]) -> CompressionReport;
  fn report_summary(report: CompressionReport) -> String;
  fn classify(data: &[u8]) -> Tone;
  fn first_byte(data: &[u8]) -> Option<u8>;
}
```

`build.rs` generates the trait, types, Op enum, schema, tower client, and spier dispatch macro. Implement the trait and load it:

```rust
// Spier crate
impl RleEngine for RleState {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String> { /* ... */ }
    fn analyze(&self, data: &[u8]) -> Result<CompressionReport, String> { /* ... */ }
    // ...
}
rle_idl::impl_rle_spier!(RleState, init, "rle");
```

```rust
// Host crate
let client = DynSpireRle::connect("rle_spier", &config)?;
let compressed: Vec<u8> = client.compress(&input[..])?;
let report = client.analyze(&input[..])?;  // typed CompressionReport
```

Or from Python — with full schema reflection:

```python
with load_spier("rle_spier", lib_dir="target/debug").create_handle() as h:
    compressed = h.compress(input_data)
    report = h.analyze(input_data)   # OpaqueHandle
```

## Features

- **DSL-driven** — a `.dspi` file is the single source of truth. `build.rs` generates trait, types, Op enum, schema, tower client, and spier dispatch. No proc macros on business code.
- **Self-describing** — spiers export their full IDL schema (methods, types, enums) via a C ABI. Hosts discover everything at runtime.
- **Zero-copy FFI** — borrows (`&[u8]`, `&str`) and mutable out-params (`&mut Vec<u8>`) pass through raw pointers. No serialization overhead. `Vec<T: Clone>` input works for any element type (Rust→Rust).
- **Type-safe dispatch** — Rust hosts use the generated tower wrapper. No magic numbers, no manual slot encoding.
- **IDL hash verification** — incompatible plugins are rejected at load time.
- **Python without codegen** — a PyO3 extension reads the IDL schema from the `.so` directly. No stub generation, no `bindgen`, no C headers.
- **Any return type** — `Result<T, String>` where `T` can be `()`, `Vec<u8>`, `(u64, u64, u64)`, `Option<String>`, any DSL-declared enum or struct, or any composed combination. Application errors use IDL-declared enums (e.g., `enum ParseResult { Ok(u64), Err(ParseError) }`) — self-contained, schema-reflected, no Result nesting.

## DSL Reference

The `.dspi` file declares one `interface` containing type declarations and method signatures. It is the single source of truth — `build.rs` generates all Rust code from it. Type declarations can be shared across interfaces via `include` directives that pull in **type fragment** files.

### Declarations

| Declaration | Syntax | Notes |
|-------------|--------|-------|
| **Include** | `include "path.dspi";` | Imports types from a fragment file (no `interface` wrapper). Paths are relative to the including file. Placed before `interface`. |
| **Struct** | `struct Name { field: Type, ... }` | Crosses FFI as a boxed pointer (1 slot). Trailing comma allowed. |
| **Enum** | `enum Name { Variant, Variant(Type, ...), ... }` | Unit variants and tuple variants. Trailing comma allowed. |
| **Opaque struct** | `opaque struct Name;` | No body — same FFI behavior as `struct` but no field access. Use for handles you only pass around. |
| **Method** | `fn name(param: Type, ...) -> Type;` | Arrow + return type required. Every return is implicitly `Result<T, String>`. |

### Type grammar

| DSL syntax | Rust equivalent | Slots | Notes |
|------------|-----------------|-------|-------|
| `()`, `bool` | `()`, `bool` | 0–1 | Unit = no slots |
| `u8` `u16` `u32` `u64` | same | 1 | Zero-extended to `u64` |
| `i8` `i16` `i32` `i64` | same | 1 | Sign-extended to `u64` |
| `f32` `f64` | same | 1 | Via `to_bits()` |
| `&[u8]` | `&[u8]` | 2 | Zero-copy borrow. **Only `u8`** accepted. |
| `&str` | `&str` | 2 | Zero-copy borrow |
| `&mut Vec<u8>` | `&mut Vec<u8>` | 1 | Raw pointer — spier writes directly. **Only `Vec<u8>`** accepted. |
| `String` | `String` | 2 | Owned |
| `Vec<T>` | `Vec<T>` | 2 | Owned. `T` can be any type: `Vec<u8>`, `Vec<String>`, `Vec<Vec<u8>>`, ... |
| `Option<T>` | `Option<T>` | 1 + T | Tag + inner |
| `(A, B, ...)` | `(A, B, ...)` | sum | **2–8 elements** (matches slot limit). Single-element `(X)` collapses to `X`. |
| `[u8; N]` | `[u8; N]` | N/8 | Fixed-size byte array. **N must be a multiple of 8.** Runtime support: `N = 16`. |
| Named type | same | 1 (boxed ptr) or disc+fields | Must be a declared struct/enum/opaque in the same interface |

### Syntax rules

- Comments: `//` line comments only (no `/* */`)
- Keywords: `interface`, `struct`, `enum`, `opaque`, `fn`, `mut`, `include`
- Trailing commas: allowed in struct fields, enum variants, and tuples
- Tuple arity: 2–8 elements
- Borrow constraints: `&[` only accepts `u8`; `&mut` only accepts `Vec<u8>`
- Named type references: must be declared in the same interface or included from a fragment — undeclared types are a parse error
- The interface must have at least one method
- Includes: `include "path";` directives appear before `interface`, import types from fragment files. Fragments contain only type declarations (no `fn`, no `interface`). Paths resolve relative to the including file. Circular includes are an error; diamond includes (same file via different paths) are deduplicated.

### Application errors

Every method return is implicitly `Result<T, String>` (transport layer: null handle, init failure, etc.). For application-level errors, declare a custom Result enum:

```
interface Parser {
    enum ParseError { InvalidFormat, TooLarge(u64) }
    enum ParseResult { Ok(u64), Err(ParseError) }

    fn parse(data: &[u8]) -> ParseResult;
}
```

The enum's discriminant IS the application-level Ok/Err tag — no `Result<Result<T,E>, String>` nesting. See [architecture.md](docs/architecture.md#application-errors-use-idl-declared-enums) for details.

## Crate Setup

The spier and host each compile the `.dspi` independently via `build.rs`. The generated IDL hash guarantees compatibility at load time — a shared crate is a convenience, not a requirement.

### Independent compilation (default)

Each crate has its own `.dspi` + `build.rs`:

```
my-spier/           my-host/
  Cargo.toml          Cargo.toml
  build.rs            build.rs
  src/
    my.dspi             src/
    lib.rs                my.dspi
                           main.rs
```

**Spier crate** (`Cargo.toml` deps: `dynspire-codegen`, `dynspire`):

```rust
// build.rs
fn main() { dynspire_codegen::build("src/my.dspi"); }
```
```rust
// lib.rs — include the generated code
#![allow(non_upper_case_globals)]
include!(concat!(env!("OUT_DIR"), "/my_idl.rs"));

// Implement the generated trait
impl MyEngine for MyState {
    fn do_thing(&self, x: &[u8]) -> Result<Vec<u8>, String> { /* ... */ }
}

fn init(_cfg: &HashMap<String, String>) -> Result<MyState, String> {
    Ok(MyState)
}

// Generate all C-ABI dispatch functions
impl_my_spier!(MyState, init, "my");
```

**Host crate** (same `.dspi`, same `build.rs`):

```rust
// main.rs — use the generated tower wrapper
#![allow(non_upper_case_globals)]
include!(concat!(env!("OUT_DIR"), "/my_idl.rs"));

let client = DynSpireMy::connect("my_spier", &config)?;
let result = client.do_thing(&input[..])?;
```

The IDL hash is computed from the interface's canonical signature — both sides produce the same hash from the same `.dspi`, so `connect()` accepts the spier.

### Multiple interfaces with shared types

When a host needs to talk to multiple spiers that share type fragments, use `BuildContext` to deduplicate type definitions:

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build("src/a.dspi");   // generates SharedHandle
    ctx.build("src/b.dspi");   // skips SharedHandle (already emitted, same content)
}
```

Types with the same name but different content are a hard error at codegen time.

### Shared IDL crate (optional convenience)

For single-team projects, extract the `.dspi` + `build.rs` into a shared crate that both spier and host depend on. This prevents version skew by construction:

```
my-idl/             my-spier/          my-host/
  Cargo.toml          Cargo.toml         Cargo.toml
  build.rs            src/lib.rs         src/main.rs
  src/my.dspi         (depends on        (depends on
  src/lib.rs           my-idl)            my-idl)
```

The shared crate pattern is used in the [demo](#demo). The demo's `rle-idl/` crate compiles once; `rle-spier/` and `rle-host/` both depend on it.

### Naming conventions

Symbol names are derived from the interface name in the `.dspi` file:

| Interface name | Generated symbol | Example (`interface My`) |
|---|---|---|
| `interface {N}` | `pub trait {N}Engine` | `MyEngine` |
| | `pub enum {N}Op` | `MyOp` |
| | `pub struct DynSpire{N}` | `DynSpireMy` |
| | `pub const {N_UPPER}_IDL_HASH: u64` | `MY_IDL_HASH` |
| | `macro_rules! impl_{n_lower}_spier!` | `impl_my_spier!` |
| | output file: `{n_lower}_idl.rs` | `my_idl.rs` |

### Python host (no codegen)

Python needs no `.dspi` or `build.rs` at all — the PyO3 extension reads the schema from the `.so` at runtime:

```python
from dynspire import load_spier
lib = load_spier("my_spier", lib_dir="target/debug")
```

## The Boundary as Discipline

The IDL + `.so` split isn't just about runtime loading — it's an architectural
constraint that enforces clean separation at compile time.

- **Every dependency is explicit.** The `.dspi` file defines the interface.
  The spier and host each compile it; whatever isn't in the `.dspi` doesn't
  cross the boundary. No sneaky imports, no shared private modules.
- **Interfaces stay focused.** Return types cross as ≤8 `u64` slots. You can't
  return a 50-field struct without consciously choosing an opaque struct
  declaration. This friction is intentional — it surfaces design problems at
  the interface, not at integration time.
- **Components are independently built and tested.** Each spier is a separate
  crate with its own `Cargo.toml`, test suite, and release cycle. You can't
  reach into another component's internals during a refactor.

This is particularly effective with LLM-assisted development. LLMs naturally
gravitate toward tight coupling — sharing types, building implicit dependencies,
reaching across boundaries. DynSpire makes those patterns impossible at compile
time. The only path through is a clean, explicitly declared interface.

## Performance

The FFI overhead per dispatch is ~5x a direct function call — tens of nanoseconds for slot encode + indirect call + decode. This is insignificant compared to any real work the function performs: a single `HashMap` lookup or `Vec` allocation already costs more. For plugins that do I/O, data processing, or storage operations, the overhead is unmeasurable noise.

## Demo

An RLE compression spier showcases the full cycle (shared-crate pattern):

```
demo/
  rle-idl/       .dspi interface + build.rs (generates trait, types, tower, macro)
  rle-spier/     cdylib implementation (loaded at runtime)
  rle-host/      Rust host binary
  rle_client.py  Python host (PyO3, schema reflection)
  rle_client2.py Showcase (out-vec auto-tuple, negative index, scalar Option)
```

```bash
# Build everything
cargo build

# Run Rust host
cargo run -p rle-host

# Run Python host
uv run python demo/rle_client.py
uv run python demo/rle_client2.py
```

Output:

```
compress()
  -> [04 41 03 42 04 43 05 44 04 45 06 46 03 47] (14 bytes)

decompress()
  -> "AAAABBBCCCCDDDDDEEEEFFFFFFGGG" (29 bytes) [round-trip OK]

compress_into(&mut Vec<u8>)
  out buffer : [04 41 03 42 ...] (14 bytes) [matches compress]

stats()
  original  : 29 bytes
  compressed: 14 bytes
  ratio     : 48.3%
```

## Project Layout

```
pyproject.toml     uv project root (declares dynspire-py as local dependency)
dynspire/          Core: arena FFI, slot system, tower client
dynspire-codegen/  DSL parser + code generator (.dspi → .rs)
dynspire-py/       Python bindings (PyO3, schema-driven, zero codegen)
demo/              RLE compression showcase (shared-crate pattern)
  rle-idl/           .dspi + build.rs (shared by spier and host)
  rle-spier/         cdylib implementation
  rle-host/          Rust host binary
```

## How It Works

```
  Host (Rust binary or Python script)
    │
    │  DynSpire{Name}::connect("my_spier", &config)
    │   1. find .so  (DYNSPIRE_LIB_DIR / LD_LIBRARY_PATH / explicit)
    │   2. dlopen
    │   3. verify IDL hash
    │   4. resolve dispatch functions
    │
    ▼
  Spier .so (cdylib, loaded at runtime)
    dynspire_create()   → *mut State
    dynspire_dispatch_{method}()  → encode args → call → encode result
    dynspire_destroy()  → free State
```

Arguments and return values flow through **u64 slots** — a compact calling convention that handles scalars, borrows, owned types, tuples, enums, and structs without heap allocation on the FFI boundary. Complex structs cross as opaque boxed pointers (1 slot) via the DSL's `opaque struct` declaration.

For the deep dive, see [docs/architecture.md](docs/architecture.md).

## Python Bindings

The Python adapter is a compiled PyO3 extension that loads any DynSpire `.so`
and discovers its full interface at runtime:

```python
from dynspire import load_spier

lib = load_spier("rle_spier", lib_dir="target/debug")
schema = lib.schema()

# Schema reflection
for m in schema.methods:            # list[SpierMethod]
    print(schema.method_sig(m))     # "compress(data: Slice<U8>) -> Result<Vec<U8>, String>"

# Introspect a single method
m = schema.method("compress")       # SpierMethod
#   m.name          -> "compress"
#   m.params        -> [SpierParam(name="data", type_idx=5)]
#   m.return_type   -> 3  (type-table index)
#   m.index         -> 0

# Type introspection
ti = schema.type_at(m.params[0].type_idx)   # SpierTypeInfo
#   ti.kind_name    -> "Slice"

# Enum introspection + value construction
tone_schema = schema.enum_by_name("Tone")   # SpierEnumSchema
#   tone_schema.variant_names -> ["Quiet", "Normal", "Loud"]
Tone = tone_schema.create_enum_class()      # SpierEnumClass
loud = Tone.Loud(71)                         # SpierEnumValue("Loud", (71,))

# lib.idl_hash() == schema.hash
assert lib.idl_hash() == schema.hash

# Call via attribute access with native Python types
with lib.create_handle() as h:
    compressed = h.compress(b"AAAABBBBCCCC")
    decompressed = h.decompress(compressed)

    # Out-vec methods (&mut Vec<u8>) auto-return (ret_val, list[bytes])
    ok, outs = h.compress_into_checked(b"AAAABBBBCCCC")

    # Dict args and kwargs also supported
    h.call("compress", {"data": b"AAAA"})
    h.compress(data=b"AAAA")
```

### Calling styles

All four are equivalent — use whichever reads best:

| Style | Example |
|-------|---------|
| Attribute (preferred) | `h.compress(data)` |
| Attribute + kwargs | `h.compress(data=data)` |
| `call` escape hatch | `h.call("compress", data)` |
| Dict args | `h.call("compress", {"data": data})` |

### Schema API reference

| Object | Property/Method | Returns |
|--------|----------------|---------|
| `SpierLib` | `.schema()` | `SpierSchema` |
| | `.idl_hash()` | `int` |
| | `.create_handle(config=None)` | `SpierHandle` |
| `SpierSchema` | `.name` | `str` |
| | `.hash` | `int` |
| | `.methods` | `list[SpierMethod]` |
| | `.method(name)` | `SpierMethod` |
| | `.method_sig(name_or_method)` | `str` |
| | `.type_at(type_idx)` | `SpierTypeInfo` |
| | `.enum_by_name(name)` | `SpierEnumSchema` |
| `SpierMethod` | `.name` | `str` |
| | `.index` | `int` |
| | `.params` | `list[SpierParam]` |
| | `.return_type` | `int` (type-table index) |
| `SpierParam` | `.name` | `str` |
| | `.type_idx` | `int` |
| `SpierTypeInfo` | `.kind_name` | `str` (`"Slice"`, `"U64"`, `"Enum"`, ...) |
| | `.child_count` | `int` (number of child type indices) |
| | `.children` | `list[int]` (child type-table indices) |
| `SpierEnumSchema` | `.name` | `str` |
| | `.variant_names` | `list[str]` |
| | `.create_enum_class()` | `SpierEnumClass` |
| `SpierEnumClass` | `.VariantName(payload)` | `SpierEnumValue` (factory per variant) |
| `SpierEnumValue` | `.variant` | `str` |
| | `.fields` | `tuple` |
| | supports `==` (by variant name) | |

`h.compress(data)` is sugar for `h.call("compress", data)`. The bound method
holds a reference to the handle, so `f = h.compress; del h; f(data)` is safe.

Finding the `.so`:

| Priority | Mechanism |
|----------|-----------|
| 1 | `lib_dir=` parameter |
| 2 | `DYNSPIRE_LIB_DIR` env var |
| 3 | bare name → `dlopen` resolves via `LD_LIBRARY_PATH` |

## License

See [LICENSE](LICENSE).
