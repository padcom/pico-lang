// C structs by value (#114): one of each shape the C ABIs treat differently (in registers,
// split across integer and float registers, homogeneous float aggregates, by reference, odd
// sizes, nested with `bool` and pointer fields). No headers, so it compiles for every target.
typedef __INT8_TYPE__ int8_t;
typedef __INT32_TYPE__ int32_t;
typedef __INT64_TYPE__ int64_t;
typedef __UINT8_TYPE__ uint8_t;
typedef __SIZE_TYPE__ size_t;
typedef struct { int32_t x, y; } Small;
typedef struct { double d; int32_t i; } Mixed;
typedef struct { float a, b, c; } Hfa3;
typedef struct { double x, y, z, w; } Hfa4;
typedef struct { int64_t v[5]; } Big;
typedef struct { uint8_t a, b, c; } Tiny;
typedef struct { Small s; double d; _Bool ok; const char *name; } Nested;

Small small_add(Small p, Small q) { Small r = { p.x + q.x, p.y + q.y }; return r; }
Mixed mixed_scale(Mixed m, int32_t k) { Mixed r = { m.d * k, m.i * k }; return r; }
float hfa3_sum(Hfa3 h) { return h.a + h.b + h.c; }
Hfa4 hfa4_twice(Hfa4 h) { Hfa4 r = { 2 * h.x, 2 * h.y, 2 * h.z, 2 * h.w }; return r; }
Big big_fill(int64_t start) { Big b; for (int i = 0; i < 5; i++) b.v[i] = start + i; return b; }
int64_t big_sum(Big b, int8_t extra) { int64_t s = extra; for (int i = 0; i < 5; i++) s += b.v[i]; return s; }
Tiny tiny_rev(Tiny t) { Tiny r = { t.c, t.b, t.a }; return r; }
Nested nested_make(const char *name, Small s, double d, _Bool ok) { Nested n = { s, d, ok, name }; return n; }
size_t nested_len(Nested n) { size_t k = 0; while (n.name[k]) k++; return n.ok ? k + (size_t)n.s.x : 0; }
