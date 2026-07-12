#!/usr/bin/env python3
"""DType round-trip tests for the RLE demo — Python ctypes layer.

Verifies that the generated ctypes client handles managed types correctly:
OwnedDVec / DStringHandle (owned returns with Drop), new_dvec / new_dstring
(host allocation), DStr / DSlice (zero-copy views), and DOption (tag+value).
"""

import ctypes
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "rle-spier", "generated"))

import rle  # noqa: E402
from rle import Rle  # noqa: E402


LIB_PATH = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "librle_spier.so")


def test_echo_bytes_owned():
    """echo_bytes returns an OwnedDVec that holds valid data."""
    with Rle(LIB_PATH) as c:
        dv = c.echo_bytes(b"hello world")
        raw = dv.as_bytes()
        assert raw == b"hello world", f"expected b'hello world', got {raw!r}"
        assert len(dv) == 11
        del dv  # __del__ frees the spier-allocated buffer.


def test_build_string_owned():
    """build_string returns a DStringHandle with valid UTF-8."""
    with Rle(LIB_PATH) as c:
        ds = c.build_string(b"test string")
        assert ds.as_str() == "test string"
        assert len(ds) == 11
        del ds


def test_consume_dvec():
    """consume_dvec accepts an OwnedDVec (Copy view) and returns len."""
    with Rle(LIB_PATH) as c:
        dv = c.echo_bytes(b"abcdef")
        n = c.consume_dvec(dv)
        assert n == 6
        del dv


def test_consume_dstring():
    """consume_dstring accepts a DStringHandle and returns len."""
    with Rle(LIB_PATH) as c:
        ds = c.build_string(b"xyz")
        n = c.consume_dstring(ds)
        assert n == 3
        del ds


def test_view_len():
    """view_len accepts a DStr over host memory and returns byte count."""
    with Rle(LIB_PATH) as c:
        data = b"view test"
        buf = (ctypes.c_ubyte * len(data)).from_buffer_copy(data)
        ptr = ctypes.cast(buf, ctypes.c_void_p)
        dstr = rle.DStr(ptr, len(data))
        n = c.view_len(dstr)
        assert n == len(data)


def test_view_slice():
    """view_slice accepts a DSlice over host memory and returns len."""
    with Rle(LIB_PATH) as c:
        data = b"slice test"
        buf = (ctypes.c_ubyte * len(data)).from_buffer_copy(data)
        ptr = ctypes.cast(buf, ctypes.c_void_p)
        dslice = rle.DSlice(ptr, len(data))
        n = c.view_slice(dslice)
        assert n == len(data)


def test_probe_some():
    """probe returns the first byte when input is non-empty."""
    with Rle(LIB_PATH) as c:
        result = c.probe(b"ABC")
        assert result == 65  # ord('A')


def test_probe_empty():
    """probe returns None when input is empty."""
    with Rle(LIB_PATH) as c:
        result = c.probe(b"")
        assert result is None


def test_opt_classify():
    """opt_classify returns the max byte value."""
    with Rle(LIB_PATH) as c:
        result = c.opt_classify(b"hello")
        assert result == ord('o')  # max of 'h','e','l','l','o'


def test_opt_classify_empty():
    """opt_classify returns None on empty input."""
    with Rle(LIB_PATH) as c:
        result = c.opt_classify(b"")
        assert result is None


def test_new_dvec():
    """new_dvec allocates an OwnedDVec in the host allocator."""
    with Rle(LIB_PATH) as c:
        dv = c.new_dvec(8)
        assert dv.ptr is not None or dv.len == 0
        assert dv.len == 0
        assert dv.cap >= 8
        del dv


def test_new_dstring():
    """new_dstring allocates a DStringHandle in the host allocator."""
    with Rle(LIB_PATH) as c:
        ds = c.new_dstring("allocated")
        assert ds.as_str() == "allocated"
        assert len(ds) == 9
        del ds


def test_owned_dvec_del_no_leak():
    """OwnedDVec __del__ releases the spier allocation (debug allocator live=0)."""
    with Rle(LIB_PATH, debug=True) as c:
        before = c.allocator_report()
        live_before = before.live_bytes

        dv = c.echo_bytes(b"leak test")
        mid = c.allocator_report()
        assert mid.live_bytes > live_before

        del dv
        after = c.allocator_report()
        assert after.live_bytes == live_before


def test_owned_dstring_del_no_leak():
    """DStringHandle __del__ releases the spier allocation."""
    with Rle(LIB_PATH, debug=True) as c:
        before = c.allocator_report()
        live_before = before.live_bytes

        ds = c.build_string(b"leak test")
        mid = c.allocator_report()
        assert mid.live_bytes > live_before

        del ds
        after = c.allocator_report()
        assert after.live_bytes == live_before


def test_roundtrip_dvec_dstring():
    """Full DType round-trip: owned return → consume → verify no leak."""
    with Rle(LIB_PATH, debug=True) as c:
        before = c.allocator_report()

        dv = c.echo_bytes(b"round-trip")
        assert dv.as_bytes() == b"round-trip"
        n = c.consume_dvec(dv)
        assert n == 10
        del dv

        ds = c.build_string(b"round-trip")
        assert ds.as_str() == "round-trip"
        ns = c.consume_dstring(ds)
        assert ns == 10
        del ds

        after = c.allocator_report()
        assert after.live_bytes == before.live_bytes


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    passed = 0
    failed = 0
    for t in tests:
        try:
            t()
            print(f"  PASS  {t.__name__}")
            passed += 1
        except Exception as e:
            print(f"  FAIL  {t.__name__}: {e}")
            failed += 1
    print(f"\n{passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)
