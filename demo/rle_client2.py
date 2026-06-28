#!/usr/bin/env python3
"""DynSpire RLE demo 2 — edge-case return types via generated ctypes bindings.

B1: Out-vec with a non-Unit return type.
    compress_into_checked -> Result<bool, String> exercises the &mut Vec<u8>
    path with a bool return. The generated method returns (ret_val, list[bytes])
    automatically.

B2: Vec<Vec<u8>> decoded to a native Python list.
    split_runs -> Result<Vec<Vec<u8>>, String> returns a plain list of bytes.

B3: Scalar Option<T> decoded to a native Python value.
    first_byte -> Result<Option<u8>, String> returns a plain int or None.

B4: Vec<String> and enum round-trip.
    run_labels / classify / describe_tone.
"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "rle-spier", "generated"))

from rle import Rle, Tone  # noqa: E402


def hex_fmt(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data)


def main():
    lib_path = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "librle_spier.so")

    print("=== DynSpire RLE Demo 2 — Edge-Case Showcase ===")
    print()

    input_data = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"

    with Rle(lib_path) as c:
        compressed = c.compress(input_data)

        # --- B1: out-vec auto-tuple -------------------------------------
        print("[B1] compress_into_checked(&mut Vec<u8>) -> Result<bool, String>")
        ok, outs = c.compress_into_checked(input_data)
        out_data = outs[0]
        matches = out_data == compressed
        print(f"  out_data : [{hex_fmt(out_data)}] ({len(out_data)} bytes)")
        print(f"  ret_val  : {ok}  (type={type(ok).__name__})")
        print(f"  matches compress(): {matches}")
        print()

        # --- B2: Vec<Vec<u8>> -> native list ----------------------------
        print("[B2] split_runs() -> Result<Vec<Vec<u8>>, String>")
        runs = c.split_runs(input_data)
        first = bytes(runs[0])
        last = bytes(runs[-1])
        print(f"  runs[0]  : {first!r}")
        print(f"  runs[-1] : {last!r}  (negative index, native list)")
        print(f"  type     : {type(runs).__name__}")
        print()

        # --- B3: Scalar Option<T> -> native int | None ------------------
        print("[B3] first_byte() -> Result<Option<u8>, String>")
        some_val = c.first_byte(input_data)
        none_val = c.first_byte(b"")
        print(f"  first_byte(input) : {some_val}  (type={type(some_val).__name__})")
        print(f"  first_byte(b'')   : {none_val}  (is None: {none_val is None})")
        print()

        # --- B4: Vec<String> and enum round-trip ------------------------
        print("[B4] run_labels() / classify() / describe_tone()")
        labels = c.run_labels(input_data)
        print(f"  run_labels : {labels}")
        tone = c.classify(input_data)
        print(f"  classify   : {tone}")
        print(f"  describe   : {c.describe_tone(tone)}")

    print()
    print("Done. All edge cases handled natively by generated ctypes bindings.")


if __name__ == "__main__":
    main()
