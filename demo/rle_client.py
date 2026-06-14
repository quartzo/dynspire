#!/usr/bin/env python3
"""DynSpire RLE demo — pure Python via ctypes, zero code generation.

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

    print("=== DynSpire RLE Demo (Python / ctypes) ===")
    print()
    print(f"  spier : {lib._lib._name}")
    print(f"  name  : {lib.spier_name()}")
    print(f"  hash  : 0x{lib.idl_hash():016x}")
    print()

    print("  IDL schema (discovered at runtime):")
    for m in schema.methods:
        print(f"    [{m.index}] {schema.method_sig(m)}")
    print()

    input_data = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"

    print(f'  input : "{input_data.decode()}" ({len(input_data)} bytes)')
    print()

    with lib.create_handle() as handle:
        compressed = handle.call("compress", input_data)
        print("compress()")
        print(f"  -> [{hex_fmt(compressed)}] ({len(compressed)} bytes)")
        print()

        decompressed = handle.call("decompress", compressed)
        ok = decompressed == input_data
        print("decompress()")
        print(
            f'  -> "{decompressed.decode()}" ({len(decompressed)} bytes) '
            f'{"[round-trip OK]" if ok else "[MISMATCH]"}'
        )
        print()

        result = handle.call("compress_into", input_data)
        ok = result == compressed
        print("compress_into(&mut Vec<u8>)")
        print("  spier wrote into a Rust Vec created by dynspire_vec_create()")
        print(
            f"  -> [{hex_fmt(result)}] ({len(result)} bytes) "
            f'{"[matches compress]" if ok else "[MISMATCH]"}'
        )
        print()

        orig, comp = handle.call("stats", input_data)
        ratio = comp * 100.0 / orig if orig > 0 else 0.0
        print("stats()")
        print(f"  original  : {orig} bytes")
        print(f"  compressed: {comp} bytes")
        print(f"  ratio     : {ratio:.1f}%")
        print()

        # analyze returns a #[slot_struct] — opaque boxed pointer (1 slot).
        # Python receives a raw integer handle. Fields are accessed via
        # explicit IDL methods, not by reading Rust memory directly.
        report_handle = handle.call("analyze", input_data)
        print("analyze() -> CompressionReport")
        print(f"  opaque handle : 0x{report_handle:016x}")
        print()

        # Pass the opaque handle back to an IDL method that reads the struct
        # on the Rust side and returns a base type (String).
        summary = handle.call("report_summary", report_handle)
        print("report_summary(handle)")
        print(f'  -> "{summary}"')

    print()
    print("Done. Everything discovered and dispatched via ctypes at runtime.")


if __name__ == "__main__":
    main()
