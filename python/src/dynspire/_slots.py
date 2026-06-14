from __future__ import annotations

import ctypes
import struct
from typing import Any

from ._ffi import (
    IDL_ARRAY,
    IDL_BOOL,
    IDL_ENUM,
    IDL_OPTION,
    IDL_SLICE,
    IDL_STR,
    IDL_STRING,
    IDL_STRUCT,
    IDL_TUPLE,
    IDL_U8,
    IDL_U32,
    IDL_U64,
    IDL_UNIT,
    IDL_VEC,
    get_vec_sizeof,
    read_rust_vec_u8,
)
from ._schema import EnumValue, MethodInfo, SpierSchema, TypeInfo


class SlotBuilder:
    """Builds u64 slots for request encoding (caller -> spier)."""

    def __init__(self):
        self._slots: list[int] = []
        self._keepalive: list[Any] = []

    def write_u64(self, val: int):
        self._slots.append(val & 0xFFFFFFFFFFFFFFFF)

    def write_borrow(self, data: bytes):
        if not data:
            self.write_u64(0)
            self.write_u64(0)
            return
        arr = (ctypes.c_uint8 * len(data))(*data)
        self._keepalive.append(arr)
        self.write_u64(ctypes.addressof(arr))
        self.write_u64(len(data))

    def slots(self) -> list[int]:
        return self._slots

    @property
    def keepalive(self) -> list[Any]:
        return self._keepalive


class SlotDecoder:
    """Reads u64 slots from response (spier -> caller)."""

    def __init__(self, slots: list[int]):
        self._slots = slots
        self._pos = 0

    def read_u64(self) -> int:
        val = self._slots[self._pos]
        self._pos += 1
        return val

    def read_owned_bytes(self) -> bytes:
        ptr = self.read_u64()
        length = self.read_u64()
        if ptr == 0 or length == 0:
            return b""
        return bytes((ctypes.c_uint8 * length).from_address(ptr))


def encode_request(
    schema: SpierSchema, method: MethodInfo, args: dict[str, Any]
) -> tuple[list[int], list[Any]]:
    b = SlotBuilder()
    for param in method.params:
        val = args[param.name]
        ti = schema.type_at(param.type_idx)
        encode_slot(b, ti, schema, val)
    return b.slots(), b.keepalive


def encode_enum(b: SlotBuilder, ti: TypeInfo, schema: SpierSchema, val: Any):
    if not isinstance(val, EnumValue):
        raise TypeError(
            f"expected EnumValue for enum parameter, got {type(val).__name__}; "
            f"use EnumValue(variant_name, *fields) or EnumSchema.create_enum_class()"
        )
    enum_desc = schema.enum_at(ti.child0)
    vinfo = enum_desc.variant(val.variant)
    b.write_u64(vinfo.disc)
    for i, ft in enumerate(vinfo.field_types):
        field_val = val.fields[i] if i < len(val.fields) else 0
        encode_slot(b, ft, schema, field_val)


def encode_slot(b: SlotBuilder, ti: TypeInfo, schema: SpierSchema, val: Any):
    if ti.kind == IDL_BOOL:
        b.write_u64(1 if val else 0)
    elif ti.kind == IDL_U32:
        b.write_u64(val)
    elif ti.kind == IDL_U64:
        if isinstance(val, float):
            b.write_u64(struct.unpack("<Q", struct.pack("<d", val))[0])
        else:
            b.write_u64(val)
    elif ti.kind == IDL_U8:
        b.write_u64(val & 0xFF)
    elif ti.kind == IDL_STR or ti.kind == IDL_STRING:
        data = val.encode("utf-8") if isinstance(val, str) else bytes(val)
        b.write_borrow(data)
    elif ti.kind == IDL_SLICE:
        child = schema.type_at(ti.child0)
        if child.kind == IDL_U8:
            data = val if isinstance(val, (bytes, bytearray)) else bytes(val)
            b.write_borrow(data)
        else:
            raise ValueError(f"unsupported slice element kind {child.kind}")
    elif ti.kind == IDL_VEC:
        child = schema.type_at(ti.child0)
        if child.kind == IDL_U8:
            data = val if isinstance(val, (bytes, bytearray)) else bytes(val)
            b.write_borrow(data)
        else:
            raise ValueError(f"unsupported vec element kind {child.kind}")
    elif ti.kind == IDL_ARRAY:
        if isinstance(val, (bytes, bytearray)):
            raw = bytes(val[:ti.size]).ljust(ti.size, b"\x00")
        elif isinstance(val, (list, tuple)):
            raw = bytes(v & 0xFF for v in val[:ti.size]).ljust(ti.size, b"\x00")
        else:
            raw = bytes(val)
        if ti.size >= 8:
            lo = struct.unpack_from("<Q", raw, 0)[0]
        else:
            lo = int.from_bytes(raw[:8], "little")
        hi = struct.unpack_from("<Q", raw, 8)[0] if ti.size >= 16 else 0
        b.write_u64(lo)
        b.write_u64(hi)
    elif ti.kind == IDL_UNIT:
        pass
    elif ti.kind == IDL_ENUM:
        encode_enum(b, ti, schema, val)
    elif ti.kind == IDL_STRUCT:
        b.write_u64(val)
    else:
        raise ValueError(f"unsupported input type kind {ti.kind}")


