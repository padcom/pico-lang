# lldb formatters for pico programs (spec §14.5, PLAN P10.8c): strings as text, arrays with
# their elements, maps and sets with their entries, reactive cells as their values. `pico dap`
# loads this into lldb-dap; with plain lldb: `command script import <this file>`.
#
# The layouts are the runtime's (runtime/include/pico_rt.h) as debug info describes them:
# `string` is `PicoStr` (`byte_len` at 0, the UTF-8 bytes at 32), `T[]` is `PicoArray` (`len`,
# `cap`, `data`), `Map<K, V>` and `Set<T>` are the core library's classes (stdlib/core/map.pi),
# and so is `Reactive<T>` (stdlib/core/reactive.pi: the value in `cache[0]`, a computed's
# observer in `obs`).

import re

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


def _shown_value(v):
    """The value lldb shows for a reference: the object itself (its summary), not the address."""
    raw = v.GetNonSyntheticValue()
    if raw.TypeIsPointerType() and raw.GetValueAsUnsigned(0) != 0:
        return raw.Dereference()
    return raw


def _object(name, v):
    """A child to show: a (non-null) reference as the object it points to (its summary, not
    an address), anything else as it is."""
    raw = v.GetNonSyntheticValue()
    if raw.TypeIsPointerType() and raw.GetValueAsUnsigned(0) != 0:
        return v.CreateValueFromAddress(name, raw.GetValueAsUnsigned(0), raw.GetType().GetPointeeType())
    return v.CreateValueFromData(name, raw.GetData(), raw.GetType())


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
        return _object("[%d]" % i, self.v.CreateValueFromAddress("e", addr, self.elem))

    def has_children(self):
        return True


class NoChildren:
    """A string isn't expandable (its runtime fields and bytes are implementation details)."""

    def __init__(self, valobj, _dict):
        self.v = valobj

    def num_children(self):
        return 0

    def get_child_index(self, _name):
        return -1

    def get_child_at_index(self, _i):
        return None

    def update(self):
        return False

    def has_children(self):
        return False


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
        return _object("[%s]" % _shown(k), v)

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
        return _object("[%d]" % i, self.keys[i])

    def has_children(self):
        return True


def set_summary(valobj, _dict):
    if _null(valobj):
        return "null"
    return "size=%d" % _field(_field(valobj, "map"), "count").GetValueAsUnsigned(0)


def _cell_value(v):
    """A reactive cell's value (`cache[0]`), or None before a computed's first run."""
    cache = _field(v, "cache")
    if not cache.IsValid() or _null(cache) or _field(cache, "len").GetValueAsUnsigned(0) == 0:
        return None
    data = _field(cache, "data")
    return v.CreateValueFromAddress("value", data.GetValueAsUnsigned(0), data.GetType().GetPointeeType())


def reactive_summary(valobj, _dict):
    """`Reactive<T>` (spec §16): its value; a computed's also says when it's out of date."""
    if _null(valobj):
        return "null"
    obs = _field(valobj, "obs")
    computed = obs.IsValid() and not _null(obs)
    value = _cell_value(valobj)
    if value is None:
        return "not computed yet" if computed else "?"
    shown = _shown(_shown_value(value))
    if computed and _field(obs, "dirty").GetValueAsUnsigned(0):
        shown += " (dirty)"
    return shown


class ReactiveProvider:
    """`Reactive<T>`: just its value (the dependency graph is the runtime's)."""

    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        self.value = None if _null(self.v) else _cell_value(self.v)
        return False

    def num_children(self):
        return 1 if self.value is not None else 0

    def get_child_index(self, name):
        return 0 if name == "value" else -1

    def get_child_at_index(self, i):
        if i != 0 or self.value is None:
            return None
        return _object("value", self.value)

    def has_children(self):
        return self.value is not None


# Types with providers of their own (their fields are the runtime's).
_COLLECTION = re.compile(r"^(string|.*\[\]|Map<.*>|Set<.*>|Reactive<.*>)$")


def _struct(t):
    if t.IsPointerType():
        t = t.GetPointeeType()
    return t if t.GetTypeClass() == lldb.eTypeClassStruct else None


def is_object_with_refs(sbtype, _dict):
    """A pico struct or object with reference fields (strings, arrays, objects): its fields are
    shown through `ObjectProvider`."""
    t = _struct(sbtype)
    if t is None or _COLLECTION.match(t.GetName() or ""):
        return False
    for i in range(t.GetNumberOfFields()):
        if _struct(t.GetFieldAtIndex(i).GetType()) is not None and t.GetFieldAtIndex(i).GetType().IsPointerType():
            return True
    return False


class ObjectProvider:
    """An object's fields, references shown as their objects (`"Ann"`, not an address)."""

    def __init__(self, valobj, _dict):
        self.v = valobj
        self.update()

    def update(self):
        self.raw = None if _null(self.v) else _target(self.v)
        return False

    def num_children(self):
        return self.raw.GetNumChildren() if self.raw else 0

    def get_child_index(self, name):
        return self.raw.GetIndexOfChildWithName(name) if self.raw else -1

    def get_child_at_index(self, i):
        if not self.raw or i < 0 or i >= self.raw.GetNumChildren():
            return None
        c = self.raw.GetChildAtIndex(i)
        return _object(c.GetName(), c)

    def has_children(self):
        return True


def __lldb_init_module(debugger, _dict):
    run = debugger.HandleCommand
    run("type category define pico")
    # Variables of these types are described as the objects (the reference dereferenced), so
    # a debugger shows the summary alone (`-v`: no value): `"hello"`, `length=2`, `size=2`.
    # The providers apply to the objects and to references to them (fields, nullables).
    run('type summary add -w pico -v -F pico_lldb.string_summary "string"')
    run('type synthetic add -w pico -l pico_lldb.NoChildren -x "^string( \\*)?$"')
    run('type summary add -w pico -v -F pico_lldb.array_summary -x "\\[\\]$"')
    run('type synthetic add -w pico -l pico_lldb.ArrayProvider -x "\\[\\]( \\*)?$"')
    run('type summary add -w pico -v -F pico_lldb.map_summary -x "^Map<.*>$"')
    run('type synthetic add -w pico -l pico_lldb.MapProvider -x "^Map<.*>( \\*)?$"')
    run('type summary add -w pico -v -F pico_lldb.set_summary -x "^Set<.*>$"')
    run('type synthetic add -w pico -l pico_lldb.SetProvider -x "^Set<.*>( \\*)?$"')
    run('type summary add -w pico -v -F pico_lldb.reactive_summary -x "^Reactive<.*>$"')
    run('type synthetic add -w pico -l pico_lldb.ReactiveProvider -x "^Reactive<.*>( \\*)?$"')
    # References to them (fields, elements, watch expressions such as `who.name`) show no
    # address either (`void`: no value, the summary alone; `null` when there's none).
    for pattern in ["^string \\*$", "\\[\\] \\*$", "^Map<.*> \\*$", "^Set<.*> \\*$", "^Reactive<.*> \\*$"]:
        run('type format add -w pico -f void -x "%s"' % pattern)
    run(
        "type synthetic add -w pico -l pico_lldb.ObjectProvider "
        "--recognizer-function pico_lldb.is_object_with_refs"
    )
    run("type category enable pico")
