from __future__ import annotations

import ctypes

MAX_OUT_SLOTS = 8

IDL_UNIT = 0
IDL_U8 = 1
IDL_U32 = 2
IDL_U64 = 3
IDL_ARRAY = 4
IDL_SLICE = 5
IDL_STR = 6
IDL_VEC = 7
IDL_OPTION = 8
IDL_TUPLE = 9
IDL_STRING = 10
IDL_BOOL = 11
IDL_OUT_VEC = 12
IDL_ENUM = 13
IDL_STRUCT = 14

KIND_NAMES = {
    IDL_UNIT: "Unit", IDL_U8: "U8", IDL_U32: "U32", IDL_U64: "U64",
    IDL_ARRAY: "Array", IDL_SLICE: "Slice", IDL_STR: "Str",
    IDL_VEC: "Vec", IDL_OPTION: "Option", IDL_TUPLE: "Tuple",
    IDL_STRING: "String", IDL_BOOL: "Bool", IDL_OUT_VEC: "OutVec",
    IDL_ENUM: "Enum", IDL_STRUCT: "Struct",
}


class IdlTypeNode(ctypes.Structure):
    _fields_ = [
        ("kind", ctypes.c_uint8),
        ("_pad", ctypes.c_uint8 * 3),
        ("size", ctypes.c_uint32),
        ("child0", ctypes.c_int32),
        ("child1", ctypes.c_int32),
    ]


class IdlParam(ctypes.Structure):
    _fields_ = [
        ("name", ctypes.c_void_p),
        ("name_len", ctypes.c_size_t),
        ("type_idx", ctypes.c_uint32),
    ]


class IdlMethod(ctypes.Structure):
    _fields_ = [
        ("name", ctypes.c_void_p),
        ("name_len", ctypes.c_size_t),
        ("params", ctypes.c_void_p),
        ("param_count", ctypes.c_size_t),
        ("return_type_idx", ctypes.c_uint32),
        ("_pad", ctypes.c_uint8 * 4),
    ]


class EnumVariantDescC(ctypes.Structure):
    _fields_ = [
        ("disc", ctypes.c_uint32),
        ("name", ctypes.c_void_p),
        ("name_len", ctypes.c_size_t),
        ("field_count", ctypes.c_uint32),
        ("field_type_offset", ctypes.c_uint32),
    ]


class EnumDescriptorC(ctypes.Structure):
    _fields_ = [
        ("name", ctypes.c_void_p),
        ("name_len", ctypes.c_size_t),
        ("variant_count", ctypes.c_size_t),
        ("variants", ctypes.c_void_p),
        ("type_table", ctypes.c_void_p),
        ("type_count", ctypes.c_size_t),
        ("field_types", ctypes.c_void_p),
        ("field_type_count", ctypes.c_size_t),
    ]


FREE_FN = ctypes.CFUNCTYPE(None, ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint64), ctypes.c_size_t)


class StructDescriptor(ctypes.Structure):
    _fields_ = [
        ("name", ctypes.c_void_p),
        ("name_len", ctypes.c_size_t),
    ]


class DynSpireIdl(ctypes.Structure):
    _fields_ = [
        ("name", ctypes.c_void_p),
        ("name_len", ctypes.c_size_t),
        ("hash", ctypes.c_uint64),
        ("type_table", ctypes.c_void_p),
        ("type_count", ctypes.c_size_t),
        ("methods", ctypes.c_void_p),
        ("method_count", ctypes.c_size_t),
        ("enum_table", ctypes.c_void_p),
        ("enum_count", ctypes.c_size_t),
        ("struct_table", ctypes.c_void_p),
        ("struct_count", ctypes.c_size_t),
        ("free_fn", FREE_FN),
    ]


class VecView(ctypes.Structure):
    _fields_ = [
        ("ptr", ctypes.c_void_p),
        ("len", ctypes.c_size_t),
    ]


def read_cstring(ptr: int, length: int) -> str:
    buf = (ctypes.c_char * length).from_address(ptr)
    return bytes(buf).decode("utf-8", errors="replace")


def serialize_kvmap(config: dict[str, str]) -> bytes:
    from urllib.parse import urlencode
    return urlencode(config).encode("utf-8")


def get_vec_sizeof(lib: ctypes.CDLL) -> int:
    return lib.dynspire_vec_u8_sizeof()


def read_rust_vec_u8(lib: ctypes.CDLL, elem_addr: int) -> bytes:
    view = lib.dynspire_vec_view(elem_addr)
    if not view.ptr or view.len == 0:
        return b""
    return bytes((ctypes.c_uint8 * view.len).from_address(view.ptr))
