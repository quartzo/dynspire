# AGENTS.md

## DynSpire Architecture Rules

### IDL is the contract — never bypass it

The IDL trait (e.g., `RleEngine`) defines the interface between spier and tower.
Both sides MUST implement it faithfully:

- **Spier side**: `#[spier_dispatch]` generates dispatch functions from the trait.
- **Tower side**: The host crate MUST `impl Trait for DynSpireXxx` explicitly,
  with each method calling `self.client.call(Op::Variant, args)`.

**NEVER** call `DynSpireClient::call()` directly from host business logic.
It is internal infrastructure — type-erased, no editor support, no signature
checking. Always go through the trait.

### IDL crate must stay neutral

`#[modulo_interface]` generates neutral artifacts only: Op enum, schema, hash,
dispatch signatures. The IDL crate must NOT contain tower-side client code.
Client wrappers (`DynSpireXxx` structs + trait impls) belong in the host crate.

### Layout

```
demo/
  rle-idl/     ← IDL: trait + #[modulo_interface] (neutral, no host code)
  rle-spier/   ← Spier: #[spier_dispatch] impl on the trait
  rle-host/    ← Tower: DynSpireRle struct + impl RleEngine (explicit, handwritten)
```
