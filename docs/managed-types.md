# DynSpire Managed Types — Design Proposal

This document specifies the managed type system and allocator proposed for
DynSpire, enabling spiers written in languages other than Rust (Go, C, Python)
while preserving the zero-copy borrow fast-path that exists today.

## Table of Contents

- [Motivation](#motivation)
- [Design Principles](#design-principles)
- [Memory Regimes](#memory-regimes)
- [DynSpire Types](#dynspire-types)
- [Allocator](#allocator)
- [Reference Counting](#reference-counting)
- [Drop Glue](#drop-glue)
- [Allocator Lifecycle](#allocator-lifecycle)
- [C-ABI Evolution](#c-abi-evolution)
- [Cross-Language Projection](#cross-language-projection)
- [IDL DType Support (Implemented)](#idl-dtype-support-implemented)
- [Codegen Changes](#codegen-changes)
- [Implementation Phases](#implementation-phases)

---

## Motivation

DynSpire's current architecture assumes a single shared heap between host and
spier — both Rust, both using the system allocator. This makes `Box::into_raw`
/ `Box::from_raw`, raw-pointer borrows, and `&mut Vec<u8>` out-params sound by
construction. The IDL syntax mirrors Rust types, and the codegen emits Rust
trait signatures using `String`, `Vec<T>`, `&str`, `&[u8]` directly.

The goal is to enable spiers in languages with their own runtime and allocator
(Go with GC, Python with ctypes) without sacrificing the in-process zero-copy
properties that make DynSpire efficient. The key insight: the IDL should define
**its own type system with Rust semantics** — ownership, borrowing, drop — but
with a **C-stable wire representation** controlled by DynSpire, not by any
language's native layout.

The `dynspire-py` PyO3 adapter was removed in favor of inlined-ctypes codegen
because the performance was comparable and the complexity was lower. This
proposal follows the same principle: no per-language runtime package, just
codegen that emits thin FFI wrappers calling the DynSpire C-ABI directly.

---

## Design Principles

1. **The `.dspi` is the contract.** Types declared in the IDL have C-stable
   layout by definition. If a type crosses the boundary, it is projectable by
   any language with C FFI.

2. **Rust syntax, DynSpire semantics.** The IDL uses Rust-like syntax (`String`,
   `Vec<T>`, `&str`, `&mut`) but the materialized types are DynSpire types
   (`DString`, `DVec<T>`, `DStr`) with `repr(C)` layout. The codegen translates
   automatically.

3. **Borrows are fast-path, universal.** `&T` (immutable, call-scoped) works
   for any type including composite structs — raw pointer + C-stable layout,
   zero-copy, no allocator, no RC. Any language projects via
   `unsafe.Pointer` / `ctypes` / raw pointer arithmetic.

 4. **Allocator is host-controlled.** The host provides the allocator strategy;
    DynSpire defines only the interface (vtable). The spier is forced to use the
    host's allocator for all dynamic allocations — the host controls memory.
    The allocator is **configured once at spier creation** (`dynspire_create`),
    not per call.

5. **Reference counting for lifecycle.** Every allocation carries an inline RC
   header. `retain`/`release` manage lifetime. When RC reaches zero, the
   type-specific drop function runs, then the allocator reclaims the block. No
   `dynspire_free` with `type_index` dispatch — the drop function is stored in
   the header at allocation time.

6. **Allocator pointer distributed in dynamic types.** Each dynamic type
   (`DString`, `DVec<T>`) carries its own allocator pointer as the first field.
   This makes every dynamic element self-contained — extractable, passable
   individually, operable without external context.

7. **No per-language runtime package.** Each target gets codegen that emits
   types and thin FFI wrappers inline, calling the DynSpire C-ABI directly.

---

## Memory Regimes

Three regimes govern how data crosses the boundary:

### 1. Immutable Borrow (`&T`)

Call-scoped, read-only, zero-copy. The caller passes a raw pointer; the spier
reads during the call and never escapes with the pointer.

Works for **any type** with C-stable layout — primitives, composite structs,
even structs containing dynamic fields (`&Report` where `Report { name:
DString, ... }`). The spier reads `report.name.ptr` and `report.name.len` to
access the string data directly in the caller's memory.

No allocator involvement. No allocation. No RC.

```
Host: pin buffer during call → pass raw ptr → spier reads → unpin after call
```

### 2. Mutable Borrow, Fixed-Size (`&mut Point`)

The type contains only fixed-size fields (primitives, arrays, nested
fixed-size structs). The caller passes a raw pointer; the spier writes fields
directly. No allocation can occur, so no allocator is needed.

```
Host: allocate Point on stack/heap → pass raw ptr → spier mutates fields in place
```

### 3. Mutable Borrow, Dynamic (`&mut DString`, `&mut DVec<T>`, `&mut Report`)

The type contains dynamic fields that may need reallocation. The type carries
an allocator pointer (first field). The spier reads the allocator pointer from
the type itself and calls `dynspire_realloc` to grow.

```
Host: allocate DString via dynspire_alloc → pass raw ptr to DString
        → spier reads dstring.allocator → calls dynspire_realloc → writes to buffer
```

The allocator pointer is self-contained in the type — the spier needs no
external context to operate.

---

## DynSpire Types

All DynSpire types are `#[repr(C)]` with a stable, documented layout. The
codegen translates IDL types to DTypes when they appear inside struct fields or
when they need C-stable layout for cross-language projection.

### View types (immutable, no allocator)

```rust
#[repr(C)]
pub struct DStr {       // = &str semantics
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct DSlice<T: ReprC> {   // = &[T] semantics
    pub ptr: *const T,
    pub len: usize,
}
```

Views are used in two contexts:
- As function parameters (`fn compress(data: &[u8])` — already works today).
- As fields inside borrowed structs (`&Report` where `Report` contains `DStr`
  fields — the view aliases the caller's memory, read-only).

Views never carry an allocator pointer — they are non-owning by definition.

### Dynamic types (owned, with allocator pointer)

```rust
#[repr(C)]
pub struct DString {    // = String semantics
    pub allocator: *mut DynSpireAllocator,   // first field — self-contained
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

#[repr(C)]
pub struct DVec<T: ReprC> {    // = Vec<T> semantics
    pub allocator: *mut DynSpireAllocator,   // first field — self-contained
    pub ptr: *mut T,
    pub len: usize,
    pub cap: usize,
}
```

The allocator pointer is the first field, always at offset 0. Any language can
read it via `*(ptr_to_dtype)` to obtain the allocator for reallocation or
release.

### Composite structs

```rust
#[repr(C)]
pub struct Report {
    pub name: DString,          // has its own allocator pointer (self-contained)
    pub tags: DVec<DString>,    // has its own allocator pointer (self-contained)
    pub ratio: f64,
    pub _pad: [u8; 8],
}

#[repr(C)]
pub struct Point {             // fixed-size — no allocator pointer
    pub x: f64,
    pub y: f64,
}
```

**Rule:** a struct has an allocator pointer if and only if it is itself dynamic
(allocated on the allocator for ownership transfer). Composite structs that
are fixed-size shells containing dynamic fields do **not** have a top-level
allocator pointer — the dynamic fields inside them carry their own.

When a composite struct is heap-allocated for ownership transfer (return
value), it is allocated via `state.allocator` (configured at `dynspire_create`).
The type-specific drop function (codegen-emitted, stored in the RC header) knows
how to walk the struct and release each dynamic field's buffer via its embedded
allocator pointer, then release the struct shell itself.

### Enums

```rust
#[repr(C, u8)]
pub struct DOption<T: ReprC> {   // = Option<T> semantics
    pub tag: u8,                 // 0 = None, 1 = Some
    pub _pad: [u8; 7],
    pub value: T,               // valid when tag == 1; uninitialized otherwise
}
```

DSL-declared enums get a `#[repr(C, u32)]` tag followed by a union of variant
payloads. The codegen emits the tag + union layout explicitly.

### `ReprC` trait

A marker trait indicating a type has a stable C layout:

```rust
pub unsafe trait ReprC: Copy {}
```

Implemented for all primitives, `[T; N]`, and all DynSpire types. Generic
containers (`DVec<T>`) require `T: ReprC`.

---

## Allocator

The allocator is the sole allocation interface for dynamic types. It replaces
the current `Box::into_raw` / `Box::from_raw` / `dynspire_free` pattern.

### Design

The allocator is a **vtable-based interface** — DynSpire defines the contract;
the host provides the implementation. DynSpire does not impose any allocation
strategy (no free-list, no slab, no bump allocator). The host controls how
memory is obtained and returned.

```rust
#[repr(C)]
pub struct DynSpireAllocatorVtable {
    alloc: extern "C" fn(ctx: *mut c_void, size: usize, align: usize) -> *mut u8,
    dealloc: extern "C" fn(ctx: *mut c_void, ptr: *mut u8, size: usize, align: usize),
    realloc: extern "C" fn(
        ctx: *mut c_void,
        ptr: *mut u8,
        old_size: usize,
        new_size: usize,
        align: usize,
    ) -> *mut u8,
    drop_allocator: extern "C" fn(ctx: *mut c_void),
}

#[repr(C)]
pub struct DynSpireAllocator {
    vtable: *const DynSpireAllocatorVtable,
    ctx: *mut c_void,   // opaque host state
}
```

The host constructs the allocator with its own implementation and passes it to
`dynspire_create`. The spier stores the pointer in its `State` and reuses it for
every dispatch call. The spier only sees the vtable — never accesses internals.

### C-ABI

```rust
#[no_mangle]
pub extern "C" fn dynspire_alloc(
    alloc: *mut DynSpireAllocator,
    size: usize,
    align: usize,
) -> *mut u8;

#[no_mangle]
pub extern "C" fn dynspire_dealloc(
    alloc: *mut DynSpireAllocator,
    ptr: *mut u8,
    size: usize,
    align: usize,
);

#[no_mangle]
pub extern "C" fn dynspire_realloc(
    alloc: *mut DynSpireAllocator,
    ptr: *mut u8,
    old_size: usize,
    new_size: usize,
    align: usize,
) -> *mut u8;

#[no_mangle]
pub extern "C" fn dynspire_retain(ptr: *mut u8);

#[no_mangle]
pub extern "C" fn dynspire_release(ptr: *mut u8);
```

`dynspire_alloc` allocates a block and writes the RC header at the start.
`dynspire_retain` increments the RC. `dynspire_release` decrements the RC; if
it reaches zero, calls the stored `drop_fn`, then calls `dealloc` on the
allocator stored in the header.

### Default allocator

DynSpire provides a default allocator backed by `std::alloc` (Rust system
allocator):

```rust
pub fn default_allocator() -> DynSpireAllocator {
    DynSpireAllocator { vtable: &DEFAULT_VTABLE, ctx: ptr::null_mut() }
}
```

Hosts that don't want to customize use this default. Advanced hosts can inject
their own (mimalloc, jemalloc, custom arena/slab, etc.) without changing the
ABI.

### Two roles of the allocator

The allocator serves two distinct purposes, both necessary:

1. **Allocator inside DTypes** (`DString.allocator`, `DVec.allocator`): allows
   the spier to **grow** a mutable borrow. The spier reads the allocator
   pointer from the type itself and calls `dynspire_realloc`.

2. **Allocator as spier configuration**: allows the spier to **construct**
   return values. The host passes an allocator **once** to `dynspire_create`;
   the spier stores it in `State` and reads `state.allocator` on every dispatch
   to allocate returns. The host owns the allocator and controls its lifetime.
   It is **not** passed per dispatch call.

### Cross-spier data

If the host passes a `DString` created by spier A to spier B, spier B sees
`allocator` pointing to the host's allocator (or whichever allocator was used).
Since DynSpire is in-process (shared address space), spier B can call
`dynspire_realloc` on that allocator pointer — it is a valid
`*mut DynSpireAllocator` regardless of which `.so` created it, because the
allocator functions live in the `dynspire` runtime crate, which is linked into
every spier.

---

## Reference Counting

Every allocation carries an inline RC header before the payload. This is
DynSpire protocol — not a host decision.

### Header layout

```rust
#[repr(C)]
struct DynSpireHeader {           // placed before the payload
    rc: AtomicUsize,              // lifecycle — atomic, thread-safe
    type_index: u32,              // drop dispatch (codegen-time constant)
    drop_fn: Option<extern "C" fn(*mut c_void)>,  // type-specific cleanup
    allocator: *mut DynSpireAllocator,   // for release to know who to call
    size: usize,
    align: usize,
}
```

Memory layout of an allocated `DString`:

```
[DynSpireHeader | DStringFields { allocator, ptr, len, cap }]
```

The `allocator` appears both in the header (for release to call dealloc) and
in the `DString` payload (for the spier to read during realloc). This is
intentional — the header's copy is for lifecycle management; the payload's copy
is for operational use by the spier.

### `retain` / `release`

```rust
pub extern "C" fn dynspire_retain(ptr: *mut u8) {
    let header = (ptr as *mut DynSpireHeader).sub(1);  // header before payload
    (*header).rc.fetch_add(1, Ordering::Release);
}

pub extern "C" fn dynspire_release(ptr: *mut u8) {
    let header = (ptr as *mut DynSpireHeader).sub(1);
    if (*header).rc.fetch_sub(1, Ordering::AcqRel) == 1 {
        // RC reached zero — run drop glue, then dealloc
        if let Some(drop_fn) = (*header).drop_fn {
            drop_fn(ptr);  // type-specific recursive release
        }
        let alloc = (*header).allocator;
        let size = (*header).size;
        let align = (*header).align;
        dynspire_dealloc(alloc, header as *mut u8, size, align);
    }
}
```

### Thread safety

`rc` is `AtomicUsize` with `AcqRel` ordering — safe for concurrent
`retain`/`release` across threads. This matches the `Send + Sync` bound on the
spier trait.

### Initial RC

`dynspire_alloc` sets `rc = 1`. The first `release` after construction (with no
intervening `retain`) will drop the value. The host is responsible for calling
`release` when done with a returned value.

### Cross-language GC integration

- **Rust host**: `Drop` implementation on the wrapper calls `dynspire_release`.
- **Go spier/host**: Go finalizer (`runtime.SetFinalizer`) calls
  `dynspire_release` via cgo.
- **Python host**: `OpaqueHandle.__del__` calls `dynspire_release` via ctypes.

All follow the JNI/V8/PyO3 pattern: foreign GC triggers `release` when the
wrapper is collected.

---

## Drop Glue

When `dynspire_release` brings RC to zero, the stored `drop_fn` runs before
dealloc. This handles recursive release of owned sub-fields.

### How it works

The codegen emits one `drop_fn` per type that has dynamic fields. At allocation
time, the dispatch layer writes the function pointer into the header's
`drop_fn` field.

```rust
// Codegen-emitted drop function for Report
extern "C" fn drop_report(ptr: *mut c_void) {
    let report = unsafe { &mut *(ptr as *mut Report) };
    // Release each dynamic field via its own allocator + RC
    dynspire_release(report.name.ptr as *mut u8);
    dynspire_release(report.tags.ptr as *mut u8);
}
```

### Recursion

`drop_fn` for a composite type calls `dynspire_release` on each dynamic field.
Each release decrements that field's RC; if it reaches zero, that field's own
`drop_fn` runs. The recursion terminates at leaf types (`DString`, `DVec<T>`
with `T: Copy`) whose `drop_fn` is `None` — plain dealloc.

### Opaque structs

Opaque structs (`opaque struct Handle;`) have unknown internal layout — the
codegen does not emit field definitions for them. The spier that creates an
opaque value provides its own `drop_fn` at allocation time. When the host
`release`s the opaque, the spier's `drop_fn` runs, executing whatever cleanup
the spier needs (closing handles, releasing locks, etc.).

### Foreign resources

A Go spier that pins a Go buffer and passes it as a `DStr` can register a
`drop_fn` that calls back into Go via cgo to unpin the buffer. The function
pointer is C-ABI (`extern "C" fn(*mut c_void)`), the data pointer is opaque —
any language can participate.

---

## Allocator Lifecycle

### Default: allocator created once at spier creation

The host (`DynSpireClient::connect` or the generated `DynSpire{Name}` wrapper)
creates a default allocator, passes it to `dynspire_create`, and **holds it
alive for the spier's whole lifetime**. Every dispatch call reuses the same
allocator via `state.allocator`:

```rust
// Host side (DynSpireClient::connect / generated wrapper)
let allocator = default_allocator();
let state = dynspire_create(&allocator as *mut _, config_ptr, config_len);
// allocator is held by the client for the spier's lifetime
// dispatch calls read state.allocator — no per-call allocator
```

The user does not see the allocator. Values are released immediately after
consumption. The host is responsible for keeping the allocator alive until
after `dynspire_destroy` and all `release` calls complete.

### Debug allocator & memory reporting

For debugging spier memory problems, opt into the **debug allocator** instead
of the zero-overhead default. It tracks live/peak/total occupation in
process-lifetime counters:

```rust
// Rust host: pass `debug = true` to opt into the tracking allocator.
let client = DynSpireRle::connect("rle_spier", &config, /* debug = */ true)?;
let report = client.allocator_report();
println!("live bytes: {}, peak bytes: {}", report.live_bytes, report.peak_bytes);
```

```python
# Python host
with Rle(lib_path, debug=True) as c:
    c.compress(data)
    rep = c.allocator_report()   # DynSpireAllocatorReport(live_bytes=..., ...)
```

The report is a `#[repr(C)]` `DynSpireAllocatorReport { live_bytes,
live_allocations, peak_bytes, total_allocations }`, available through:

- C-ABI: `dynspire_allocator_report(alloc)` (and `dynspire_debug_allocator()`
  to obtain the debug allocator's pointer).
- Rust: `DynSpireAllocator::report()`, `DynSpireClient::allocator_report()`,
  and the generated `DynSpire{Name}::allocator_report()`.
- Python: `SpierClient.allocator_report()`.

The default allocator keeps a null `ctx` and a `report` that returns all
zeros, so choosing the debug allocator is purely opt-in and has no overhead
otherwise.

### Explicit: host-managed allocator

When the host wants to reuse allocations or hold returns across multiple calls:

```rust
let allocator = MyAllocator::new();
let client = DynSpireClient::connect_with_allocator("my_spier", &IDL, &config, &allocator)?;
let report = client.analyze(&data)?;
let summary = client.report_summary(&report)?;
// both `report` and `summary` retained (RC > 0)
// released when the host calls release or drops the wrappers
```

The host controls the allocator's lifetime. Retained values stay alive until
`release`d, regardless of when the allocator itself is dropped.

### Allocator lifetime vs value lifetime

The allocator can be dropped while values allocated through it are still alive
(RC > 0), **provided** the values have already been released (RC = 0) before
the allocator is dropped. If the allocator is dropped first, releasing a value
would call `dealloc` on a dangling pointer. The host wrapper ensures values
are released before the allocator is dropped.

For retained values (opaques held by the host), the allocator must outlive them.
The host is responsible for this ordering — either keep the allocator alive
for the process lifetime, or release all retained values before dropping it.

---

## C-ABI Evolution

### Current dispatch signature

```rust
type FnDispatch = unsafe extern "C" fn(
    *mut c_void,     // state handle
    *const u64,       // in_slots
    usize,            // in_count
    *mut u64,         // out_slots
    usize,            // out_capacity
) -> u8;
```

`FnDispatch` is **unchanged** — the allocator is not passed per call. Instead it
is configured once at spier creation (see `FnCreate` below) and read from
`state.allocator` inside each dispatch.

### Changed create signature

```rust
type FnCreate = unsafe extern "C" fn(
    *mut DynSpireAllocator,    // allocator — spier configuration
    *const u8,                 // config KV bytes
    usize,                     // config length
) -> *mut c_void;
```

The allocator is the **first** parameter of `dynspire_create`. The spier stores
it in `State` and reuses it for all dispatch calls. This is a **breaking ABI
change** — all spiers and hosts must be recompiled. The project is pre-1.0 with
only the RLE demo as existing code, so this is acceptable.

### Removed functions

- `dynspire_free` (replaced by `dynspire_release` with inline `drop_fn`)
- `dynspire_vec_create`, `dynspire_vec_view`, `dynspire_vec_free` (replaced by
  `DVec<u8>` with allocator + RC)

### Added functions

- `dynspire_alloc`
- `dynspire_dealloc`
- `dynspire_realloc`
- `dynspire_retain`
- `dynspire_release`

### Unchanged functions

- `dynspire_destroy` — spier state destruction
- `dynspire_idl_hash` — IDL compatibility check

### Changed functions

- `dynspire_create` — gains `*mut DynSpireAllocator` as its first parameter
  (spier configuration). The spier stores it in `State`.

---

## Cross-Language Projection

Every target language projects DynSpire types via its own C FFI:

### Rust (host + spier)

- **Trait signatures** continue using Rust stdlib types (`String`, `Vec<T>`,
  `&str`, `&[u8]`). Ergonomic for the spier implementor.
- **Dispatch layer** (generated) converts between stdlib types and DTypes:
  `String` → `DString` (move, no copy), `Vec<T>` → `DVec<T>` (move), `&str` →
  `DStr` (zero-copy view), `&[u8]` → `DSlice<u8>` (zero-copy view).
- **Generated code** uses DTypes internally for clarity — the wire format is
  visible in the generated source.
- **Drop implementations** on wrapper types call `dynspire_release`.

### Go (spier)

- Codegen emits Go `struct` types mirroring the C layout:
  ```go
  type DString struct {
      Allocator uintptr
      Ptr       *byte
      Len       int
      Cap       int
  }
  ```
- cgo wrappers call `dynspire_alloc`/`realloc`/`release` directly.
- `&DString` passed to a spier function: Go reads `s.Allocator` for realloc.
- `&Report` (immutable borrow): Go projects via `unsafe.Pointer`, reads fields
  including `report.Name.Ptr` / `report.Name.Len` — zero-copy access to the
  caller's memory.
- Go finalizer (`runtime.SetFinalizer`) calls `dynspire_release` via cgo when
  a wrapper is garbage collected.

### Python (host)

- ctypes `Structure` types mirror the C layout (already done today for
  opaque handles — extends to all DTypes).
- `ctypes.addressof` for borrowing, `ctypes.string_at` for copying.
- `OpaqueHandle.__del__` calls `dynspire_release` via ctypes (replaces the
  current `dynspire_free` call).

### C (spier or host)

- Direct struct definitions matching `#[repr(C)]`.
- Direct calls to `dynspire_alloc`/`release`.
- The most transparent target — DTypes are just C structs.

---

## IDL DType Support (Implemented)

The managed types from the design above are exposed as **first-class, opt-in
types in the `.dspi` IDL**. A spier/host may use them alongside the ordinary
Rust-style types (`String`, `Vec<T>`, `&str`, `&[u8]`, `Option<T>`). There is
no automatic conversion: if you write `DVec<u8>` in the IDL you get the managed
`DVec<u8>` across the boundary — zero-copy, owner-tracked, FFI-stable.

### IDL syntax

| IDL type | Meaning | Wire (slots) |
|---|---|---|
| `DStr` | immutable `&str`-like view | `ptr: u64, len: u64` |
| `DSlice<T>` | immutable `&[T]`-like view | `ptr: u64, len: u64` |
| `DString` | owned string (carries allocator) | `allocator, ptr, len, cap` (4 slots) |
| `DVec<T>` | owned `Vec<T>` (carries allocator) | `allocator, ptr, len, cap` (4 slots) |
| `DOption<T>` | managed `Option<T>` (tag + value) | `tag: u64` + value slots |

Views (`DStr`, `DSlice<T>`) are non-owning — they alias caller memory, never an
allocator pointer. Owned types (`DString`, `DVec<T>`) carry their allocator as
the first field, so the receiver can later release the buffer through the inline
RC header.

### Authoring ergonomics — no allocator parameter

The allocator is configured once at `dynspire_create` and lives inside the
spier's `State` (see [Allocator Lifecycle](#allocator-lifecycle)). Codegen
recovers it from the trait's `&self` via `offset_of!(State, inner)`, so **no
extra allocator parameter appears in the IDL or the trait**:

```rust
// spier side — author writes plain Rust, allocator hidden:
fn echo_bytes(&self, data: &[u8]) -> Result<OwnedDVec<u8>, String> {
    let mut out = self.new_dvec(data.len());   // DynSpireStateExt::new_dvec
    for &b in data { out.push(b); }
    Ok(out)                                     // into_raw() hands buffer to host
}
```

The generated `impl_{name}_spier!` macro injects a `DynSpireStateExt` impl for
the state type that exposes `new_dvec`/`new_dstring` (backed by the recovered
allocator). On the host, the generated `DynSpire{Name}` client exposes the same
helpers:

```rust
// host side:
let echoed = client.echo_bytes(&input[..]).unwrap();   // OwnedDVec<u8>
let copied = echoed.as_slice().to_vec();                 // copy out if needed
// release happens on Drop of `echoed`
```

### Trait signatures & ownership

Rust-side trait signatures use the managed types for owned values:

- `DVec<T>` / `DString` **returns** materialize as `OwnedDVec<T>` /
  `OwnedDString` — owning guards whose `Drop` calls `dynspire_release`, so the
  host is the sole owner. The spier side returns an `OwnedD*`; the generated
  dispatch converts it with `into_raw()` *before* writing the slots, so the
  buffer is **not** released on the spier side.
- `DVec<T>` / `DString` **parameters** are received by value as `DVec<T>` /
  `DString` (the managed value, `Copy`). The spier only reads them; the host
  retains ownership and releases on its side.
- `DStr` / `DSlice<T>` parameters are received by value as the managed view
  (`ptr` + `len`); zero-copy over host memory.
- `DOption<T>` is the managed `DOption<T>` struct (`tag` + `value`) on **both**
  sides — never a Rust `Option<T>`. Python projects it to `None` / the inner
  value.

### Python projection

The generated `SpierClient` emits `ctypes` classes for every managed type
(`DStr`, `DSlice`, `DString`, `DVec`, `DOption`) plus owning wrappers
(`OwnedDVec`, `DStringHandle`) that call `dynspire_release` on `__del__`, and
`new_dvec` / `new_dstring` helpers backed by the host allocator. `DOption`
returns map to `None` or the inner Python value.

```python
dv = c.echo_bytes(input_data)        # OwnedDVec
copied = dv.as_bytes()                # zero-copy view
n = c.consume_dvec(dv)               # pass the (Copy) view back
# freed on GC via __del__
```

### Status

Implemented in `dynspire` (`src/managed.rs`), `dynspire-codegen` (`ast.rs`,
`parser.rs`, `gen.rs`), and exercised end-to-end by the RLE demo spier/host
(Rust) and `demo/rle_client.py` (Python). Allocated returns, view borrows, and
`DOption` all round-trip across the FFI boundary.

---

## Codegen Changes

### Struct/enum definitions

Current (`gen.rs:718`):
```rust
#[derive(Clone, Debug, PartialEq)]
pub struct CompressionReport { ... }
```

Proposed:
```rust
#[repr(C)]
#[derive(Clone, Debug, PartialEq)]
pub struct CompressionReport { ... }
```

One-line addition per struct/enum. Enums get `#[repr(C, u32)]` for tag + union
layout.

### Field type translation

Inside struct fields, IDL types are translated to DTypes for C-stable layout:

| IDL type in field | Current Rust emit | Proposed DType emit |
|---|---|---|
| `String` | `String` | `DString` |
| `Vec<T>` | `Vec<T>` | `DVec<DTypeOf(T)>` |
| `&str` (in borrowed struct) | `&str` | `DStr` |
| `&[u8]` (in borrowed struct) | `&[u8]` | `DSlice<u8>` |
| `Option<T>` | `Option<T>` | `DOption<DTypeOf(T)>` |
| primitives | same | same (no change) |
| named structs | same | same (already `repr(C)`) |

Function parameter types are **unchanged** — the slot system already handles
`&[u8]`, `&str`, `String`, `Vec<T>` as `(ptr, len)` pairs. DTypes only matter
when types appear **inside struct fields** where layout must be stable.

### Dispatch layer

The generated dispatch function (spier side) reads the allocator from `State`:
- Reads `alloc = state.allocator` (set once in `dynspire_create`).
- When constructing return values, allocates via `dynspire_alloc(alloc, ...)`.
- Writes RC header with `rc=1`, `type_index`, `drop_fn`, `allocator=alloc`.
- For `DString` returns: allocates buffer, constructs `DString { allocator,
  ptr, len, cap }`.
- For `DVec<T>` returns: allocates element buffer, constructs `DVec {
  allocator, ptr, len, cap }`.
- For composite struct returns: allocates struct, populates fields (each
  dynamic field gets its own sub-allocation in the same allocator).
- For opaque returns: allocates with the spier-provided `drop_fn`.

### Host wrapper

The generated `DynSpire{Name}` wrapper:
- Creates a default allocator once (default path) in `connect` / constructor,
  passes it to `dynspire_create`, and holds it alive for the spier's lifetime.
- Reads return value, converts DTypes → Rust stdlib types (copy out of
  allocator into Rust-owned `String`/`Vec<T>`).
- Calls `dynspire_release` on the DType pointers (RC → 0, dealloc).
- For opaque returns: wraps in an `OpaqueHandle` that retains the pointer and
  calls `dynspire_release` on `Drop`.

### `dynspire_free` removal

The current `dynspire_free` function (`gen.rs:850-889`) with its `type_index`
dispatch table is removed. Drop logic moves to the RC header:
- `drop_fn` stored in the header at allocation time.
- `dynspire_release` calls `drop_fn` when RC reaches zero, then `dealloc`.
- No runtime `type_index` dispatch — the function pointer is stored directly.

### Trait signatures

For the ordinary Rust-style types, the spier trait continues to use stdlib
types (`String`, `Vec<T>`, `&str`, `&[u8]`, `Option<T>`); the dispatch layer
handles conversion to/from DTypes. For **owned managed types**, the trait
exposes the managed types directly — the IDL type *is* the boundary type:

```rust
pub trait RleEngine: Send + Sync {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn analyze(&self, data: &[u8]) -> Result<CompressionReport, String>;
    // owned managed returns expose the DType guard:
    fn echo_bytes(&self, data: &[u8]) -> Result<OwnedDVec<u8>, String>;
    fn build_string(&self, data: &[u8]) -> Result<DString, String>;
    // owned managed params arrive as the managed value (Copy):
    fn consume_dvec(&self, data: DVec<u8>) -> Result<u64, String>;
    // views are zero-copy over caller memory:
    fn view_slice(&self, data: DSlice<u8>) -> Result<u64, String>;
    // DOption stays a managed struct on both sides (not Rust Option):
    fn probe(&self, data: &[u8]) -> Result<DOption<u8>, String>;
}
```

The dispatch layer (generated) handles the `into_raw()` conversion for owned
returns and the `from_raw` reconstruction on the host, plus the allocator
recovery from `State`. The author never sees an allocator parameter — `self`
(spier) / `client` (host) provides `new_dvec` / `new_dstring`.

---

## Implementation Phases

### Phase 1 — Allocator, RC, and DTypes in the runtime

**Scope:** `dynspire/src/` (runtime crate)

- `DynSpireAllocator` / `DynSpireAllocatorVtable` with `#[repr(C)]`
- `DynSpireHeader` (RC + `type_index` + `drop_fn` + `allocator` + size/align)
- `DynSpireAllocatorVtable` with `#[repr(C)]`
- `DynSpireAllocator` with `#[repr(C)]`
- Default allocator backed by `std::alloc`
- C-ABI: `dynspire_alloc`, `dynspire_dealloc`, `dynspire_realloc`,
  `dynspire_retain`, `dynspire_release`
- DTypes: `DStr`, `DSlice<T>`, `DString`, `DVec<T>`, `DOption<T>` with
  `#[repr(C)]`
- `ReprC` marker trait
- Unit tests for allocation, retain/release, drop_fn dispatch, realloc

**Validation:** Allocator + RC + DTypes work in isolation, no codegen changes
yet.

### Phase 2 — Codegen: `repr(C)` and DType fields in structs

**Scope:** `dynspire-codegen/src/gen.rs`

- Add `#[repr(C)]` to all generated struct definitions
- Add `#[repr(C, u32)]` to all generated enum definitions
- Translate IDL types to DTypes inside struct fields (`String` → `DString`,
  `Vec<T>` → `DVec<T>`, etc.)
- Emit DType definitions alongside struct definitions
- Emit `drop_fn` per composite type with dynamic fields

**Validation:** Generated structs have C-stable layout. `&MyStruct` is
projectable cross-lang. Existing slot-based function params/returns unchanged.

### Phase 3 — Dispatch layer with allocator + RC

**Scope:** `dynspire-codegen/src/gen.rs`, `dynspire/src/tower.rs`

- Update `FnCreate` signature to include `*mut DynSpireAllocator` as first
  parameter; spier `State` stores it and dispatch reads `state.allocator`
- Generate dispatch functions that read the allocator from `State` and
  allocate returns in it
- Write RC header with `drop_fn` at allocation time
- Generate host wrappers that create a default allocator once (at connect),
  pass it to `dynspire_create`, hold it for the spier's lifetime, consume
  returns, and call `dynspire_release`
- Conversion between DTypes and Rust stdlib types in the dispatch/host layers
- Remove `dynspire_free` and `dynspire_vec_*` functions

**Validation:** RLE demo (spier + host) works end-to-end with allocator + RC.
Rust spier trait signatures unchanged (stdlib types).

### Phase 4 — Go codegen target

**Scope:** `dynspire-codegen/src/` (new Go backend)

- Go struct definitions mirroring C layout
- cgo wrappers for `dynspire_alloc`/`realloc`/`retain`/`release` functions
- Go spier entry points (`dynspire_create`, `dynspire_dispatch_{method}`,
  `dynspire_destroy`)
- Go trait equivalent (interface type)
- `runtime.SetFinalizer` integration for GC → `dynspire_release`

**Validation:** A Go spier implementing the RLE interface loads and runs from
a Rust host.

### Phase 5 — Go RLE demo

**Scope:** `demo/rle-go-spier/`

- Implement RLE spier in Go
- Run from the existing Rust host and Python host
- Validate `&[u8]` borrows (zero-copy), `DVec<u8>` returns (allocator + RC),
  `&mut DVec<u8>` out-params

**Validation:** End-to-end cross-language spier.
