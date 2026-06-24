# AGENTS.md

## DynSpire Architecture Rules

### The `.dspi` file is the contract — never bypass it

The `.dspi` interface file is the single source of truth. A `build.rs`
step invokes `dynspire_codegen::build()` to generate all Rust code:

- **IDL crate**: `build.rs` reads `.dspi`, writes generated `.rs` to
  `OUT_DIR`. `lib.rs` does `include!()`. The generated code contains the
  trait, types, Op enum, schema, hash, tower client wrapper, and the
  spier dispatch macro.
- **Spier crate**: depends on the IDL crate. Implements the generated
  trait, then invokes `impl_{name}_spier!($state, init, "name")` — a
  `macro_rules!` that generates all C-ABI dispatch functions, create/
  destroy, and schema exports.
- **Host crate**: depends on the IDL crate. Uses the generated
  `DynSpire{Name}` client wrapper directly. No handwritten boilerplate.

**NEVER** call `DynSpireClient::call()` directly from host business logic.
Always go through the generated trait wrapper.

### Codegen crate must stay neutral

`dynspire-codegen` contains only the parser and code generators. It does
not depend on the `dynspire` runtime crate. The generated code references
`dynspire::*`, but the codegen itself just produces strings.

### Layout

```
demo/
  rle-idl/       ← IDL: .dspi + build.rs + include!() (generated trait, types, tower, macro)
  rle-spier/     ← Spier: impl trait + impl_rle_spier!() (no proc macros)
  rle-host/      ← Tower: use DynSpireRle (1 import, no boilerplate)
dynspire-codegen/ ← Parser + code generator (.dspi → .rs)
```

### What the DSL replaces

| Before (proc macros) | After (DSL codegen) |
|---|---|
| `#[modulo_interface]` on trait | `.dspi` file + `build.rs` |
| `#[slot_enum]` / `#[slot_struct]` | Types declared in `.dspi` |
| `#[spier_dispatch]` proc macro | Generated `macro_rules! impl_{name}_spier!` |
| `#[spier_storage]` proc macro | Absorbed into `impl_{name}_spier!` |
| Handwritten tower wrapper | Generated `DynSpire{Name}` struct |
