"""DynSpire — pure-Python ctypes runtime for spiers.

Per-interface bindings are produced by ``dynspire-codegen`` (``generate_python``)
and import the slot primitives from :mod:`dynspire._runtime`.
"""

from __future__ import annotations

from ._runtime import (
    DynSpireError,
    OpaqueHandle,
    OutReader,
    SlotWriter,
    SpierClient,
    new_out_array,
)

__all__ = [
    "DynSpireError",
    "OpaqueHandle",
    "OutReader",
    "SlotWriter",
    "SpierClient",
    "new_out_array",
]

__version__ = "0.3.0"
