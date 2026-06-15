from __future__ import annotations

import ctypes
from dataclasses import dataclass, field
from typing import Any

from ._ffi import (
    KIND_NAMES,
    DynSpireIdl,
    EnumDescriptorC,
    EnumVariantDescC,
    IdlMethod,
    IdlParam,
    IdlTypeNode,
    StructDescriptor,
    read_cstring,
)


@dataclass
class TypeInfo:
    kind: int
    size: int
    child0: int
    child1: int

    @property
    def kind_name(self) -> str:
        return KIND_NAMES.get(self.kind, f"?{self.kind}")


@dataclass
class ParamInfo:
    name: str
    type_idx: int


@dataclass
class MethodInfo:
    name: str
    params: list[ParamInfo]
    return_type_idx: int
    index: int


@dataclass
class EnumVariantInfo:
    disc: int
    name: str
    field_types: list[TypeInfo]


@dataclass
class EnumSchema:
    name: str
    variants: list[EnumVariantInfo]
    _variant_map: dict[str, EnumVariantInfo] = field(default_factory=dict, repr=False)
    _disc_map: dict[int, EnumVariantInfo] = field(default_factory=dict, repr=False)

    def __post_init__(self):
        self._variant_map = {v.name: v for v in self.variants}
        self._disc_map = {v.disc: v for v in self.variants}

    def variant(self, name: str) -> EnumVariantInfo:
        return self._variant_map[name]

    def variant_by_disc(self, disc: int) -> EnumVariantInfo:
        return self._disc_map[disc]

    def create_enum_class(self, class_name: str | None = None) -> type[EnumValue]:
        cls_name = class_name or self.name
        attrs: dict[str, Any] = {}
        for v in self.variants:
            def _factory(_variant: str):
                @classmethod
                def _m(cls, *fields: Any) -> EnumValue:
                    return EnumValue(_variant, *fields)
                return _m
            method = v.name.lower()
            attrs[method] = _factory(v.name)
        return type(cls_name, (EnumValue,), attrs)


class EnumValue:
    def __init__(self, variant: str, *fields: Any):
        self.variant = variant
        self.fields = fields

    def __eq__(self, other: object) -> bool:
        if isinstance(other, EnumValue):
            return self.variant == other.variant and self.fields == other.fields
        return False

    def __repr__(self) -> str:
        return f"EnumValue({self.variant!r}, {', '.join(repr(f) for f in self.fields)})"


@dataclass
class SpierSchema:
    name: str
    hash: int
    types: list[TypeInfo]
    methods: list[MethodInfo]
    enums: list[EnumSchema] = field(default_factory=list)
    struct_names: list[str] = field(default_factory=list)
    free_fn: Any = None
    _method_map: dict[str, MethodInfo] = field(default_factory=dict, repr=False)
    _enum_map: dict[int, EnumSchema] = field(default_factory=dict, repr=False)
    _enum_name_map: dict[str, EnumSchema] = field(default_factory=dict, repr=False)

    def __post_init__(self):
        self._method_map = {m.name: m for m in self.methods}
        self._enum_map = {i: e for i, e in enumerate(self.enums)}
        self._enum_name_map = {e.name: e for e in self.enums}

    def method(self, name: str) -> MethodInfo:
        return self._method_map[name]

    def type_at(self, idx: int) -> TypeInfo:
        return self.types[idx]

    def type_str(self, idx: int) -> str:
        if idx < 0 or idx >= len(self.types):
            return "?"
        t = self.types[idx]
        name = KIND_NAMES.get(t.kind, f"?{t.kind}")
        if t.kind == 14 and t.child0 >= 0:
            sname = self.struct_names[t.child0] if t.child0 < len(self.struct_names) else "?"
            return f"Struct<{sname}>"
        parts = []
        if t.child0 >= 0:
            parts.append(self.type_str(t.child0))
        if t.child1 >= 0:
            parts.append(self.type_str(t.child1))
        if parts:
            return f"{name}<{', '.join(parts)}>"
        return name

    def method_sig(self, m: MethodInfo) -> str:
        params = ", ".join(
            f"{p.name}: {self.type_str(p.type_idx)}" for p in m.params
        )
        ret = self.type_str(m.return_type_idx)
        return f"{m.name}({params}) -> Result<{ret}, String>"

    def enum_at(self, idx: int) -> EnumSchema:
        return self._enum_map[idx]

    def enum_by_name(self, name: str) -> EnumSchema:
        return self._enum_name_map[name]

    def struct_name_for_type(self, type_idx: int) -> str | None:
        ti = self.type_at(type_idx)
        if ti.kind != 14 or ti.child0 < 0:
            return None
        if ti.child0 >= len(self.struct_names):
            return None
        return self.struct_names[ti.child0]


