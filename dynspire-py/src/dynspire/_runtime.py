"""Pure-Python ctypes runtime for DynSpire spiers.

Consumed by code-generated per-interface modules (produced by
``dynspire-codegen``'s ``generate_python``). The generated module subclasses
:class:`SpierClient` and calls the slot primitives here; it never touches
ctypes or raw pointers directly.

The ABI symbols resolved from the spier ``.so``:

  - ``dynspire_create`` / ``dynspire_destroy``     handle lifecycle
  - ``dynspire_idl_hash``                          compatibility check
  - ``dynspire_spier_name``                        introspection (optional)
  - ``dynspire_dispatch_{method}``                 per-method dispatch
  - ``dynspire_free``                              release owned returns
  - ``dynspire_vec_create`` / ``_view`` / ``_free`` out-vec (``&mut Vec<u8>``)
  - ``dynspire_vec_view_at``                        element access for nested vecs
"""

from __future__ import annotations

import ctypes
import struct

MAX_OUT_SLOTS = 8


class DynSpireError(RuntimeError):
    """Raised when a spier method returns ``Err`` or dispatch fails."""


class _VecView(ctypes.Structure):
    _fields_ = [("ptr", ctypes.c_void_p), ("len", ctypes.c_size_t)]


# ---------------------------------------------------------------------------
# SlotWriter — grows the input u64 stream and pins borrowed buffers alive
# ---------------------------------------------------------------------------


class SlotWriter:
    def __init__(self):
        self._vals: list[int] = []
        self._keepalive: list[object] = []

    def write_u64(self, v: int) -> None:
        self._vals.append(v & 0xFFFFFFFFFFFFFFFF)

    # --- primitives ---
    def write_bool(self, b) -> None:
        self.write_u64(1 if b else 0)

    def write_u8(self, v: int) -> None:
        self.write_u64(v)

    def write_u16(self, v: int) -> None:
        self.write_u64(v)

    def write_u32(self, v: int) -> None:
        self.write_u64(v)

    def write_i8(self, v: int) -> None:
        self.write_u64(v & 0xFF)

    def write_i16(self, v: int) -> None:
        self.write_u64(v & 0xFFFF)

    def write_i32(self, v: int) -> None:
        self.write_u64(v & 0xFFFFFFFF)

    def write_i64(self, v: int) -> None:
        self.write_u64(v & 0xFFFFFFFFFFFFFFFF)

    def write_f32(self, v: float) -> None:
        self.write_u64(struct.unpack("<I", struct.pack("<f", v))[0])

    def write_f64(self, v: float) -> None:
        self.write_u64(struct.unpack("<Q", struct.pack("<d", v))[0])

    # --- borrowed buffers (ptr, len) ---
    def write_bytes(self, data) -> None:
        ptr, n = self._pin_bytes(data)
        self.write_u64(ptr)
        self.write_u64(n)

    def write_str(self, s) -> None:
        if isinstance(s, str):
            self.write_bytes(s.encode("utf-8"))
        else:
            self.write_bytes(s)

    def write_opaque(self, handle) -> None:
        self.write_u64(handle._ptr)

    def _pin_bytes(self, data):
        if not isinstance(data, (bytes, bytearray)):
            raise TypeError(f"expected bytes, got {type(data).__name__}")
        n = len(data)
        if n == 0:
            return 0, 0
        arr = (ctypes.c_uint8 * n).from_buffer_copy(bytes(data))
        self._keepalive.append(arr)
        return ctypes.addressof(arr), n

    def array(self):
        if not self._vals:
            return None
        return (ctypes.c_uint64 * len(self._vals))(*self._vals)

    def __len__(self):
        return len(self._vals)


# ---------------------------------------------------------------------------
# OutReader — decodes the output u64 stream produced by a dispatch call
# ---------------------------------------------------------------------------


