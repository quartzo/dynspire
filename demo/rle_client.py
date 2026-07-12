#!/usr/bin/env python3
"""DynSpire RLE demo — Python via generated ctypes bindings.

The spier's .so is loaded at runtime through a code-generated typed client
(produced by dynspire-codegen's build_python at spier build time). No PyO3,
no runtime reflection — just a clean typed API.
"""

import os
import sys

# The generated rle.py lives next to the spier crate's build output
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "rle-spier", "generated"))

from rle import Rle  # noqa: E402


def hex_fmt(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data)


def main():
    lib_path = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "librle_spier.so")

    print("=== DynSpire RLE Demo (Python / codegen) ===")
    print()

    input_data = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"

    print(f'  input : "{input_data.decode()}" ({len(input_data)} bytes)')
    print()

    with Rle(lib_path) as c:
        compressed = c.compress(input_data)
        print("c.compress()")
        print(f"  -> [{hex_fmt(compressed)}] ({len(compressed)} bytes)")
        print()

        decompressed = c.decompress(compressed)
        ok = decompressed == input_data
        print("c.decompress()")
        print(
            f'  -> "{decompressed.decode()}" ({len(decompressed)} bytes) '
            f'{"[round-trip OK]" if ok else "[MISMATCH]"}'
        )
        print()

        # Out-vec: &mut Vec<u8> params are handled automatically.
        # The call returns (ret_val, list[bytes]) — one bytes per out-vec.
        _, outs = c.compress_into(input_data)
        result = outs[0]
        ok = result == compressed
        print("c.compress_into(&mut Vec<u8>)")
        print("  spier filled a DVec<u8> backed by the host allocator")
        print(
            f"  -> [{hex_fmt(result)}] ({len(result)} bytes) "
            f'{"[matches compress]" if ok else "[MISMATCH]"}'
        )
        print()

        orig, comp = c.stats(input_data)
        ratio = comp * 100.0 / orig if orig > 0 else 0.0
        print("c.stats()")
        print(f"  original  : {orig} bytes")
        print(f"  compressed: {comp} bytes")
        print(f"  ratio     : {ratio:.1f}%")
        print()

        # analyze returns an opaque struct — boxed pointer.
        # Python receives an OpaqueHandle that is freed on GC.
        report = c.analyze(input_data)
        print("c.analyze() -> CompressionReport")
        print(f"  {report!r}")
        print()

        # Pass the opaque handle back to an IDL method that reads the struct
        # on the Rust side and returns a base type (String).
        summary = c.report_summary(report)
        print("c.report_summary(handle)")
        print(f'  -> "{summary}"')

    print()
    print("Done. Typed dispatch via code-generated ctypes bindings.")


if __name__ == "__main__":
    main()
