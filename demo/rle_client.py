#!/usr/bin/env python3
"""DynSpire RLE demo — Python via generated ctypes bindings.

The spier's .so is loaded at runtime through a code-generated typed client
(produced by dynspire-codegen's build_python at spier build time). No PyO3,
no runtime reflection — just a clean typed API.
"""

import os
import sys
import ctypes

# The generated rle.py lives next to the spier crate's build output
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "rle-spier", "generated"))

import rle  # noqa: E402
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

        # --- Optional managed types (DVec / DString) ---
        # echo_bytes returns an OwnedDVec backed by the spier allocator.
        dv = c.echo_bytes(input_data)
        print("c.echo_bytes() -> DVec<u8> (zero-copy)")
        print(f"  -> [{hex_fmt(dv.as_bytes())}] ({len(dv)} bytes)")
        print()

        # consume_dvec takes the (Copy) view back; host still owns/frees it.
        n = c.consume_dvec(dv)
        print("c.consume_dvec(DVec<u8>)")
        print(f"  -> {n} (host still owns the buffer, freed on GC)")
        print()

        ds = c.build_string(input_data)
        print("c.build_string() -> DString (zero-copy)")
        print(f'  -> "{ds.as_str()}" ({len(ds)} bytes)')
        print()

        ns = c.consume_dstring(ds)
        print("c.consume_dstring(DString)")
        print(f"  -> {ns} (host still owns the buffer, freed on GC)")
        print()

        # Views: pass a DStr / DSlice over host-owned memory (no copy).
        buf = (ctypes.c_ubyte * len(input_data)).from_buffer_copy(input_data)
        ptr = ctypes.cast(buf, ctypes.c_void_p)
        dstr = rle.DStr(ptr, len(input_data))
        vn = c.view_len(dstr)
        print("c.view_len(DStr)")
        print(f"  -> {vn} (zero-copy view over host memory)")
        print()

        dslice = rle.DSlice(ptr, len(input_data))
        vs = c.view_slice(dslice)
        print("c.view_slice(DSlice<u8>)")
        print(f"  -> {vs} (zero-copy view over host memory)")
        print()

        # DOption return: None or the present value.
        present = c.probe(input_data)
        maxv = c.opt_classify(input_data)
        print("c.probe() / c.opt_classify() -> DOption<u8>")
        print(f"  probe        -> {present!r}")
        print(f"  opt_classify -> {maxv!r} (max byte)")

    # --- allocator report (debug allocator) ---
    # A separate client backed by the debug allocator tracks live/peak/total
    # memory occupation across all spier allocations.
    with Rle(lib_path, debug=True) as d:
        d.compress(input_data)
        d.analyze(input_data)
        rep = d.allocator_report()
        print("allocator_report() (debug allocator)")
        print(f"  live bytes        : {rep.live_bytes}")
        print(f"  live allocations  : {rep.live_allocations}")
        print(f"  peak bytes        : {rep.peak_bytes}")
        print(f"  total allocations : {rep.total_allocations}")

    print()
    print("Done. Typed dispatch via code-generated ctypes bindings.")


if __name__ == "__main__":
    main()