class OutReader:
    def __init__(self, arr, client=None):
        self._arr = arr
        self._pos = 0
        self._client = client

    def read(self) -> int:
        v = self._arr[self._pos]
        self._pos += 1
        return v

    def pos(self) -> int:
        return self._pos

    # --- primitives ---
    def read_bool(self) -> bool:
        return self.read() != 0

    def read_u8(self) -> int:
        return self.read() & 0xFF

    def read_u16(self) -> int:
        return self.read() & 0xFFFF

    def read_u32(self) -> int:
        return self.read() & 0xFFFFFFFF

    def read_u64(self) -> int:
        return self.read()

    def read_i8(self) -> int:
        v = self.read() & 0xFF
        return v - 0x100 if v >= 0x80 else v

    def read_i16(self) -> int:
        v = self.read() & 0xFFFF
        return v - 0x10000 if v >= 0x8000 else v

    def read_i32(self) -> int:
        v = self.read() & 0xFFFFFFFF
        return v - 0x100000000 if v >= 0x80000000 else v

    def read_i64(self) -> int:
        v = self.read()
        return v - 0x10000000000000000 if v >= 0x8000000000000000 else v

    def read_f32(self) -> float:
        return struct.unpack("<f", struct.pack("<I", self.read() & 0xFFFFFFFF))[0]

    def read_f64(self) -> float:
        return struct.unpack("<d", struct.pack("<Q", self.read()))[0]

    # --- owned returns (caller must free via SpierClient.free_owned) ---
    def read_string(self) -> str:
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return ""
        return ctypes.string_at(ptr, n).decode("utf-8", "replace")

    def read_bytes(self) -> bytes:
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return b""
        return ctypes.string_at(ptr, n)

    def read_vec_string(self) -> list[str]:
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return []
        out: list[str] = []
        for i in range(n):
            view = self._client._vec_view_at_fn(ptr, i)
            out.append("" if not view.len else ctypes.string_at(view.ptr, view.len).decode("utf-8", "replace"))
        return out

    def read_vec_bytes(self) -> list[bytes]:
        ptr, n = self.read(), self.read()
        if not ptr or not n:
            return []
        out: list[bytes] = []
        for i in range(n):
            view = self._client._vec_view_at_fn(ptr, i)
            out.append(b"" if not view.len else ctypes.string_at(view.ptr, view.len))
        return out


def new_out_array():
    return (ctypes.c_uint64 * MAX_OUT_SLOTS)()


# ---------------------------------------------------------------------------
# OpaqueHandle — a boxed struct pointer returned by a spier
# ---------------------------------------------------------------------------


class OpaqueHandle:
    """Wraps a ``Box::into_raw`` pointer produced by the spier.

    The pointer is freed through ``dynspire_free`` when the handle is GC'd, so
    callers must keep a reference for as long as the spier might read it.
    """

    __slots__ = ("_client", "_ptr", "_free_idx", "__weakref__")

    def __init__(self, client: "SpierClient", ptr: int, free_idx: int):
        self._client = client
        self._ptr = ptr
        self._free_idx = free_idx

    @property
    def type_name(self) -> str:
        return type(self).__name__

    def __repr__(self) -> str:
        return f"{type(self).__name__}(ptr=0x{self._ptr:x})"

    def __del__(self):
        try:
            self._client._free_opaque(self._free_idx, self._ptr)
        except Exception:
            pass


# ---------------------------------------------------------------------------
# SpierClient — base class for generated per-interface clients
# ---------------------------------------------------------------------------


