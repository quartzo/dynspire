# AGENTS.md

## DynSpire Architecture Rules

### The `.dspi` file is the contract — never bypass it

The `.dspi` interface file is the single source of truth. A `build.rs`
step invokes `dynspire_codegen::build()` to generate all Rust code:

- **Spier crate**: compiles the `.dspi` via its own `build.rs`.
  Implements the generated trait, then invokes
  `impl_{name}_spier!($state, init, "name")` — a `macro_rules!` that
  generates all C-ABI dispatch functions, create/destroy, and schema
  exports.
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
