# DynSpire Architecture

This document covers the internals of the DynSpire plugin framework: the slot system, FFI conventions, IDL schema export, proc macros, and the Python ctypes adapter.

## Table of Contents

- [Overview](#overview)
- [Slot System](#slot-system)
- [IDL Schema Export](#idl-schema-export)
- [Proc Macros](#proc-macros)
- [Tower Client](#tower-client)
- [Python ctypes Adapter](#python-ctypes-adapter)
- [Path Resolution](#path-resolution)

---

## Overview

DynSpire is a plugin architecture with three roles:

| Role | Who | What |
|------|-----|------|
| **IDL** | Shared crate | Defines a trait interface. The `#[modulo_interface]` macro generates an Op enum, IDL hash, type table, and method descriptors. |
| **Spier** | `cdylib` crate | Implements the IDL trait. The `#[spier_dispatch]` and `#[spier_storage]` macros generate C-ABI entry points (`dynspire_create`, `dynspire_dispatch_{method}`, `dynspire_destroy`, etc.). |
| **Host** | Binary or script | Loads the spier `.so` via `dlopen`, verifies the IDL hash, and dispatches method calls through slots. |

```
┌─────────────────────────────────────────────┐
│  Host (Rust binary or Python script)        │
│                                             │
│  DynSpireClient / SpierHandle               │
│    .call(Op::Method, args)                  │
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
│  dynspire_idl_schema() → &DynSpireIdl       │
└─────────────────────────────────────────────┘
```

---

## Slot System

The slot system is the core FFI calling convention. Every argument and return value is encoded as a flat array of `u64` values ("slots"). This avoids heap allocation on the FFI boundary and works with any C-compatible caller.

### Direction: Input (Host → Spier)

The host encodes arguments using `SlotEncode`:

```rust
pub trait SlotEncode {
    fn encode(&self, w: &mut SlotWriter);
}
```

`SlotWriter` accumulates `u64` values into an inline array (up to `MAX_IN_SLOTS = 16`), spilling to heap only if exceeded.

### Direction: Output (Spier → Host)

The spier encodes the return value using `SlotReturn`:

```rust
pub trait SlotReturn: Sized {
    fn into_slots(self, w: &mut SlotWriter);
}
```

The host decodes using `SlotReceive`:

```rust
pub trait SlotReceive: Sized {
    unsafe fn from_slots(r: &mut SlotReader) -> Self;
}
```

### Supported Types

| Type | Slots | Encoding |
|------|-------|----------|
| `()`, `bool` | 0–1 | Unit = no slots. Bool = 0 or 1. |
| `u8`..`u64`, `i8`..`i64`, `f64` | 1 | Cast to `u64` (floats via `to_bits`). |
| `[u8; 16]` | 2 | Two `u64` (little-endian halves). |
| `&str`, `&[u8]`, `String`, `Vec<u8>` | 2 | `(ptr, len)` — zero-copy borrow for refs, clone for owned. |
| `&mut Vec<u8>` | 1 | Raw pointer to the `Vec` struct. Spier writes directly into the caller's allocation. |
| `Vec<T: Clone>` (input) | 2 | `(ptr, len)` borrow — spier clones elements from caller's memory. Works for any `T: Clone` including `Vec<String>`, `Vec<Vec<u8>>`, nested Vecs. Rust→Rust only; Python cannot construct Rust memory layouts. |
| `Vec<T>` (output) | 2 | `(ptr, len)` with ownership transfer via `Box::into_raw` / `Box::from_raw`. |
| `Option<T>` | 1 + T | Tag (0=None, 1=Some) followed by T's slots. |
| `(A, B)` | A + B | Concatenation of A's and B's slots. |
| `Result<T, E>` | 1 + T or E | Tag (0=Ok, 1=Err) followed by the variant's slots. |
| Enums (`#[slot_enum]`) | 1 + fields | Discriminant + each field's slots. |
| Structs (`#[slot_struct]`) | 1 | Opaque boxed pointer (`Box::into_raw` / `Box::from_raw`). Rust dereferences directly; Python receives an opaque handle. |

### Key Design Points

- **Borrows are zero-copy**: `&[u8]` passes `(raw_ptr, len)` — the spier reads the host's memory directly. No copy, no allocation.
- **Out-params via `&mut Vec<u8>`**: the host passes a raw pointer to its `Vec`. The spier pushes data into it. Changes are visible to the host immediately. This is the `IDL_OUT_VEC` pattern.
- **Ownership transfer on returns**: `Vec<T>` is moved across the boundary via `Box::into_raw` / `forget` on the spier side, `Box::from_raw` on the host side. No copy.
- **`Vec<T: Clone>` input**: the caller sends `(ptr, len)` pointing to its own heap memory. The spier clones elements via `slice::from_raw_parts(ptr, len).to_vec()`. Elements are never individually slot-encoded — always 2 slots regardless of count or element complexity. Rust→Rust only; Python callers serialize complex Vecs to `&[u8]`.
- **`Result<T, String>` is universal**: every dispatch method returns `Result<T, String>`. The tag slot (0/1) distinguishes Ok from Err. The host's `call()` returns `Result<R, String>` — the outer layer is transport errors, the inner is the spier's application error.

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
    pub kind: u8,      // IDL_U8, IDL_SLICE, IDL_VEC, IDL_TUPLE, ...
    pub size: u32,     // Array length (for IDL_ARRAY)
    pub child0: i32,   // Index into type_table, or -1
    pub child1: i32,   // Index into type_table, or -1
}
```

Composite types are built recursively: `Vec<u8>` is `IDL_VEC` with `child0` pointing to `IDL_U8`. `Option<(String, u64)>` is `IDL_OPTION` with `child0` pointing to `IDL_TUPLE(String, U64)`.

---

## Proc Macros

### `#[modulo_interface]`

Applied to a trait. Generates:

| Output | Description |
|--------|-------------|
| `{PREFIX}_IDL_HASH: u64` | FNV-1a hash of the canonical method signatures |
| `pub static IDL: IdlDescriptor` | Bundle of hash + method names for `connect()` |
| `{Prefix}Op` enum | `#[repr(u8)]` enum with one variant per method |
| `impl SpierOp for {Prefix}Op` | Enables type-safe `call(Op::Method, ...)` |
| `pub mod tower` | `METHODS`, `IDL_TYPE_TABLE`, `IDL_METHODS`, `IDL_SCHEMA` |
| `idl_schema()` | Returns `&'static DynSpireIdl` |

The hash is computed from the canonical signature string: `TraitName{method(params)->ret,...}`. This ensures that any change to method names, parameter types, or return types produces a different hash, preventing version mismatches.

### `#[spier_storage]`

Applied to an `init` function. Generates:

- `dynspire_create(data_ptr, data_len) -> *mut c_void` — deserializes config (URL-encoded kvmap), calls the init function, returns `Box::into_raw(Box::new(state))`.
- `dynspire_destroy(handle)` — `drop(Box::from_raw(handle))`.

### `#[spier_dispatch(name = "...", idl = ...)]`

Applied to `impl Trait for State`. For each method, generates:

```c
// Signature:
u8 dynspire_dispatch_{method}(
    void *state,
    const u64 *in_slots, usize in_count,
    u64 *out_slots, usize out_capacity
);
```

The function:
1. Decodes arguments from `in_slots` using `SlotDecode`
2. Calls the trait method
3. Encodes the `Result<T, String>` return into `out_slots` using `write_to_ffi`
4. Returns 0 on success, 2 if out buffer too small

Also generates `dynspire_idl_hash()`, `dynspire_spier_name()`, and `dynspire_idl_schema()`.

### `#[slot_enum]`

Applied to an enum. Generates `SlotEncode`, `SlotDecode`, `SlotReturn`, `SlotReceive` impls plus a static `EnumDescriptor` for schema reflection. Each variant is encoded as `(discriminant, field0_slots, field1_slots, ...)`.

### `#[slot_struct]`

Applied to a struct. Generates `SlotEncode`, `SlotDecode`, `SlotReturn`, `SlotReceive` impls using an opaque boxed pointer (1 slot). The struct crosses the FFI boundary as `Box::into_raw` on the sender side and `Box::from_raw` on the receiver side. Rust callers access fields natively; Python callers receive an opaque integer handle and use explicit IDL methods for field access. Requires `Clone`.

---

## Tower Client

The tower client (`DynSpireClient`) is the Rust host-side API:

```rust
// One-line setup
let client = DynSpireClient::connect(
    "rle_spier",       // spier name → finds .so
    &rle_idl::IDL,     // IDL descriptor (hash + methods)
    &config,           // creation config
)?;

// Type-safe dispatch
let result: Vec<u8> = client.call(RleOp::Compress, (&data[..]))?;
```

`connect()` performs:
1. `DynSpireLib::find(name)` — resolves `.so` path
2. `libloading::Library::new(path)` — `dlopen`
3. Hash verification — calls `dynspire_idl_hash()`, compares with expected
4. Symbol resolution — loads `dynspire_dispatch_{method}` for each method
5. Instance creation — calls `dynspire_create(config)`

The `SpierOp` trait ensures `call()` only accepts the generated Op enum — raw integers are a compile-time error.

---

## Python ctypes Adapter

The Python adapter (`python/dynspire_ctypes.py`) provides the same capabilities as the Rust tower client, entirely through `ctypes`:

### Schema Reflection

```python
lib = load_spier("rle_spier", lib_dir="target/debug")
schema = lib.schema()

for m in schema.methods:
    print(schema.method_sig(m))
    # compress(data: Slice<U8>) -> Result<Vec<U8>, String>
```

The adapter reads the `DynSpireIdl` C struct directly via `ctypes.from_address`, walking the type table and method table to build Pythonic dataclasses (`TypeInfo`, `MethodInfo`, `EnumSchema`).

### Calling Convention

```python
with lib.create_handle() as handle:
    # Positional args (recommended)
    compressed = handle.call("compress", input_data)

    # Named args (explicit)
    compressed = handle.call("compress", {"data": input_data})
```

The `SpierHandle` holds the schema internally. Positional args are bound by parameter order (skipping `OutVec` params). The adapter handles:

- **Borrows** (`&[u8]`, `&str`): allocates ctypes byte arrays, keeps them alive during the call
- **Owned returns** (`Vec<u8>`, `String`, `#[slot_struct]`): reads `(ptr, len)` or opaque handle from slots, wraps in `FFIResource` for lifecycle management (see below)
- **OutVec** (`&mut Vec<u8>`): creates a Rust `Vec` via `dynspire_vec_create()`, passes pointer, reads back via `dynspire_vec_view()`, frees via `dynspire_vec_free()`
- **Result<T, String>**: reads tag slot, decodes Ok value or raises `RuntimeError` with the error string

### Lifecycle

```python
with lib.create_handle() as handle:
    handle.call("compress", data)
    # destroy() called automatically on __exit__
```

`SpierHandle.__del__` is a safety net — calls `destroy()` if the `with` block wasn't used. `destroy()` is idempotent (checks for null handle).

### Return Value Lifecycle (`FFIResource`)

Non-scalar return values from the spier carry heap allocations that must be released. All non-scalar returns are wrapped in `FFIResource`, which provides **lazy access** to the Rust heap memory — data is never copied until the user explicitly accesses it.

**Lazy access by type:**

- **`String` / `Vec<u8>`**: `len()` reads the length from slot metadata (no copy). Indexing and iteration read individual bytes directly from the Rust pointer. `str()` / `bytes()` copy the full buffer on demand.
- **`#[slot_struct]`**: the opaque handle is exposed via `int()` directly from the slot (no decode). Can be passed to subsequent calls — `encode_slot` unwraps automatically.
- **Other types** (`Vec<String>`, tuples, enums): decoded on first `.value` access and cached.

**Mechanism:** Each spier exports a single unified `dynspire_free(type_index, slots, count)` function. It reconstructs the value via `SlotReceive::from_slots` and drops it — the Rust `Drop` cascade frees all nested heap allocations. Called on `close()` or garbage collection (`__del__`).

**Error path:** When a spier returns `Err(String)`, the adapter calls `dynspire_free` to release the error string's heap allocation before raising `RuntimeError`.

**Transparency:** `FFIResource` proxies dunder methods (`__len__`, `__iter__`, `__eq__`, `__str__`, `__format__`, `__getattr__`, etc.) with lazy semantics. Scalar returns (`u32`, `bool`, etc.) are returned as native Python values with no wrapping.

---

## Path Resolution

Both Rust and Python follow the same three-tier resolution:

| Priority | Rust | Python |
|----------|------|--------|
| 1 | Explicit path in `DynSpireLib::load(path)` | `lib_dir=` parameter |
| 2 | `DYNSPIRE_LIB_DIR` env var → constructs full path | `DYNSPIRE_LIB_DIR` env var → constructs full path |
| 3 | Bare filename → `dlopen` resolves via `LD_LIBRARY_PATH` | Bare filename → `ctypes.CDLL` resolves via `dlopen` |

Levels 1 and 2 construct a full filesystem path before calling `dlopen` — no side effects on the system's dynamic linker. Level 3 delegates entirely to `dlopen`, which searches `LD_LIBRARY_PATH`, `/usr/lib`, `/usr/local/lib`, etc.

This means spiers installed in standard library paths work out of the box, while project-specific spiers can be isolated via `DYNSPIRE_LIB_DIR` without polluting the global library search path.
