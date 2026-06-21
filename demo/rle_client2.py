#!/usr/bin/env python3
"""DynSpire RLE demo 2 — showcases edge-case return types under the PyO3 engine.

B1: Out-vec with a non-Unit return type.
    compress_into_checked -> Result<bool, String> exercises the &mut Vec<u8>
    path with a bool return. The unified call returns (ret_val, list[bytes])
    automatically — no separate call_with_outs needed.

B2: Vec<Vec<u8>> decoded to a native Python list.
    split_runs -> Result<Vec<Vec<u8>>, String> returns a plain list of bytes,
    so negative indexing works out of the box.

B3: Scalar Option<T> decoded to a native Python value.
    first_byte -> Result<Option<u8>, String> returns a plain int or None,
    so isinstance / is None checks work directly.
"""

from dynspire import load_spier


def hex_fmt(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data)


def main():
    lib = load_spier("rle_spier", lib_dir="target/debug")

    print("=== DynSpire RLE Demo 2 — Edge-Case Showcase ===")
    print()

    input_data = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"

    with lib.create_handle() as h:
        compressed = h.compress(input_data)

        # --- B1: out-vec auto-tuple -------------------------------------
        print("[B1] compress_into_checked(&mut Vec<u8>) -> Result<bool, String>")
        ok, outs = h.compress_into_checked(input_data)
        out_data = outs[0]
        matches = out_data == compressed
        print(f"  out_data : [{hex_fmt(out_data)}] ({len(out_data)} bytes)")
        print(f"  ret_val  : {ok}  (type={type(ok).__name__})")
        print(f"  matches compress(): {matches}")
        print()

        # --- B2: Vec<Vec<u8>> -> native list ----------------------------
        print("[B2] split_runs() -> Result<Vec<Vec<u8>>, String>")
        runs = h.split_runs(input_data)
        first = bytes(runs[0])
        last = bytes(runs[-1])
        print(f"  runs[0]  : {first!r}")
        print(f"  runs[-1] : {last!r}  (negative index, native list)")
        print(f"  type     : {type(runs).__name__}")
        print()

        # --- B3: Scalar Option<T> -> native int | None ------------------
        print("[B3] first_byte() -> Result<Option<u8>, String>")
        some_val = h.first_byte(input_data)
        none_val = h.first_byte(b"")
        print(f"  first_byte(input) : {some_val}  (type={type(some_val).__name__})")
        print(f"  first_byte(b'')   : {none_val}  (is None: {none_val is None})")

    print()
    print("Done. All edge cases handled natively by the PyO3 engine.")


if __name__ == "__main__":
    main()
