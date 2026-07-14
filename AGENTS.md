# AGENTS.md

## DynSpire Architecture Rules

### The `.dspi` file is the contract — never bypass it

The `.dspi` interface file is the single source of truth. A `build.rs`
step invokes `dynspire_codegen::build()` to generate all Rust code:

- **Spier crate**: compiles the `.dspi` via its own `build.rs`.
  Implements the generated trait, then invokes
  `impl_{name}_spier!($state, init, "name")` — a `macro_rules!` that
  generates all C-ABI dispatch functions, create/destroy, and name
  exports. The `build.rs` also emits a typed Python client (`.py`) via
  `BuildContext::build_python()`.
- **Host crate**: compiles the same `.dspi` (independently or via a
  shared IDL crate). Uses the generated `DynSpire{Name}` client wrapper
  directly. No handwritten boilerplate.

> **The IDL hash is the contract, not the crate dependency.** The spier
> and host can each compile the `.dspi` independently — the hash ensures
> compatibility at load time. A shared IDL crate is a convenience for
> single-team projects, not a requirement.

**NEVER** call `DynSpireClient::call()` directly from host business logic.
Always go through the generated trait wrapper.

### Codegen crate must stay neutral

`dynspire-codegen` contains only the parser and code generators. It does
not depend on the `dynspire` runtime crate. The generated code references
`dynspire::*`, but the codegen itself just produces strings.

### Layout

Two valid patterns (demo uses the shared-crate pattern):

```
Shared IDL crate:                    Independent compilation:

demo/                                  my-spier/        my-host/
  rle-idl/   ← .dspi + build.rs          build.rs         build.rs
  rle-spier/ ← depends on rle-idl        src/my.dspi      src/my.dspi
  rle-host/  ← depends on rle-idl        src/lib.rs       src/main.rs
dynspire-codegen/ ← .dspi → .rs
```

### What the DSL replaces

| Before (proc macros) | After (DSL codegen) |
|---|---|
| `#[modulo_interface]` on trait | `.dspi` file + `build.rs` |
| `#[slot_enum]` / `#[slot_struct]` | Types declared in `.dspi` |
| `#[spier_dispatch]` proc macro | Generated `macro_rules! impl_{name}_spier!` |
| `#[spier_storage]` proc macro | Absorbed into `impl_{name}_spier!` |
| Handwritten tower wrapper | Generated `DynSpire{Name}` struct |
| PyO3 runtime reflection | Codegen-emitted typed Python (`.py`) via `build_python()` |

### The IDL is language-agnostic — only DTypes cross the boundary

The `.dspi` grammar accepts **only** DynSpire managed types, primitives,
tuples, arrays, and named (user-declared) types. Native Rust types
(`String`, `Vec<T>`, `Option<T>`, `&str`, `&[u8]`, `&mut Vec<u8>`) are
**rejected at parse time** — the IDL is the contract, not a Rust mirror.

The closed set of managed types:

- `DString` — owned, RC-aware string (`{allocator, ptr, len, cap}`).
- `DVec<T>` — owned, RC-aware vec of `T: ReprC`.
- `DOption<T>` — managed optional (`{tag, _pad, value}`).
- `&T` — shared borrow view. For `&DString` the Rust decode produces a
  `DStr` (ptr+len); for `&DVec<T>` it produces a `DSlice<T>` (ptr+len).
  Wire format: 2 slots.
- `&mut DVec<T>` — mutable borrow (out-param). Generalizes the old
  `&mut Vec<u8>` to any element type. Wire format: 1 slot (raw ptr).

Borrow semantics (`&` / `&mut`) are **orthogonal** to the underlying
type. The DTypes (`DString` / `DVec<T>`) carry the memory ownership
strategy; the `&` / `&mut` operators express the borrow mode for the
duration of a single call.

### RC-aware owned types — no separate "guard" wrappers

`DVec<T>` and `DString` are **RC-aware**: `Clone` retains (calls
`dynspire_retain`), `Drop` releases (calls `dynspire_release`). The
backing buffer is freed exactly once when the last clone drops. There
is **no** `OwnedDVec` / `OwnedDString` wrapper anymore — the same type
is used for input, output, and across the FFI boundary.

FFI handoff uses `DVec::into_raw(self) -> Self` (forgets the Drop) on
the spier side and `DVec::from_raw(raw)` on the host side. The wire
format is the raw `repr(C)` struct (4 slots: allocator, ptr, len, cap).

Mutation methods (`push`, `resize`) require single ownership
(refcount == 1); they panic in debug builds if the buffer is shared.
This matches the "no direct mutation when shared" rule of `Rc::get_mut`.
