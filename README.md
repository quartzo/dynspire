# DynSpire

A Rust plugin framework for loading native `.so` libraries at runtime — with self-describing IDL schemas, zero-copy FFI, and Python bindings with no code generation.

## Why?

You wrote a Rust library. You want to load it at runtime as a plugin — discover its methods, call them, and get typed results back. Without recompiling. Without stubs. Without a build step.

DynSpire does that.

## In 30 Seconds

Define an interface:

```rust
#[modulo_interface]
pub trait RleEngine {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn compress_into(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), String>;
    fn stats(&self, data: &[u8]) -> Result<(u64, u64), String>;
}
```

Implement it as a `.so` plugin (spier). Load it from Rust:

```rust
let client = DynSpireClient::connect("rle_spier", &rle_idl::IDL, &config)?;

let compressed: Vec<u8> = client.call(RleOp::Compress, (&input[..]))?;
```

Or from Python — with full schema reflection, no codegen:

```python
with load_spier("rle_spier", lib_dir="target/debug").create_handle() as h:
    compressed = h.call("compress", input_data)
```

## Features

- **Self-describing** — spiers export their full IDL schema (methods, types, enums) via a C ABI. Hosts discover everything at runtime.
- **Zero-copy FFI** — borrows (`&[u8]`, `&str`) and mutable out-params (`&mut Vec<u8>`) pass through raw pointers. No serialization overhead. `Vec<T: Clone>` input works for any element type (Rust→Rust).
- **Type-safe dispatch** — Rust hosts use generated Op enums. No magic numbers.
- **IDL hash verification** — incompatible plugins are rejected at load time.
- **Python without codegen** — `ctypes` reads the IDL schema from the `.so` directly. No stub generation, no `bindgen`, no C headers.
- **Any return type** — `Result<T, String>` where `T` can be `()`, `Vec<u8>`, `(u64, u64)`, `Option<String>`, any `#[slot_enum]` type, any `#[slot_struct]` type, or any composed combination.

## The Boundary as Discipline

The IDL + `.so` split isn't just about runtime loading — it's an architectural
constraint that enforces clean separation at compile time.

- **Every dependency is explicit.** The IDL crate defines the interface. The
  spier depends on the IDL, not on the host. The host depends on the IDL, not
  on the spier's internals. No sneaky imports, no shared private modules. If
  it's not in the IDL trait, it doesn't cross the boundary.
- **Interfaces stay focused.** Return types cross as ≤8 `u64` slots. You can't
  return a 50-field struct without consciously choosing `#[slot_struct]`. This
  friction is intentional — it surfaces design problems at the interface, not
  at integration time.
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
  rle-idl/       IDL trait definition
  rle-spier/     cdylib implementation (loaded at runtime)
  rle-host/      Rust host binary
  rle_client.py  Python host (ctypes, schema reflection)
```

```bash
# Build everything
cargo build

# Run Rust host
cargo run -p rle-host

# Run Python host
pip install -e python/
python3 demo/rle_client.py
```

Output:

```
compress()
  -> [04 41 03 42 04 43 05 44 04 45 06 46 03 47] (14 bytes)

decompress()
  -> "AAAABBBCCCCDDDDDEEEEFFFFFFGGG" (29 bytes) [round-trip OK]

compress_into(&mut Vec<u8>)
  caller buffer after : [04 41 03 42 ...] (14 bytes) [matches compress]

stats()
  original  : 29 bytes
  compressed: 14 bytes
  ratio     : 48.3%
```

## Project Layout

```
dynspire/          Core: arena FFI, slot system, tower client
dynspire-macro/    Proc macros: #[modulo_interface], #[spier_dispatch], #[spier_storage], #[slot_enum], #[slot_struct]
dynspire-libs/     Library discovery helpers
python/            ctypes adapter (schema-driven, zero codegen)
demo/              RLE compression showcase
```

## How It Works

```
  Host (Rust binary or Python script)
    │
    │  DynSpireClient::connect("my_spier", &IDL, &config)
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

Arguments and return values flow through **u64 slots** — a compact calling convention that handles scalars, borrows, owned types, tuples, enums, and structs without heap allocation on the FFI boundary. Complex structs cross as opaque boxed pointers (1 slot) via `#[slot_struct]`.

For the deep dive, see [docs/architecture.md](docs/architecture.md).

## Python Bindings

The Python adapter loads any DynSpire `.so` and discovers its full interface at runtime:

```python
from dynspire_ctypes import load_spier

lib = load_spier("rle_spier", lib_dir="target/debug")

# Schema reflection — methods, types, params, all from the .so
for m in lib.schema().methods:
    print(lib.schema().method_sig(m))

# Call with native Python types
with lib.create_handle() as handle:
    compressed = handle.call("compress", b"AAAABBBBCCCC")
```

Finding the `.so`:

| Priority | Mechanism |
|----------|-----------|
| 1 | `lib_dir=` parameter |
| 2 | `DYNSPIRE_LIB_DIR` env var |
| 3 | bare name → `dlopen` resolves via `LD_LIBRARY_PATH` |

## License

See [LICENSE](LICENSE).
