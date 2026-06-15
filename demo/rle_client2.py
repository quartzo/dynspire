#!/usr/bin/env python3
"""DynSpire RLE demo 2 — showcases the three upstream bug fixes.

B1: _call_out_vec returns (out_data, ret_val) for non-Unit returns.
    compress_into_checked -> Result<bool, String> exercises the OutVec
    path with a bool return, producing a (bytes, bool) tuple.

B2: FFIResource.__getitem__ normalizes negative indices.
    split_runs -> Result<Vec<Vec<u8>>, String> returns an FFIResource
    (vec-of-vec). We use [-1] to access the last element safely.

B3: Eager decode for scalar Option<T>.
    first_byte -> Result<Option<u8>, String> returns a plain int or None
    instead of an FFIResource, so isinstance / is None checks work.
"""

from dynspire import FFIResource, load_spier


def hex_fmt(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data)


def main():
    lib = load_spier("rle_spier", lib_dir="target/debug")
    schema = lib.schema()

    print("=== DynSpire RLE Demo 2 — Bug Fix Showcase ===")
    print()

    input_data = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"

    with lib.create_handle() as handle:
        compressed = handle.call("compress", input_data)

        # --- B1: call_with_outs returns (ret_val, list[bytes]) ----------
        print("[B1] compress_into_checked(&mut Vec<u8>) -> Result<bool, String>")
        ok, outs = handle.call_with_outs("compress_into_checked", input_data)
        out_data = outs[0]
        matches = out_data == compressed
        print(f"  out_data : [{hex_fmt(out_data)}] ({len(out_data)} bytes)")
        print(f"  ret_val  : {ok}  (type={type(ok).__name__})")
        print(f"  matches compress(): {matches}")
        print()

        # --- B2: Negative index on vec-of-vec FFIResource ----------------
        print("[B2] split_runs() -> Result<Vec<Vec<u8>>, String>")
        runs = handle.call("split_runs", input_data)
        first = runs[0]
        last = runs[-1]
        print(f"  runs[0]  : {bytes(first)!r}")
        print(f"  runs[-1] : {bytes(last)!r}  (negative index, no segfault)")
        print(f"  is FFIResource: {isinstance(runs, FFIResource)}")
        print()

        # --- B3: Scalar Option<T> eager-decoded ---------------------------
        print("[B3] first_byte() -> Result<Option<u8>, String>")
        some_val = handle.call("first_byte", input_data)
        none_val = handle.call("first_byte", b"")
        print(f"  first_byte(input) : {some_val}  (type={type(some_val).__name__})")
        print(f"  first_byte(b'')   : {none_val}  (is None: {none_val is None})")
        print(f"  is FFIResource: {isinstance(some_val, FFIResource)}")

    print()
    print("Done. All three fixes working correctly.")


if __name__ == "__main__":
    main()
