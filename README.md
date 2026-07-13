# DynSpire

A Rust plugin framework for loading native `.so` libraries at runtime — with codegen-generated bindings, zero-copy FFI, and typed Python clients.

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

`build.rs` generates the trait, types, Op enum, and spier dispatch macro (spier side) or IDL descriptor and tower client (host side). Implement the trait and load it:

```rust
// Spier crate
impl RleEngine for RleState {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String> { /* ... */ }
    fn analyze(&self, data: &[u8]) -> Result<CompressionReport, String> { /* ... */ }
    // ...
}
impl_rle_spier!(RleState, init, "rle");
```

```rust
// Host crate
let client = DynSpireRle::connect("rle_spier", &config, false)?;
let compressed: Vec<u8> = client.compress(&input[..])?;
let report = client.analyze(&input[..])?;  // typed CompressionReport
```

Or from Python — with a code-generated typed client (emitted at spier build time):

```python
from rle import Rle
with Rle("target/debug/librle_spier.so") as c:
    compressed = c.compress(input_data)
    report = c.analyze(input_data)   # CompressionReport (opaque handle)
```

## Features

- **DSL-driven** — a `.dspi` file is the single source of truth. `build.rs` generates trait, types, Op enum, and spier dispatch macro (spier side) or IDL descriptor and tower client (host side). No proc macros on business code.
- **Zero-copy FFI** — borrows (`&[u8]`, `&str`) and mutable out-params (`&mut Vec<u8>`) pass through raw pointers. No serialization overhead. `Vec<T: Clone>` input works for any element type (Rust→Rust).
- **Type-safe dispatch** — Rust hosts use the generated tower wrapper. No magic numbers, no manual slot encoding.
- **IDL hash verification** — incompatible plugins are rejected at load time.
- **Python without a Rust toolchain** — the spier's `build.rs` emits a pure-Python ctypes client (`.py`) via `generate_python()`. Python users consume the generated module directly — no PyO3, no maturin, no compilation step.
- **Any return type** — `Result<T, String>` where `T` can be `()`, `Vec<u8>`, `(u64, u64, u64)`, `Option<String>`, any DSL-declared enum or struct, or any composed combination.

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

The `.dspi` file lives in the spier crate. The host compiles the same file by path reference. Each side uses a different `build.rs` entry point — `build_spier()` for the spier, `build_host()` for the host. The generated IDL hash guarantees compatibility at load time.

```
my-spier/                 my-host/
  Cargo.toml                Cargo.toml
  build.rs                  build.rs
  src/
    my.dspi                 src/
    lib.rs                    main.rs
```

**Spier crate** (`Cargo.toml` deps: `dynspire-codegen` as build-dep, `dynspire`):

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_spier("src/my.dspi");
}
```
```rust
// lib.rs — include the generated spier code
#![allow(non_upper_case_globals)]
include!(concat!(env!("OUT_DIR"), "/my_spier.rs"));

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

**Host crate** (same `.dspi`, referenced by path):

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_host("../my-spier/src/my.dspi");
}
```
```rust
// main.rs — include the generated host code
#![allow(non_upper_case_globals)]
include!(concat!(env!("OUT_DIR"), "/my_host.rs"));

