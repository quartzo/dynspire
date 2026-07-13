#!/usr/bin/env python3
"""Struct class tests — pure Python, no .so needed.

Verifies that codegen-generated struct classes have correct constructors,
field accessors, repr, eq, hash, _from_ptr, and _default.
"""

import ctypes
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "rle-spier", "generated"))

from rle import CompressionReport  # noqa: E402


# --- Construction ---

def test_positional_args():
    r = CompressionReport(100, 50, 0.5, 8)
    assert r.original_size == 100
    assert r.compressed_size == 50
    assert r.ratio == 0.5
    assert r.runs == 8


def test_keyword_args():
    r = CompressionReport(ratio=0.25, original_size=200, runs=4, compressed_size=100)
    assert r.original_size == 200
    assert r.compressed_size == 100
    assert r.ratio == 0.25
    assert r.runs == 4


def test_default_values():
    r = CompressionReport(None, None, None, None)
    assert r.original_size == 0
    assert r.compressed_size == 0
    assert r.ratio == 0.0
    assert r.runs == 0


# --- @property accessors (read-only) ---

def test_properties_are_readonly():
    r = CompressionReport(10, 20, 0.5, 2)
    try:
        r.original_size = 999  # noqa: B010
        assert False, "property should be read-only"
    except AttributeError:
        pass


# --- __repr__ ---

def test_repr():
    r = CompressionReport(100, 50, 0.5, 8)
    s = repr(r)
    assert s.startswith("CompressionReport(")
    assert "original_size=100" in s
    assert "compressed_size=50" in s
    assert "ratio=0.5" in s
    assert "runs=8" in s
    assert s.endswith(")")


def test_repr_float_format():
    r = CompressionReport(0, 0, 1.0 / 3.0, 0)
    s = repr(r)
    assert "ratio=" in s


# --- __eq__ ---

def test_eq_equal():
    a = CompressionReport(100, 50, 0.5, 8)
    b = CompressionReport(100, 50, 0.5, 8)
    assert a == b


def test_eq_different_values():
    a = CompressionReport(100, 50, 0.5, 8)
    b = CompressionReport(100, 50, 0.5, 9)
    assert a != b


def test_eq_different_type():
    r = CompressionReport(100, 50, 0.5, 8)
    assert r != "not a report"
    assert r != 42
    assert r != None  # noqa: E711


# --- __hash__ ---

def test_hash_equal_objects():
    a = CompressionReport(100, 50, 0.5, 8)
    b = CompressionReport(100, 50, 0.5, 8)
    assert hash(a) == hash(b)


def test_hash_usable_as_dict_key():
    r = CompressionReport(100, 50, 0.5, 8)
    d = {r: "value"}
    r2 = CompressionReport(100, 50, 0.5, 8)
    assert d[r2] == "value"


def test_hash_usable_in_set():
    a = CompressionReport(100, 50, 0.5, 8)
    b = CompressionReport(100, 50, 0.5, 8)
    s = {a, b}
    assert len(s) == 1


# --- _default ---

def test_default_classmethod():
    r = CompressionReport._default()
    assert r.original_size == 0
    assert r.compressed_size == 0
    assert r.ratio == 0.0
    assert r.runs == 0


# --- _from_ptr (ctypes round-trip) ---

def test_from_ptr():
    from rle import CompressionReportCtypes  # noqa: E402
    # Build a ctypes buffer with known values
    buf = CompressionReportCtypes(42, 21, 0.5, 7)

    ptr = ctypes.addressof(buf)
    r = CompressionReport._from_ptr(None, ptr)

    assert r.original_size == 42
    assert r.compressed_size == 21
    assert r.ratio == 0.5
    assert r.runs == 7


# --- ctypes mirror ---

def test_ctypes_mirror_fields():
    from rle import CompressionReportCtypes  # noqa: E402
    buf = CompressionReportCtypes(100, 50, 0.5, 8)
    assert buf.original_size == 100
    assert buf.compressed_size == 50
    assert buf.ratio == 0.5
    assert buf.runs == 8


def test_ctypes_mirror_sizeof():
    from rle import CompressionReportCtypes  # noqa: E402
    # 3 x u64 (24) + 1 x f64 (8) = 32 bytes
    assert ctypes.sizeof(CompressionReportCtypes) == 32


def test_ctypes_mirror_addressable():
    from rle import CompressionReportCtypes  # noqa: E402
    buf = CompressionReportCtypes(10, 20, 0.5, 3)
    addr = ctypes.addressof(buf)
    assert addr > 0


# --- run all tests ---

if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    passed = 0
    failed = 0
    for t in tests:
        try:
            t()
            passed += 1
        except Exception as e:
            failed += 1
            print(f"FAIL: {t.__name__}: {e}")
    print(f"\n{passed} passed, {failed} failed")
    if failed:
        sys.exit(1)
