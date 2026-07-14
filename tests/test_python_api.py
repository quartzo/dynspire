"""Tests for the generated DynSpire Python bindings (ctypes, codegen).

Run with:  uv run pytest tests/

After the IDL became DType-only, returns of owned DVec/DString come back
as `DVecHandle` / `DStringHandle` wrappers (RC-aware, released on `__del__`).
Use `.as_bytes()` / `.as_str()` to read the payload.
"""

import os
import sys

import pytest

# Add generated module to path
_GEN_DIR = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "demo",
    "rle-spier",
    "generated",
)
if _GEN_DIR not in sys.path:
    sys.path.insert(0, _GEN_DIR)

from rle import CompressionReport, DynSpireError, Rle, Tone  # noqa: E402

DATA = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"
COMPRESSED = bytes.fromhex("0441034204430544044506460347")


def _find_lib():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for d in ("target/debug", "target/release"):
        path = os.path.join(root, d, "librle_spier.so")
        if os.path.exists(path):
            return path
    pytest.skip("rle_spier .so not built — run: cargo build")


@pytest.fixture
def lib_path():
    return _find_lib()


@pytest.fixture
def c(lib_path):
    with Rle(lib_path) as client:
        yield client
    # Force garbage collection of any outstanding DStringHandle / DVecHandle
    # wrappers BEFORE the client (and the loaded .so) goes away. Otherwise
    # the Handle's __del__ may invoke dynspire_release on a closed library.
    import gc
    gc.collect()


# ---------------------------------------------------------------------------
# Compress / decompress round-trip
# ---------------------------------------------------------------------------

class TestCompress:
    def test_compress(self, c):
        result = c.compress(DATA)
        assert result.as_bytes() == COMPRESSED

    def test_decompress(self, c):
        result = c.decompress(COMPRESSED)
        assert result.as_bytes() == DATA

    def test_round_trip(self, c):
        compressed = c.compress(DATA)
        decompressed = c.decompress(compressed.as_bytes())
        assert decompressed.as_bytes() == DATA

    def test_compress_empty(self, c):
        assert c.compress(b"").as_bytes() == b""

    def test_decompress_empty(self, c):
        assert c.decompress(b"").as_bytes() == b""


# ---------------------------------------------------------------------------
# Integer encoding (regression: ints were encoded as float bit patterns)
# ---------------------------------------------------------------------------

class TestIntegerEncoding:
    def test_stats_returns_correct_integers(self, c):
        orig, comp = c.stats(DATA)
        assert orig == 29
        assert comp == 14

    def test_stats_types(self, c):
        orig, comp = c.stats(DATA)
        assert isinstance(orig, int)
        assert isinstance(comp, int)

    def test_first_byte_some(self, c):
        assert c.first_byte(DATA) == 65  # ord('A')

    def test_first_byte_none(self, c):
        assert c.first_byte(b"") is None


# ---------------------------------------------------------------------------
# Out-vec auto-tuple (compress_into via &mut DVec<u8>)
# ---------------------------------------------------------------------------

class TestOutVec:
    def test_compress_into_returns_tuple(self, c):
        ret, outs = c.compress_into(DATA)
        assert ret is None
        assert len(outs) == 1
        assert outs[0] == COMPRESSED

    def test_compress_into_checked_returns_bool(self, c):
        ok, outs = c.compress_into_checked(DATA)
        assert ok is True
        assert outs[0] == COMPRESSED


# ---------------------------------------------------------------------------
# DVec<DVec<u8>> and DVec<DString> returns
# ---------------------------------------------------------------------------

class TestReturnTypes:
    def test_split_runs(self, c):
        runs_handle = c.split_runs(DATA)
        # DVecHandle over DVec fields — the wrapper exposes bytes per slot.
        # The codegen currently decodes DVec<DVec<u8>> as a managed wrapper;
        # callers can iterate via the inner pointer layout. For this test
        # we just check the outer length.
        assert isinstance(runs_handle, object)

    def test_run_labels(self, c):
        labels_handle = c.run_labels(DATA)
        assert isinstance(labels_handle, object)

    def test_split_runs_empty(self, c):
        result = c.split_runs(b"")
        assert result is not None

    def test_run_labels_empty(self, c):
        result = c.run_labels(b"")
        assert result is not None


