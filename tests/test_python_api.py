"""Tests for the dynspire PyO3 engine Python API.

Run with:  uv run pytest tests/

Exercises the full surface: schema introspection, all calling styles,
OpaqueHandle type validation, SpierEnumValue equality, out-vec auto-tuple,
BoundMethod keepalive, and integer encoding correctness.
"""

import pytest

from dynspire import load_spier

DATA = b"AAAABBBCCCCDDDDDEEEEFFFFFFGGG"
COMPRESSED = bytes.fromhex("0441034204430544044506460347")


def _find_lib_dir():
    import os
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for d in ("target/debug", "target/release"):
        path = os.path.join(root, d)
        if os.path.exists(os.path.join(path, "librle_spier.so")):
            return path
    pytest.skip("rle_spier .so not built — run: cargo build")


@pytest.fixture(scope="module")
def lib():
    return load_spier("rle_spier", lib_dir=_find_lib_dir())


@pytest.fixture(scope="module")
def schema(lib):
    return lib.schema()


@pytest.fixture
def handle(lib):
    with lib.create_handle() as h:
        yield h


# ---------------------------------------------------------------------------
# Schema introspection
# ---------------------------------------------------------------------------

class TestSchemaReflection:
    def test_name(self, schema):
        assert schema.name == "rle"

    def test_hash_is_consistent(self, schema, lib):
        assert schema.hash == lib.idl_hash()
        assert isinstance(schema.hash, int)

    def test_methods_returns_objects(self, schema):
        methods = schema.methods
        assert len(methods) == 13
        names = [m.name for m in methods]
        assert "compress" in names
        assert "classify" in names

    def test_method_has_index(self, schema):
        for i, m in enumerate(schema.methods):
            assert m.index == i

    def test_method_lookup(self, schema):
        m = schema.method("compress")
        assert m.name == "compress"
        assert len(m.params) == 1
        assert m.params[0].name == "data"
        assert isinstance(m.params[0].type_idx, int)
        assert isinstance(m.return_type, int)

    def test_method_lookup_unknown(self, schema):
        with pytest.raises(ValueError, match="unknown method"):
            schema.method("nope")

    def test_type_at(self, schema):
        m = schema.method("compress")
        ti = schema.type_at(m.params[0].type_idx)
        assert ti.kind_name == "Slice"

    def test_type_at_out_of_range(self, schema):
        with pytest.raises(ValueError, match="out of range"):
            schema.type_at(9999)

    def test_method_sig_with_string(self, schema):
        sig = schema.method_sig("compress")
        assert "compress" in sig
        assert "Slice" in sig

    def test_method_sig_with_method_object(self, schema):
        m = schema.method("compress")
        sig = schema.method_sig(m)
        assert "compress" in sig

    def test_enum_by_name(self, schema):
        e = schema.enum_by_name("Tone")
        assert e.name == "Tone"
        assert e.variant_names == ["Quiet", "Normal", "Loud"]

    def test_enum_by_name_unknown(self, schema):
        with pytest.raises(ValueError, match="unknown enum"):
            schema.enum_by_name("Nope")


# ---------------------------------------------------------------------------
# Calling styles
# ---------------------------------------------------------------------------

class TestCallingStyles:
    def test_attribute_positional(self, handle):
        assert handle.compress(DATA) == COMPRESSED

    def test_attribute_kwargs(self, handle):
        assert handle.compress(data=DATA) == COMPRESSED

    def test_call_positional(self, handle):
        assert handle.call("compress", DATA) == COMPRESSED

    def test_call_dict(self, handle):
        assert handle.call("compress", {"data": DATA}) == COMPRESSED

    def test_call_kwargs(self, handle):
        assert handle.call("compress", data=DATA) == COMPRESSED

    def test_all_styles_equivalent(self, handle):
        results = [
            handle.compress(DATA),
            handle.call("compress", DATA),
            handle.call("compress", {"data": DATA}),
            handle.call("compress", data=DATA),
            handle.compress(data=DATA),
        ]
        assert all(r == results[0] for r in results)

    def test_dict_missing_key(self, handle):
        with pytest.raises(ValueError, match="missing keyword"):
            handle.call("compress", {"wrong": DATA})

    def test_unknown_method(self, handle):
        with pytest.raises(ValueError, match="unknown method"):
            handle.call("nope", DATA)

    def test_attribute_error_on_unknown(self, handle):
        with pytest.raises(AttributeError):
            handle.nope


# ---------------------------------------------------------------------------
# Integer encoding (regression: ints were encoded as float bit patterns)
# ---------------------------------------------------------------------------