def decode_response(
    slots: list[int], schema: SpierSchema, method: MethodInfo, lib: ctypes.CDLL
) -> Any:
    if not slots:
        return None
    r = SlotDecoder(slots)
    tag = r.read_u64()
    if tag == 1:
        err = r.read_owned_bytes().decode("utf-8", errors="replace")
        raise RuntimeError(f"spier error: {err}")
    ti = schema.type_at(method.return_type_idx)
    return decode_slot(r, ti, schema, lib)


def decode_slot(
    r: SlotDecoder, ti: TypeInfo, schema: SpierSchema, lib: ctypes.CDLL
) -> Any:
    if ti.kind == IDL_UNIT:
        return None
    elif ti.kind == IDL_BOOL:
        return r.read_u64() != 0
    elif ti.kind == IDL_U32:
        return r.read_u64()
    elif ti.kind == IDL_U64:
        return r.read_u64()
    elif ti.kind == IDL_U8:
        return r.read_u64() & 0xFF
    elif ti.kind == IDL_ARRAY:
        lo = r.read_u64().to_bytes(8, "little")
        hi = r.read_u64().to_bytes(8, "little")
        return lo + hi
    elif ti.kind == IDL_STRING:
        return r.read_owned_bytes().decode("utf-8", errors="replace")
    elif ti.kind == IDL_VEC:
        child = schema.type_at(ti.child0)
        if child.kind == IDL_U8:
            return r.read_owned_bytes()
        return _decode_owned_vec(r, child, schema, lib)
    elif ti.kind == IDL_OPTION:
        if r.read_u64() == 0:
            return None
        return decode_slot(r, schema.type_at(ti.child0), schema, lib)
    elif ti.kind == IDL_TUPLE:
        a = decode_slot(r, schema.type_at(ti.child0), schema, lib) if ti.child0 >= 0 else None
        b = decode_slot(r, schema.type_at(ti.child1), schema, lib) if ti.child1 >= 0 else None
        if ti.child0 >= 0 and ti.child1 >= 0:
            return (a, b)
        return a if ti.child0 >= 0 else b
    elif ti.kind == IDL_ENUM:
        disc = r.read_u64()
        enum_desc = schema.enum_at(ti.child0)
        vinfo = enum_desc.variant_by_disc(disc)
        fields = tuple(decode_slot(r, ft, schema, lib) for ft in vinfo.field_types)
        return EnumValue(vinfo.name, *fields)
    elif ti.kind == IDL_STRUCT:
        return r.read_u64()
    raise ValueError(f"unsupported return type kind {ti.kind}")


def _decode_owned_vec(
    r: SlotDecoder, child: TypeInfo, schema: SpierSchema, lib: ctypes.CDLL
) -> list:
    ptr = r.read_u64()
    count = r.read_u64()
    if ptr == 0 or count == 0:
        return []

    vec_size = get_vec_sizeof(lib)

    if child.kind == IDL_STRING or (
        child.kind == IDL_VEC and schema.type_at(child.child0).kind == IDL_U8
    ):
        result = []
        for i in range(count):
            elem_addr = ptr + i * vec_size
            data = read_rust_vec_u8(lib, elem_addr)
            if child.kind == IDL_STRING:
                result.append(data.decode("utf-8", errors="replace"))
            else:
                result.append(data)
        return result

    if child.kind == IDL_ARRAY:
        elem_size = child.size
        raw = bytes((ctypes.c_uint8 * (count * elem_size)).from_address(ptr))
        return [raw[i * elem_size:(i + 1) * elem_size] for i in range(count)]

    if child.kind == IDL_TUPLE:
        c0 = schema.type_at(child.child0) if child.child0 >= 0 else None
        c1 = schema.type_at(child.child1) if child.child1 >= 0 else None

        def is_vec_u8(t: TypeInfo | None) -> bool:
            return t is not None and t.kind == IDL_VEC and schema.type_at(t.child0).kind == IDL_U8

        if is_vec_u8(c0) and is_vec_u8(c1):
            elem_size = 2 * vec_size
            result = []
            for i in range(count):
                base = ptr + i * elem_size
                first = read_rust_vec_u8(lib, base)
                second = read_rust_vec_u8(lib, base + vec_size)
                result.append((first, second))
            return result

    raise ValueError(f"unsupported Vec element kind {child.kind}")