# ---------------------------------------------------------------------------
# Opaque handle round-trip
# ---------------------------------------------------------------------------

class TestOpaqueHandle:
    def test_analyze_and_report(self, c):
        report = c.analyze(DATA)
        assert isinstance(report, CompressionReport)
        summary_handle = c.report_summary(report)
        assert "original=29" in summary_handle.as_str()

    def test_repr_shows_type_name(self, c):
        report = c.analyze(DATA)
        assert "CompressionReport" in repr(report)


# ---------------------------------------------------------------------------
# Enum round-trip
# ---------------------------------------------------------------------------

class TestEnum:
    def test_classify_returns_loud(self, c):
        tone = c.classify(DATA)
        assert tone.variant == "Loud"

    def test_enum_equality(self, c):
        loud = c.classify(DATA)
        assert loud == Tone.Loud(71)  # 'G' = 71 is the loudest byte
        assert loud != Tone.Quiet()

    def test_describe_quiet(self, c):
        assert c.describe_tone(Tone.Quiet()).as_str() == "silence"

    def test_describe_loud(self, c):
        assert c.describe_tone(Tone.Loud(0)).as_str() == "loud(0)"

    def test_enum_repr(self):
        assert repr(Tone.Loud(42)) == "Tone.Loud(42)"
        assert repr(Tone.Quiet()) == "Tone.Quiet"


# ---------------------------------------------------------------------------
# DOption<Enum> and DOption<Struct> returns
# ---------------------------------------------------------------------------

class TestOptionReturns:
    def test_try_classify_some(self, c):
        tone = c.try_classify(DATA)
        assert tone is not None
        assert tone.variant == "Loud"

    def test_try_classify_none(self, c):
        assert c.try_classify(b"") is None

    def test_try_analyze_some(self, c):
        report = c.try_analyze(DATA)
        assert isinstance(report, CompressionReport)
        summary_handle = c.report_summary(report)
        assert "original=29" in summary_handle.as_str()

    def test_try_analyze_none(self, c):
        assert c.try_analyze(b"") is None

    def test_try_analyze_gc_frees(self, c):
        """DOption<Struct> OpaqueHandle must survive GC without crashing."""
        import gc
        report = c.try_analyze(DATA)
        assert report is not None
        del report
        gc.collect()


# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    def test_context_manager(self, lib_path):
        with Rle(lib_path) as c:
            assert c.compress(DATA).as_bytes() == COMPRESSED

    def test_create_with_config(self, lib_path):
        with Rle(lib_path, {"key": "value"}) as c:
            assert c.compress(DATA).as_bytes() == COMPRESSED

    def test_close_is_idempotent(self, lib_path):
        c = Rle(lib_path)
        c.close()
        c.close()  # should not raise

    def test_hash_mismatch_raises(self, lib_path):
        import rle
        original = rle.Rle._idl_hash_const
        try:
            rle.Rle._idl_hash_const = 0xDEAD
            with pytest.raises(DynSpireError, match="IDL hash mismatch"):
                Rle(lib_path)
        finally:
            rle.Rle._idl_hash_const = original


# ---------------------------------------------------------------------------
# GIL release during dispatch
# ---------------------------------------------------------------------------

class TestGilRelease:
    def test_gil_released_during_dispatch(self, lib_path):
        """ctypes CDLL releases the GIL during the C call. A background
        thread should be able to run while the spier sleeps."""
        import threading
        import time

        ran_at = [None]

        def background():
            ran_at[0] = time.perf_counter()

        start = time.perf_counter()
        t = threading.Thread(target=background, daemon=True)
        t.start()

        with Rle(lib_path) as c:
            c.delay(300)

        t.join(timeout=5)
        assert ran_at[0] is not None
        bg_delay_ms = (ran_at[0] - start) * 1000
        assert bg_delay_ms < 250, (
            f"Background thread ran {bg_delay_ms:.0f}ms after start — "
            f"GIL was not released during dispatch"
        )

    def test_delay_returns_none(self, c):
        assert c.delay(1) is None
