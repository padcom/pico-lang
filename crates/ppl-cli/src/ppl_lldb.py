# lldb formatters for ppl programs (spec §14.5, PLAN P10.8c): strings as text, arrays with
# their elements, maps and sets with their entries. `ppl dap` loads this into lldb-dap; with
# plain lldb: `command script import <this file>`.
#
# The layouts are the runtime's (runtime/include/ppl_rt.h) as debug info describes them:
# `string` is `PplStr` (`byte_len` at 0, the UTF-8 bytes at 32), `T[]` is `PplArray` (`len`,
# `cap`, `data`), `Map<K, V>` and `Set<T>` are the core library's classes (stdlib/core/map.ppl).

import lldb

MAX_CHILDREN = 10000


def _target(v):
    # (The raw value: with a synthetic provider, members by name are its children.)
    v = v.GetNonSyntheticValue()
    if v.TypeIsPointerType():
        v = v.Dereference().GetNonSyntheticValue()
    return v


def _read_string(process, ptr):
    if ptr == 0:
        return None
    err = lldb.SBError()
    n = process.ReadUnsignedFromMemory(ptr, 8, err)
    if not err.Success() or n > (1 << 24):
        return None
    if n == 0:
        return ""
    data = process.ReadMemory(ptr + 32, n, err)
    if not err.Success():
        return None
    return data.decode("utf-8", "replace")


def _quote(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'


def string_summary(valobj, _dict):
    ptr = valobj.GetValueAsUnsigned(0) if valobj.TypeIsPointerType() else valobj.GetLoadAddress()
    s = _read_string(valobj.GetProcess(), ptr)
    return "null" if s is None else _quote(s)


def _null(valobj):
    v = valobj.GetNonSyntheticValue()
    return v.TypeIsPointerType() and v.GetValueAsUnsigned(0) == 0


def _field(v, name):
    return _target(v).GetChildMemberWithName(name)


class ArrayProvider:
    """`T[]`: its elements as `[0]`, `[1]`, …"""

    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        self.len = 0
        self.size = 0
        if _null(self.v):
            return False
        self.len = min(_field(self.v, "len").GetValueAsUnsigned(0), MAX_CHILDREN)
        self.data = _field(self.v, "data")
        self.elem = self.data.GetType().GetPointeeType()
        self.size = self.elem.GetByteSize()
        return False

    def num_children(self):
        return self.len if self.size else 0

    def get_child_index(self, name):
        try:
            return int(name.strip("[]"))
        except ValueError:
            return -1

    def get_child_at_index(self, i):
        if i < 0 or i >= self.len:
            return None
        addr = self.data.GetValueAsUnsigned(0) + i * self.size
        return self.v.CreateValueFromAddress("[%d]" % i, addr, self.elem)

    def has_children(self):
        return True


def array_summary(valobj, _dict):
    if _null(valobj):
        return "null"
    return "length=%d" % _field(valobj, "len").GetValueAsUnsigned(0)


def _entries(m):
    """A map's live entries: (key, value) value objects, in insertion order."""
    items = _field(m, "items")
    if not items.IsValid() or _null(items):
        return []
    n = min(_field(items, "len").GetValueAsUnsigned(0), MAX_CHILDREN)
    data = _field(items, "data")
    entry_t = data.GetType().GetPointeeType()
    size = entry_t.GetByteSize()
    out = []
    for i in range(n):
        e = m.CreateValueFromAddress("e", data.GetValueAsUnsigned(0) + i * size, entry_t)
        if _null(e):
            continue
        if not _field(e, "live").GetValueAsUnsigned(0):
            continue
        out.append((_field(e, "key"), _field(e, "value")))
    return out


def _shown(v):
    s = v.GetSummary()
    return s if s else (v.GetValue() or "?")


class MapProvider:
    """`Map<K, V>`: one child per entry, named by its key."""

    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        self.entries = [] if _null(self.v) else _entries(self.v)
        return False

    def num_children(self):
        return len(self.entries)

    def get_child_index(self, name):
        for i, (k, _) in enumerate(self.entries):
            if "[%s]" % _shown(k) == name:
                return i
        return -1

    def get_child_at_index(self, i):
        if i < 0 or i >= len(self.entries):
            return None
        k, v = self.entries[i]
        return self.v.CreateValueFromData("[%s]" % _shown(k), v.GetData(), v.GetType())

    def has_children(self):
        return True


def map_summary(valobj, _dict):
    if _null(valobj):
        return "null"
    return "size=%d" % _field(valobj, "count").GetValueAsUnsigned(0)


class SetProvider:
    """`Set<T>`: its elements (the keys of the map inside)."""

    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        self.keys = [] if _null(self.v) else [k for k, _ in _entries(_field(self.v, "map"))]
        return False

    def num_children(self):
        return len(self.keys)

    def get_child_index(self, name):
        try:
            return int(name.strip("[]"))
        except ValueError:
            return -1

    def get_child_at_index(self, i):
        if i < 0 or i >= len(self.keys):
            return None
        k = self.keys[i]
        return self.v.CreateValueFromData("[%d]" % i, k.GetData(), k.GetType())

    def has_children(self):
        return True


def set_summary(valobj, _dict):
    if _null(valobj):
        return "null"
    return "size=%d" % _field(_field(valobj, "map"), "count").GetValueAsUnsigned(0)


def __lldb_init_module(debugger, _dict):
    run = debugger.HandleCommand
    run("type category define ppl")
    run('type summary add -w ppl -F ppl_lldb.string_summary "string"')
    run('type summary add -w ppl -F ppl_lldb.array_summary -x "\\[\\]$"')
    run('type synthetic add -w ppl -l ppl_lldb.ArrayProvider -x "\\[\\]$"')
    run('type summary add -w ppl -F ppl_lldb.map_summary -x "^Map<"')
    run('type synthetic add -w ppl -l ppl_lldb.MapProvider -x "^Map<"')
    run('type summary add -w ppl -F ppl_lldb.set_summary -x "^Set<"')
    run('type synthetic add -w ppl -l ppl_lldb.SetProvider -x "^Set<"')
    run("type category enable ppl")
