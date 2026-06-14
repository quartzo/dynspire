from __future__ import annotations

import ctypes
import os
from typing import Any

from ._ffi import IDL_OUT_VEC, IDL_UNIT, MAX_OUT_SLOTS, DynSpireIdl, VecView, serialize_kvmap
from ._schema import MethodInfo, SpierSchema, read_schema
from ._slots import SlotBuilder, decode_response, encode_request, encode_slot


class SpierHandle:
    def __init__(self, lib: ctypes.CDLL, handle: int, schema: SpierSchema):
        self._lib = lib
        self._handle = ctypes.c_void_p(handle)
        self._schema = schema

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.destroy()

    def __del__(self):
        try:
            self.destroy()
        except Exception:
            pass

    def dispatch(self, method_name: str, in_slots: list[int] | None = None) -> list[int]:
        fn_name = f"dynspire_dispatch_{method_name}"
        fn = getattr(self._lib, fn_name)
        fn.restype = ctypes.c_uint8
        fn.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_uint64), ctypes.c_size_t,
            ctypes.POINTER(ctypes.c_uint64), ctypes.c_size_t,
        ]

        in_arr = (ctypes.c_uint64 * len(in_slots))(*in_slots) if in_slots else None
        in_count = len(in_slots) if in_slots else 0
        out_arr = (ctypes.c_uint64 * MAX_OUT_SLOTS)()

        ret = fn(self._handle, in_arr, in_count, out_arr, MAX_OUT_SLOTS)
        if ret != 0:
            raise RuntimeError(f"dispatch transport error (code {ret})")

        return list(out_arr)

    def call(self, method_name: str, *args: Any) -> Any:
        schema = self._schema
        m = schema.method(method_name)

        if len(args) == 1 and isinstance(args[0], dict):
            args_dict = args[0]
        elif len(args) == 0:
            args_dict = {}
        else:
            input_params = [
                p for p in m.params
                if schema.type_at(p.type_idx).kind != IDL_OUT_VEC
            ]
            if len(args) != len(input_params):
                raise ValueError(
                    f"{method_name} expects {len(input_params)} args, got {len(args)}"
                )
            args_dict = {p.name: v for p, v in zip(input_params, args)}

        has_out_vec = any(
            schema.type_at(p.type_idx).kind == IDL_OUT_VEC for p in m.params
        )

        if has_out_vec:
            return self._call_out_vec(m, args_dict)

        in_slots, keepalive = encode_request(schema, m, args_dict)
        resp_slots = self.dispatch(method_name, in_slots)
        del keepalive
        return decode_response(resp_slots, schema, m, self._lib)

    def _call_out_vec(self, method: MethodInfo, args: dict[str, Any]) -> Any:
        schema = self._schema
        lib = self._lib

        vec_ptrs: list[int] = []
        b = SlotBuilder()
        for param in method.params:
            ti = schema.type_at(param.type_idx)
            if ti.kind == IDL_OUT_VEC:
                vp = lib.dynspire_vec_create()
                vec_ptrs.append(vp)
                b.write_u64(vp)
            else:
                val = args.get(param.name)
                encode_slot(b, ti, schema, val)

        resp_slots = self.dispatch(method.name, b.slots())

        try:
            ret_val = decode_response(resp_slots, schema, method, lib)

            vp = vec_ptrs[0]
            view = lib.dynspire_vec_view(vp)
            if view.ptr and view.len > 0:
                out_data = bytes((ctypes.c_uint8 * view.len).from_address(view.ptr))
            else:
                out_data = b""

            ret_ti = schema.type_at(method.return_type_idx)
            if ret_ti.kind == IDL_UNIT:
                return out_data
            return ret_val
        finally:
            for vp in vec_ptrs:
                lib.dynspire_vec_free(vp)

    def destroy(self):
        if self._handle:
            self._lib.dynspire_destroy(self._handle)
            self._handle = None


class SpierLib:
    def __init__(self, so_path: str):
        self._lib = ctypes.CDLL(so_path)
        self._schema: SpierSchema | None = None
        self._bind_vec_helpers()

    def _bind_vec_helpers(self):
        self._lib.dynspire_vec_create.restype = ctypes.c_void_p
        self._lib.dynspire_vec_create.argtypes = []
        self._lib.dynspire_vec_view.restype = VecView
        self._lib.dynspire_vec_view.argtypes = [ctypes.c_void_p]
        self._lib.dynspire_vec_free.restype = None
        self._lib.dynspire_vec_free.argtypes = [ctypes.c_void_p]
        self._lib.dynspire_vec_u8_sizeof.restype = ctypes.c_size_t
        self._lib.dynspire_vec_u8_sizeof.argtypes = []

    @property
    def lib(self) -> ctypes.CDLL:
        return self._lib

    def idl_hash(self) -> int:
        fn = self._lib.dynspire_idl_hash
        fn.restype = ctypes.c_uint64
        return fn()

    def spier_name(self) -> str:
        fn = self._lib.dynspire_spier_name
        fn.restype = ctypes.c_void_p
        ptr = fn()
        if not ptr:
            return ""
        buf = (ctypes.c_char * 256).from_address(ptr)
        return bytes(buf).split(b"\x00")[0].decode("utf-8", errors="replace")

    def schema(self) -> SpierSchema:
        if self._schema:
            return self._schema

        fn = self._lib.dynspire_idl_schema
        fn.restype = ctypes.POINTER(DynSpireIdl)
        ptr = fn()

        self._schema = read_schema(ptr.contents)
        return self._schema

    def create_handle(self, config: dict[str, str] | None = None) -> SpierHandle:
        fn = self._lib.dynspire_create
        fn.restype = ctypes.c_void_p
        fn.argtypes = [ctypes.c_char_p, ctypes.c_size_t]
        buf = serialize_kvmap(config or {})
        handle = fn(buf, len(buf))
        if not handle:
            raise RuntimeError("spier create returned null")
        return SpierHandle(self._lib, handle, self.schema())


def load_spier(name: str, lib_dir: str | None = None) -> SpierLib:
    so_name = f"lib{name}.so"

    search_dir = lib_dir or os.environ.get("DYNSPIRE_LIB_DIR")
    if search_dir:
        path = os.path.join(search_dir, so_name)
        if not os.path.exists(path):
            raise FileNotFoundError(f"spier .so not found: {path}")
        return SpierLib(path)

    return SpierLib(so_name)
