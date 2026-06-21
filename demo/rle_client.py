#!/usr/bin/env python3
"""DynSpire RLE demo — Python via PyO3, zero code generation.

The spier's .so is loaded at runtime. Method names, parameter types, and
return types are all discovered via IDL schema reflection — no stubs, no
codegen, no prior knowledge of the interface.
"""

from dynspire import load_spier


def hex_fmt(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data)


def main():
    lib = load_spier("rle_spier", lib_dir="target/debug")
    schema = lib.schema()

    print("=== DynSpire RLE Demo (Python / PyO3) ===")
    print()
    print(f"  name  : {schema.name}")
    print(f"  hash  : 0x{schema.hash:016x}")
    print()

    print("  IDL schema (discovered at runtime):")
    for m in schema.methods:
        print(f"    {schema.method_sig(m)}")
    print()

    input_data = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"

    print(f'  input : "{input_data.decode()}" ({len(input_data)} bytes)')
    print()

    with lib.create_handle() as h:
        compressed = h.compress(input_data)
        print("h.compress()")
        print(f"  -> [{hex_fmt(compressed)}] ({len(compressed)} bytes)")
        print()

        decompressed = h.decompress(compressed)
        ok = decompressed == input_data
        print("h.decompress()")
        print(
            f'  -> "{decompressed.decode()}" ({len(decompressed)} bytes) '
            f'{"[round-trip OK]" if ok else "[MISMATCH]"}'
        )
        print()

        # Out-vec: &mut Vec<u8> params are auto-detected from the schema.
        # The call returns (ret_val, list[bytes]) — one bytes per out-vec.
        _, outs = h.compress_into(input_data)
        result = outs[0]
        ok = result == compressed
        print("h.compress_into(&mut Vec<u8>)")
        print("  spier wrote into a Rust Vec created by dynspire_vec_create()")
        print(
            f"  -> [{hex_fmt(result)}] ({len(result)} bytes) "
            f'{"[matches compress]" if ok else "[MISMATCH]"}'
        )
        print()

        orig, comp = h.stats(input_data)
        ratio = comp * 100.0 / orig if orig > 0 else 0.0
        print("h.stats()")
        print(f"  original  : {orig} bytes")
        print(f"  compressed: {comp} bytes")
        print(f"  ratio     : {ratio:.1f}%")
        print()

        # analyze returns a #[slot_struct] — opaque boxed pointer (1 slot).
        # Python receives an OpaqueHandle. Fields are accessed via explicit
        # IDL methods, not by reading Rust memory directly.
        report_handle = h.analyze(input_data)
        print("h.analyze() -> CompressionReport")
        print(f"  {report_handle!r}")
        print()

        # Pass the opaque handle back to an IDL method that reads the struct
        # on the Rust side and returns a base type (String).
        summary = h.report_summary(report_handle)
        print("h.report_summary(handle)")
        print(f'  -> "{summary}"')

    print()
    print("Done. Everything discovered and dispatched via PyO3 at runtime.")


if __name__ == "__main__":
    main()