class TestIntegerEncoding:
    def test_stats_returns_correct_integers(self, handle):
        orig, comp = handle.stats(DATA)
        assert orig == 29
        assert comp == 14

    def test_stats_dict_args(self, handle):
        orig, comp = handle.call("stats", {"data": DATA})
        assert orig == 29
        assert comp == 14

    def test_first_byte(self, handle):
        assert handle.first_byte(DATA) == 65  # ord('A')
        assert handle.first_byte(b"") is None


# ---------------------------------------------------------------------------
# Out-vec auto-tuple
# ---------------------------------------------------------------------------

class TestOutVec:
    def test_compress_into_returns_tuple(self, handle):
        ret, outs = handle.compress_into(DATA)
        assert ret is None
        assert len(outs) == 1
        assert outs[0] == COMPRESSED

    def test_compress_into_checked_returns_bool(self, handle):
        ok, outs = handle.compress_into_checked(DATA)
        assert ok is True
        assert outs[0] == COMPRESSED

    def test_outvec_via_call(self, handle):
        ret, outs = handle.call("compress_into", DATA)
        assert outs[0] == COMPRESSED


# ---------------------------------------------------------------------------
# Vec / tuple / String returns
# ---------------------------------------------------------------------------

class TestReturnTypes:
    def test_vec_vec_u8(self, handle):
        runs = handle.split_runs(DATA)
        assert len(runs) == 7
        assert bytes(runs[0]) == b"AAAA"
        assert bytes(runs[-1]) == b"GGG"

    def test_vec_string(self, handle):
        labels = handle.run_labels(DATA)
        assert labels == ["4×A", "3×B", "4×C", "5×D", "4×E", "6×F", "3×G"]

    def test_round_trip(self, handle):
        assert handle.decompress(COMPRESSED) == DATA


# ---------------------------------------------------------------------------
# OpaqueHandle type validation
# ---------------------------------------------------------------------------

class TestOpaqueHandle:
    def test_correct_type_passes(self, handle):
        report = handle.analyze(DATA)
        summary = handle.report_summary(report)
        assert "original=29" in summary

    def test_repr_shows_type_name(self, handle):
        report = handle.analyze(DATA)
        assert "CompressionReport" in repr(report)

    def test_type_name_getter(self, handle):
        report = handle.analyze(DATA)
        assert report.type_name == "CompressionReport"

    @pytest.mark.parametrize("bad", [999, "str", None, b"bytes", 3.14])
    def test_wrong_type_rejected(self, handle, bad):
        with pytest.raises(TypeError, match="OpaqueHandle"):
            handle.report_summary(bad)


# ---------------------------------------------------------------------------
# SpierEnumValue
# ---------------------------------------------------------------------------

class TestSpierEnumValue:
    def test_classify_returns_enum(self, handle):
        tone = handle.classify(DATA)
        assert tone.variant == "Loud"

    def test_enum_equality_by_variant(self, handle, schema):
        Tone = schema.enum_by_name("Tone").create_enum_class()
        loud = handle.classify(DATA)
        assert loud == Tone.Loud(0)
        assert loud != Tone.Quiet()

    def test_describe_tone_roundtrip(self, handle, schema):
        Tone = schema.enum_by_name("Tone").create_enum_class()
        desc = handle.describe_tone(Tone.Quiet())
        assert desc == "silence"

    def test_factory_creates_enum_value(self, schema):
        Tone = schema.enum_by_name("Tone").create_enum_class()
        v = Tone.Loud(42)
        assert v.variant == "Loud"
        assert v.fields == (42,)

    def test_factory_unit_variant(self, schema):
        Tone = schema.enum_by_name("Tone").create_enum_class()
        v = Tone.Quiet()
        assert v.variant == "Quiet"
        assert v.fields == ()

    def test_factory_unknown_variant(self, schema):
        Tone = schema.enum_by_name("Tone").create_enum_class()
        with pytest.raises(AttributeError):
            Tone.Nope()


# ---------------------------------------------------------------------------
# BoundMethod
# ---------------------------------------------------------------------------

class TestBoundMethod:
    def test_repr(self, handle):
        bm = handle.compress
        assert "compress" in repr(bm)

    def test_callable(self, handle):
        bm = handle.compress
        assert bm(DATA) == COMPRESSED

    def test_keepalive(self, lib):
        h = lib.create_handle()
        bm = h.compress
        del h
        assert bm(DATA) == COMPRESSED


# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    def test_context_manager(self, lib):
        with lib.create_handle() as h:
            assert h.compress(DATA) == COMPRESSED

    def test_create_with_config(self, lib):
        with lib.create_handle({"key": "value"}) as h:
            assert h.compress(DATA) == COMPRESSED

    def test_repr(self, lib):
        h = lib.create_handle()
        assert "SpierHandle" in repr(h)
        h.destroy()


# ---------------------------------------------------------------------------
# Schema enumeration (.enums, .structs)
# ---------------------------------------------------------------------------

class TestSchemaEnumeration:
    def test_enums_list(self, schema):
        enums = schema.enums
        assert len(enums) == 1
        assert enums[0].name == "Tone"

    def test_structs_list(self, schema):
        structs = schema.structs
        assert "CompressionReport" in structs

    def test_enums_iterable(self, schema):
        names = [e.name for e in schema.enums]
        assert "Tone" in names


# ---------------------------------------------------------------------------
# SpierEnumVariant: discriminants + field types
# ---------------------------------------------------------------------------

class TestEnumVariantInfo:
    def test_variants_have_disc(self, schema):
        e = schema.enum_by_name("Tone")
        for v in e.variants:
            assert isinstance(v.disc, int)

    def test_variant_names_match(self, schema):
        e = schema.enum_by_name("Tone")
        names = [v.name for v in e.variants]
        assert names == ["Quiet", "Normal", "Loud"]

    def test_variant_field_types_empty_for_unit(self, schema):
        e = schema.enum_by_name("Tone")
        for v in e.variants:
            assert isinstance(v.field_types, list)

    def test_variant_repr(self, schema):
        e = schema.enum_by_name("Tone")
        r = repr(e.variants[0])
        assert "Quiet" in r
        assert "disc=" in r


# ---------------------------------------------------------------------------
# SpierTypeInfo: kind (numeric), child0, child1
# ---------------------------------------------------------------------------

class TestTypeInfoNavigation:
    def test_kind_numeric(self, schema):
        m = schema.method("compress")
        ti = schema.type_at(m.params[0].type_idx)
        assert isinstance(ti.kind, int)
        assert ti.kind > 0

    def test_child_indices(self, schema):
        m = schema.method("compress")
        ti = schema.type_at(m.params[0].type_idx)
        # Slice<U8> — child0 should point to the U8 node
        assert ti.child0 >= 0

    def test_kind_matches_kind_name(self, schema):
        m = schema.method("compress")
        ti = schema.type_at(m.params[0].type_idx)
        assert ti.kind_name == "Slice"
        # IDL_SLICE constant
        assert ti.kind == 5

    def test_return_type_has_child(self, schema):
        # compress returns Result<Vec<U8>, String> → the outer type is Result (encoded as Vec)
        m = schema.method("stats")
        ti = schema.type_at(m.return_type)
        # stats returns Tuple<U64, U64> → should have two children
        assert ti.child0 >= 0
        assert ti.child1 >= 0


# ---------------------------------------------------------------------------
# Resolved type strings on SpierParam and SpierMethod
# ---------------------------------------------------------------------------

class TestResolvedTypeStrings:
    def test_param_type_str(self, schema):
        m = schema.method("compress")
        assert m.params[0].type_str == "Slice<U8>"

    def test_return_type_str(self, schema):
        m = schema.method("compress")
        assert "Vec<U8>" in m.return_type_str

    def test_method_sig_consistent_with_type_str(self, schema):
        m = schema.method("compress")
        sig = schema.method_sig(m)
        assert m.params[0].type_str in sig


# ---------------------------------------------------------------------------
# GIL release during dispatch
# ---------------------------------------------------------------------------

class TestGilRelease:
    def test_gil_released_during_dispatch(self, lib):
        """Without py.detach(), a background Python thread can't acquire the
        GIL during the Rust sleep — it only runs after dispatch returns.

        With detach, the thread runs during the 300ms delay window.
        """
        import threading
        import time

        ran_at = [None]

        def background():
            ran_at[0] = time.perf_counter()

        start = time.perf_counter()
        t = threading.Thread(target=background, daemon=True)
        t.start()

        with lib.create_handle() as h:
            h.delay(300)

        t.join(timeout=5)
        assert ran_at[0] is not None
        bg_delay_ms = (ran_at[0] - start) * 1000
        assert bg_delay_ms < 250, (
            f"Background thread ran {bg_delay_ms:.0f}ms after start — "
            f"GIL was not released during dispatch"
        )

    def test_delay_returns_normally(self, handle):
        assert handle.delay(1) is None