class SpierClient:
    _spier_name: str | None = None
    _idl_hash_const: int = 0

    def __init__(self, lib_path: str, config: dict[str, str] | None = None):
        self._lib = ctypes.CDLL(lib_path)
        self._configure_symbols()
        actual = self._idl_hash_fn()
        expected = self._idl_hash_const
        if actual != expected:
            raise DynSpireError(
                f"IDL hash mismatch: host expects 0x{expected:016x}, "
                f"spier '{self._spier_name}' has 0x{actual:016x}"
            )
        cfg = _encode_config(config)
        self._handle = self._create_fn(cfg, len(cfg))
        if not self._handle:
            raise DynSpireError(f"spier '{self._spier_name}' create failed")
        self._closed = False
        self._dispatch_cache: dict[str, object] = {}

    # --- symbol resolution ---
    def _configure_symbols(self) -> None:
        f = self._lib.dynspire_create
        f.restype = ctypes.c_void_p
        f.argtypes = [ctypes.c_char_p, ctypes.c_size_t]
        self._create_fn = f

        f = self._lib.dynspire_destroy
        f.argtypes = [ctypes.c_void_p]
        self._destroy_fn = f

        f = self._lib.dynspire_idl_hash
        f.restype = ctypes.c_uint64
        self._idl_hash_fn = f

        f = self._lib.dynspire_free
        f.argtypes = [ctypes.c_uint32, ctypes.c_void_p, ctypes.c_size_t]
        f.restype = None
        self._free_fn = f

        f = self._lib.dynspire_vec_create
        f.restype = ctypes.c_void_p
        self._vec_create_fn = f

        f = self._lib.dynspire_vec_view
        f.restype = _VecView
        f.argtypes = [ctypes.c_void_p]
        self._vec_view_fn = f

        f = self._lib.dynspire_vec_free
        f.argtypes = [ctypes.c_void_p]
        self._vec_free_fn = f

        f = self._lib.dynspire_vec_view_at
        f.restype = _VecView
        f.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
        self._vec_view_at_fn = f

    # --- dispatch ---
    def _dispatch_fn(self, method: str):
        fn = self._dispatch_cache.get(method)
        if fn is None:
            fn = getattr(self._lib, "dynspire_dispatch_" + method)
            fn.restype = ctypes.c_uint8
            fn.argtypes = [
                ctypes.c_void_p,
                ctypes.c_void_p,
                ctypes.c_size_t,
                ctypes.POINTER(ctypes.c_uint64),
                ctypes.c_size_t,
            ]
            self._dispatch_cache[method] = fn
        return fn

    def _dispatch(self, method: str, writer: SlotWriter, out) -> None:
        in_arr = writer.array()
        in_ptr = ctypes.cast(in_arr, ctypes.c_void_p) if in_arr is not None else None
        status = self._dispatch_fn(method)(self._handle, in_ptr, len(writer), out, MAX_OUT_SLOTS)
        if status == 2:
            raise DynSpireError(f"out-slot overflow dispatching '{method}'")
        if status != 0:
            raise DynSpireError(f"dispatch '{method}' failed (status {status})")

    # --- owned-return freeing ---
    def free_owned(self, type_idx: int, out, count: int) -> None:
        self._free_fn(type_idx, ctypes.cast(out, ctypes.c_void_p), count)

    def _free_opaque(self, free_idx: int, ptr: int) -> None:
        if ptr == 0:
            return
        slots = (ctypes.c_uint64 * 2)(0, ptr)
        self._free_fn(free_idx, slots, 2)

    # --- out-vec (&mut Vec<u8>) lifecycle ---
    def _new_outvec(self) -> int:
        return self._vec_create_fn()

    def _read_outvec(self, addr: int) -> bytes:
        view = self._vec_view_fn(addr)
        data = ctypes.string_at(view.ptr, view.len) if view.len else b""
        self._vec_free_fn(addr)
        return data

    # --- lifecycle ---
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def close(self) -> None:
        if getattr(self, "_closed", True):
            return
        self._closed = True
        h = getattr(self, "_handle", None)
        if h:
            self._handle = None
            self._destroy_fn(h)

    def __del__(self):
        self.close()


def _encode_config(config: dict[str, str] | None) -> bytes:
    if not config:
        return b""
    import urllib.parse

    return urllib.parse.urlencode(config).encode("utf-8")