let client = DynSpireMy::connect("my_spier", &config, false)?;
let result = client.do_thing(&input[..])?;
```

The IDL hash is computed from the interface's canonical signature — both sides produce the same hash from the same `.dspi`, so `connect()` accepts the spier.

### Multiple interfaces with shared types

When a host needs to talk to multiple spiers that share type fragments, use `BuildContext` to deduplicate type definitions:

```rust
// build.rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_spier("src/a.dspi");   // generates SharedHandle
    ctx.build_spier("src/b.dspi");   // skips SharedHandle (already emitted, same content)
}
```

Types with the same name but different content are a hard error at codegen time.

### Naming conventions

Symbol names are derived from the interface name in the `.dspi` file:

| Interface name | Generated symbol | Example (`interface My`) |
|---|---|---|
| `interface {N}` | `pub trait {N}Engine` | `MyEngine` |
| | `pub enum {N}Op` | `MyOp` |
| | `pub struct DynSpire{N}` | `DynSpireMy` |
| | `pub const {N_UPPER}_IDL_HASH: u64` | `MY_IDL_HASH` |
| | `macro_rules! impl_{n_lower}_spier!` | `impl_my_spier!` |
| | output file (spier): `{n_lower}_spier.rs` | `my_spier.rs` |
| | output file (host): `{n_lower}_host.rs` | `my_host.rs` |

### Python host

Python uses a typed client generated by the spier's `build.rs`:

```rust
// spier build.rs — also emits a .py alongside the .rs
fn main() {
    let mut ctx = dynspire_codegen::BuildContext::new();
    ctx.build_spier("src/my.dspi");
    ctx.build_python("src/my.dspi", "generated/my.py");
}
```

```python
from my import My  # import the generated module
with My("target/debug/libmy_spier.so") as c:
    result = c.do_thing(b"input")
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

An RLE compression spier showcases the full cycle:

```
demo/
  rle-spier/     .dspi interface + build.rs (generates trait, types, spier macro)
  rle-host/      build.rs compiles same .dspi (generates trait, types, tower)
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
pyproject.toml     uv project root
dynspire/          Core: arena FFI, slot system, tower client
dynspire-codegen/  DSL parser + code generator (.dspi → .rs)
demo/              RLE compression showcase
  rle-spier/         .dspi + build.rs (generates spier code) + cdylib implementation
  rle-host/          build.rs compiles same .dspi (generates host code) + binary
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
    dynspire_free()     → release owned returns (String, Vec, opaque handles)
    dynspire_destroy()  → free State
```

Arguments and return values flow through **u64 slots** — a compact calling convention that handles scalars, borrows, owned types, tuples, enums, and structs without heap allocation on the FFI boundary. Complex structs cross as opaque boxed pointers (1 slot) via the DSL's `opaque struct` declaration.

For the deep dive, see [docs/architecture.md](docs/architecture.md).

## Python Bindings

The spier's `build.rs` emits a pure-Python ctypes client (`.py`) alongside
the Rust code. Python users import the generated module directly — no PyO3,
no maturin, no Rust toolchain required on the consuming side.

The generated module is self-contained — it inlines the ctypes primitives
(`SlotWriter`, `SpierClient`, `OpaqueHandle`, etc.) and provides a typed
class per interface:

```python
from rle import Rle, Tone, CompressionReport, NamedRun

with Rle("target/debug/librle_spier.so") as c:
    compressed = c.compress(b"AAAABBBBCCCC")
    decompressed = c.decompress(compressed)

    # Out-vec methods (&mut Vec<u8>) return (ret_val, list[bytes])
    ok, outs = c.compress_into_checked(b"AAAABBBBCCCC")

    # Enums are typed Python objects
    tone = c.classify(b"AAAABBBBCCCC")     # Tone.Loud(71)
    desc = c.describe_tone(Tone.Quiet())    # "silence"

    # Structs with fields get full Python classes (not opaque handles)
    report = c.analyze(b"AAAABBBBCCCC")     # CompressionReport
    print(report.original_size)             # @property accessor
    summary = c.report_summary(report)      # pass back to spier

    # Construct structs from Python
    run = NamedRun(label="A", count=42)
    print(run.label, run.count)             # "A" 42
    repr(run)                               # "NamedRun(label='A', count=42)"
    assert run == NamedRun(label="A", count=42)  # structural equality
```

The generated module inlines all runtime primitives: `SlotWriter`
(encodes args into `u64[]`), `SpierClient` (loads `.so`, verifies IDL
hash, dispatches), `OpaqueHandle` (GC-managed boxed pointer), and
out-vec helpers. No external package dependency — only `ctypes` from
the Python stdlib. See [docs/architecture.md](docs/architecture.md)
for the full struct codegen specification.

## License

See [LICENSE](LICENSE).