def read_schema_types(schema: DynSpireIdl) -> list[TypeInfo]:
    arr = (IdlTypeNode * schema.type_count).from_address(schema.type_table)
    return [TypeInfo(t.kind, t.size, t.child0, t.child1) for t in arr]


def read_schema_methods(schema: DynSpireIdl) -> list[MethodInfo]:
    result = []
    arr = (IdlMethod * schema.method_count).from_address(schema.methods)
    for i, m in enumerate(arr):
        name = read_cstring(m.name, m.name_len)
        params = []
        if m.param_count > 0:
            parr = (IdlParam * m.param_count).from_address(m.params)
            for p in parr:
                pname = read_cstring(p.name, p.name_len)
                params.append(ParamInfo(pname, p.type_idx))
        result.append(MethodInfo(name, params, m.return_type_idx, i))
    return result


def read_schema_enums(schema: DynSpireIdl) -> list[EnumSchema]:
    result: list[EnumSchema] = []
    if schema.enum_count == 0:
        return result
    ptr_arr = (ctypes.c_void_p * schema.enum_count).from_address(schema.enum_table)
    for i in range(schema.enum_count):
        desc_ptr = ptr_arr[i]
        if not desc_ptr:
            continue
        desc = EnumDescriptorC.from_address(desc_ptr)
        name = read_cstring(desc.name, desc.name_len)

        variants: list[EnumVariantInfo] = []
        if desc.variant_count > 0:
            varr = (EnumVariantDescC * desc.variant_count).from_address(desc.variants)
            enum_types = []
            if desc.type_count > 0:
                tarr = (IdlTypeNode * desc.type_count).from_address(desc.type_table)
                enum_types = [TypeInfo(t.kind, t.size, t.child0, t.child1) for t in tarr]
            field_types_flat = []
            if desc.field_type_count > 0:
                farr = (ctypes.c_uint32 * desc.field_type_count).from_address(desc.field_types)
                field_types_flat = list(farr)

            for v in varr:
                vname = read_cstring(v.name, v.name_len)
                start = v.field_type_offset
                ft_indices = field_types_flat[start:start + v.field_count]
                ft_nodes = [enum_types[idx] for idx in ft_indices]
                variants.append(EnumVariantInfo(v.disc, vname, ft_nodes))

        result.append(EnumSchema(name=name, variants=variants))
    return result


def read_schema_structs(schema: DynSpireIdl) -> list[str]:
    result: list[str] = []
    if schema.struct_count == 0:
        return result
    ptr_arr = (ctypes.c_void_p * schema.struct_count).from_address(schema.struct_table)
    for i in range(schema.struct_count):
        desc_ptr = ptr_arr[i]
        if not desc_ptr:
            result.append("")
            continue
        desc = StructDescriptor.from_address(desc_ptr)
        result.append(read_cstring(desc.name, desc.name_len))
    return result


def read_schema(schema: DynSpireIdl) -> SpierSchema:
    types = read_schema_types(schema)
    methods = read_schema_methods(schema)
    enums = read_schema_enums(schema)
    structs = read_schema_structs(schema)
    name = read_cstring(schema.name, schema.name_len) if schema.name_len > 0 else ""
    return SpierSchema(
        name=name, hash=schema.hash, types=types, methods=methods,
        enums=enums, struct_names=structs, free_fn=schema.free_fn,
    )
