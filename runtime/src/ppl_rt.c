/* The ppl runtime, v0: printing, number formatting, panics and `main`. */
#define _GNU_SOURCE
#include "ppl_rt.h"

#include <inttypes.h>
#include <setjmp.h>
/* Panic traces need `backtrace` (glibc, macOS; musl has none). `__GLIBC__` comes with
 * ppl_rt.h's <stdint.h>. */
#if defined(__APPLE__) || defined(__GLIBC__)
#define PPL_BACKTRACE 1
#include <dlfcn.h>
#include <execinfo.h>
#endif
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#if defined(_WIN32)
#include <fcntl.h>
#include <io.h>
#endif

/* ---------------------------------------------------------------- memory: see ppl_gc.c */

int32_t ppl_instance_of(const void *obj, const void *info) {
    if (!obj) return 0;
    const void *c = *(const void *const *)obj;
    while (c) {
        if (c == info) return 1;
        c = *(const void *const *)c;
    }
    return 0;
}

/* `c³ - x`, exact enough to tell which of neighbouring `c`s is nearest the cube root. */
static double cube_residual(double c, double x) {
    double c2 = c * c;
    return fma(c2, c, -x) + fma(c, c, -c2) * c;
}

/* glibc's `cbrt` can be ulps off (`cbrt(27)` is 3.0000000000000004); Rust's, which the
 * interpreter uses, and JavaScript's are correctly rounded. `c³ - x` grows with `c`, so step
 * towards the root while the residual shrinks. */
static double cbrt_rounded(double x) {
    double y = cbrt(x);
    if (!(fabs(x) > 0x1p-900 && fabs(x) < 0x1p900)) return y; /* 0, NaN, ∞, extremes */
    double err = cube_residual(y, x);
    double dir = err > 0 ? -INFINITY : INFINITY;
    for (int i = 0; i < 8; i++) {
        double c = nextafter(y, dir), e = cube_residual(c, x);
        if (!(fabs(e) < fabs(err))) break;
        y = c, err = e;
    }
    return y;
}

/* `Math`'s floating-point functions by number (the interpreter's `math_op` has the same
 * table). */
double ppl_math(int64_t op, double x, double y) {
    switch (op) {
    case 0: return sqrt(x);
    case 1: return sin(x);
    case 2: return cos(x);
    case 3: return tan(x);
    case 4: return asin(x);
    case 5: return acos(x);
    case 6: return atan(x);
    case 7: return atan2(x, y);
    case 8: return exp(x);
    case 9: return log(x);
    case 10: return log2(x);
    case 11: return log10(x);
    case 12: return pow(x, y);
    case 13: return floor(x);
    case 14: return ceil(x);
    case 15: return trunc(x);
    case 16: return cbrt_rounded(x);
    case 17: return hypot(x, y);
    case 18: return sinh(x);
    case 19: return cosh(x);
    case 20: return tanh(x);
    case 21: return asinh(x);
    case 22: return acosh(x);
    case 23: return atanh(x);
    case 24: return expm1(x);
    case 25: return log1p(x);
    case 26: return fmod(x, y);
    case 27: return (double)(float)x;
    default: return NAN;
    }
}

double ppl_parse_f64(const PplStr *s) {
    char buf[128];
    int64_t n = s->byte_len;
    if (n <= 0 || n >= (int64_t)sizeof buf) return NAN;
    memcpy(buf, s->data, (size_t)n);
    buf[n] = 0;
    char *end;
    double v = strtod(buf, &end);
    return *end ? NAN : v;
}

static _Thread_local const void **showing;
static _Thread_local int64_t showing_len, showing_cap;

int32_t ppl_show_enter(const void *obj) {
    for (int64_t i = 0; i < showing_len; i++)
        if (showing[i] == obj) return 0;
    if (showing_len == showing_cap) {
        showing_cap = showing_cap ? showing_cap * 2 : 16;
        showing = realloc(showing, (size_t)showing_cap * sizeof *showing);
        if (!showing) abort();
    }
    showing[showing_len++] = obj;
    return 1;
}

void ppl_show_leave(const void *obj) {
    for (int64_t i = showing_len - 1; i >= 0; i--) {
        if (showing[i] == obj) {
            memmove(&showing[i], &showing[i + 1], (size_t)(showing_len - i - 1) * sizeof *showing);
            showing_len--;
            return;
        }
    }
}

uint64_t ppl_hash_u64(uint64_t x) {
    uint64_t z = x + 0x9e3779b97f4a7c15ULL;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

uint64_t ppl_hash_str(const PplStr *s) {
    uint64_t h = 0xcbf29ce484222325ULL;
    for (int64_t i = 0; i < s->byte_len; i++) {
        h ^= (uint8_t)s->data[i];
        h *= 0x100000001b3ULL;
    }
    return h;
}

uint64_t ppl_hash_combine(uint64_t a, uint64_t b) {
    return a ^ (b + 0x9e3779b97f4a7c15ULL + (a << 6) + (a >> 2));
}

/* ---------------------------------------------------------------- arrays */

static const struct {
    int64_t size;
    int32_t nrefs, reserved;
    uint32_t refs[1];
} array_header_layout = {sizeof(PplArray), 1, 0, {offsetof(PplArray, data)}};

static const struct {
    int64_t size;
    int32_t nrefs, reserved;
    uint32_t refs[1];
} pointer_layout = {sizeof(void *), 1, 0, {0}};

static char *array_storage(const PplArray *a, int64_t cap, int64_t elem_size,
                           const PplLayout *elem) {
    return ppl_gc_alloc_near(a, cap * elem_size, elem);
}

PplArray *ppl_array_new(int64_t elem_size, int64_t len, const PplLayout *elem) {
    PplArray *a = ppl_gc_alloc((int64_t)sizeof(PplArray), (const PplLayout *)&array_header_layout);
    int64_t cap = len < 4 ? 4 : len;
    a->len = len;
    a->cap = cap;
    a->elem = elem;
    a->data = array_storage(a, cap, elem_size, elem);
    return a;
}

void *ppl_array_push(PplArray *a, int64_t elem_size) {
    if (a->len == a->cap) {
        int64_t cap = a->cap * 2;
        char *data = array_storage(a, cap, elem_size, a->elem);
        memcpy(data, a->data, (size_t)(a->len * elem_size));
        a->data = data;
        a->cap = cap;
        ppl_gc_write(&a->data, (const PplLayout *)&pointer_layout);
    }
    return a->data + (a->len++) * elem_size;
}

PplArray *ppl_array_slice(const PplArray *a, int64_t elem_size, int64_t start, int64_t end) {
    PplArray *r = ppl_array_new(elem_size, end - start, a->elem);
    memcpy(r->data, a->data + start * elem_size, (size_t)((end - start) * elem_size));
    return r;
}

int32_t ppl_array_pop(PplArray *a) {
    if (a->len == 0) return 0;
    a->len--;
    return 1;
}

/* ---------------------------------------------------------------- strings */

static int64_t count_chars(const char *bytes, int64_t len) {
    int64_t n = 0;
    for (int64_t i = 0; i < len; i++) {
        /* Count every byte that isn't a continuation byte (10xxxxxx). */
        if (((unsigned char)bytes[i] & 0xC0) != 0x80) n++;
    }
    return n;
}

static PplStr *str_alloc(int64_t byte_len) {
    PplStr *s = ppl_alloc((int64_t)sizeof(PplStr) + byte_len);
    s->byte_len = byte_len;
    s->cache_char = 0;
    s->cache_byte = 0;
    return s;
}

PplStr *ppl_str_new(const char *bytes, int64_t byte_len) {
    PplStr *s = str_alloc(byte_len);
    memcpy(s->data, bytes, (size_t)byte_len);
    s->char_len = count_chars(bytes, byte_len);
    return s;
}

PplStr *ppl_str_concat(const PplStr *a, const PplStr *b) {
    PplStr *s = str_alloc(a->byte_len + b->byte_len);
    memcpy(s->data, a->data, (size_t)a->byte_len);
    memcpy(s->data + a->byte_len, b->data, (size_t)b->byte_len);
    s->char_len = a->char_len + b->char_len;
    return s;
}

int32_t ppl_str_eq(const PplStr *a, const PplStr *b) {
    if (a == b) return 1;
    if (!a || !b) return 0; /* `string?` equality: null equals only null */
    return a->byte_len == b->byte_len && memcmp(a->data, b->data, (size_t)a->byte_len) == 0;
}

int32_t ppl_str_cmp(const PplStr *a, const PplStr *b) {
    int64_t n = a->byte_len < b->byte_len ? a->byte_len : b->byte_len;
    int c = memcmp(a->data, b->data, (size_t)n);
    if (c != 0) return c < 0 ? -1 : 1;
    return a->byte_len < b->byte_len ? -1 : a->byte_len > b->byte_len;
}

static int utf8_encode(uint32_t c, char *out);
int ppl_format_f64(double v, char *buf);
int ppl_format_f32(float v, char *buf);

PplStr *ppl_str_from_i64(int64_t v) {
    char buf[24];
    int n = snprintf(buf, sizeof buf, "%" PRId64, v);
    return ppl_str_new(buf, n);
}

PplStr *ppl_str_from_u64(uint64_t v) {
    char buf[24];
    int n = snprintf(buf, sizeof buf, "%" PRIu64, v);
    return ppl_str_new(buf, n);
}

PplStr *ppl_str_from_f64(double v) {
    char buf[40];
    return ppl_str_new(buf, ppl_format_f64(v, buf));
}

PplStr *ppl_str_from_f32(float v) {
    char buf[40];
    return ppl_str_new(buf, ppl_format_f32(v, buf));
}

PplStr *ppl_str_from_bool(int32_t v) { return v ? ppl_str_new("true", 4) : ppl_str_new("false", 5); }

PplStr *ppl_str_from_char(uint32_t c) {
    char buf[4];
    return ppl_str_new(buf, utf8_encode(c, buf));
}

/* Length in bytes of the UTF-8 sequence starting with `lead`. */
static int utf8_len(unsigned char lead) {
    if (lead < 0x80) return 1;
    if (lead < 0xE0) return 2;
    if (lead < 0xF0) return 3;
    return 4;
}

static uint32_t utf8_decode(const unsigned char *p) {
    switch (utf8_len(p[0])) {
    case 1: return p[0];
    case 2: return ((uint32_t)(p[0] & 0x1F) << 6) | (p[1] & 0x3F);
    case 3: return ((uint32_t)(p[0] & 0x0F) << 12) | ((uint32_t)(p[1] & 0x3F) << 6) | (p[2] & 0x3F);
    default:
        return ((uint32_t)(p[0] & 0x07) << 18) | ((uint32_t)(p[1] & 0x3F) << 12) |
               ((uint32_t)(p[2] & 0x3F) << 6) | (p[3] & 0x3F);
    }
}

uint32_t ppl_str_char_at(PplStr *s, int64_t i, const char *file, int64_t file_len, int32_t line,
                         int32_t col) {
    if (i < 0 || i >= s->char_len) {
        ppl_panic("index out of bounds", 19, file, file_len, line, col);
    }
    if (s->byte_len == s->char_len) return (unsigned char)s->data[i]; /* ASCII */
    int64_t c = 0, b = 0;
    if (i >= s->cache_char && s->cache_char >= 0) {
        c = s->cache_char;
        b = s->cache_byte;
    }
    while (c < i) {
        b += utf8_len((unsigned char)s->data[b]);
        c++;
    }
    /* Literals (`cache_char` -1) are constants shared by every thread: no cache. */
    if (s->cache_char >= 0) {
        s->cache_char = c;
        s->cache_byte = b;
    }
    return utf8_decode((const unsigned char *)s->data + b);
}

PplStr *ppl_str_slice(const PplStr *s, int64_t a, int64_t b) {
    int64_t n = s->char_len;
    a = a < 0 ? (n + a < 0 ? 0 : n + a) : (a > n ? n : a);
    b = b < 0 ? (n + b < 0 ? 0 : n + b) : (b > n ? n : b);
    if (b <= a) {
        PplStr *e = str_alloc(0);
        e->char_len = 0;
        return e;
    }
    int64_t from = 0, to;
    if (s->byte_len == n) {
        from = a;
        to = b;
    } else {
        int64_t c = 0;
        while (c < a) {
            from += utf8_len((unsigned char)s->data[from]);
            c++;
        }
        to = from;
        while (c < b) {
            to += utf8_len((unsigned char)s->data[to]);
            c++;
        }
    }
    PplStr *r = str_alloc(to - from);
    memcpy(r->data, s->data + from, (size_t)(to - from));
    r->char_len = b - a;
    return r;
}

/* ---------------------------------------------------------------- printing */

void ppl_print_i64(int64_t v) { printf("%" PRId64, v); }

void ppl_print_u64(uint64_t v) { printf("%" PRIu64, v); }

void ppl_print_bool(int32_t v) { fputs(v ? "true" : "false", stdout); }

void ppl_print_str(const char *ptr, int64_t len) {
    if (len > 0) fwrite(ptr, 1, (size_t)len, stdout);
}

void ppl_print_newline(void) { putchar('\n'); }

void ppl_print_string(const PplStr *s) { ppl_print_str(s->data, s->byte_len); }

/* Encodes a Unicode scalar value as UTF-8; returns the byte count. */
static int utf8_encode(uint32_t c, char *out) {
    if (c < 0x80) {
        out[0] = (char)c;
        return 1;
    } else if (c < 0x800) {
        out[0] = (char)(0xC0 | (c >> 6));
        out[1] = (char)(0x80 | (c & 0x3F));
        return 2;
    } else if (c < 0x10000) {
        out[0] = (char)(0xE0 | (c >> 12));
        out[1] = (char)(0x80 | ((c >> 6) & 0x3F));
        out[2] = (char)(0x80 | (c & 0x3F));
        return 3;
    }
    out[0] = (char)(0xF0 | (c >> 18));
    out[1] = (char)(0x80 | ((c >> 12) & 0x3F));
    out[2] = (char)(0x80 | ((c >> 6) & 0x3F));
    out[3] = (char)(0x80 | (c & 0x3F));
    return 4;
}

void ppl_print_char(uint32_t c) {
    char buf[4];
    fwrite(buf, 1, (size_t)utf8_encode(c, buf), stdout);
}

/* ---------------------------------------------------------------- number formatting
 *
 * Must produce exactly what `ppl_interp::format_float` produces: the shortest digits that
 * round-trip, laid out like ECMAScript's Number::toString. */

/* Lays out 0.d1d2…dk × 10^n (digits has k chars). */
static int js_layout(bool neg, const char *digits, int k, int n, char *out) {
    int len = 0;
    if (neg) out[len++] = '-';
    if (k <= n && n <= 21) {
        memcpy(out + len, digits, (size_t)k);
        len += k;
        for (int i = 0; i < n - k; i++) out[len++] = '0';
    } else if (0 < n && n <= 21) {
        memcpy(out + len, digits, (size_t)n);
        len += n;
        out[len++] = '.';
        memcpy(out + len, digits + n, (size_t)(k - n));
        len += k - n;
    } else if (-6 < n && n <= 0) {
        out[len++] = '0';
        out[len++] = '.';
        for (int i = 0; i < -n; i++) out[len++] = '0';
        memcpy(out + len, digits, (size_t)k);
        len += k;
    } else {
        out[len++] = digits[0];
        if (k > 1) {
            out[len++] = '.';
            memcpy(out + len, digits + 1, (size_t)(k - 1));
            len += k - 1;
        }
        int e = n - 1;
        len += sprintf(out + len, "e%c%d", e < 0 ? '-' : '+', e < 0 ? -e : e);
    }
    out[len] = '\0';
    return len;
}

static int special(double v, char *out) {
    if (isnan(v)) return sprintf(out, "NaN");
    if (isinf(v)) return sprintf(out, v > 0 ? "Infinity" : "-Infinity");
    if (v == 0.0) return sprintf(out, "0");
    return -1;
}

/* The shortest digits d1…dk (into `digits`, returns k) and exponent *n with
 * |v| = 0.d1…dk × 10^n that round-trip. Uses the shortest "%.*e" (half-to-even). v finite, > 0. */
static int shortest_digits(double v, bool is_f32, char *digits, int *n) {
    char sci[40];
    int max = is_f32 ? 9 : 17;
    for (int p = 1; p <= max; p++) {
        snprintf(sci, sizeof sci, "%.*e", p - 1, v);
        if (is_f32 ? (strtof(sci, NULL) == (float)v) : (strtod(sci, NULL) == v)) break;
    }
    const char *s = sci;
    int k = 0;
    while (*s && *s != 'e') {
        if (*s != '.') digits[k++] = *s;
        s++;
    }
    while (k > 1 && digits[k - 1] == '0') k--;
    *n = atoi(s + 1) + 1;
    return k;
}

static int format_shortest(double v, bool is_f32, char *out) {
    int n = special(v, out);
    if (n >= 0) return n;
    char digits[24];
    int k = shortest_digits(fabs(v), is_f32, digits, &n);
    return js_layout(v < 0, digits, k, n, out);
}

int ppl_format_f64(double v, char *buf) { return format_shortest(v, false, buf); }

int ppl_format_f32(float v, char *buf) { return format_shortest((double)v, true, buf); }

void ppl_print_f64(double v) {
    char buf[40];
    fwrite(buf, 1, (size_t)ppl_format_f64(v, buf), stdout);
}

void ppl_print_f32(float v) {
    char buf[40];
    fwrite(buf, 1, (size_t)ppl_format_f32(v, buf), stdout);
}

/* ---------------------------------------------------------------- format specs
 *
 * Must match `ppl_interp::format_spec`. */

typedef struct Buf {
    char *p;
    int64_t len, cap;
} Buf;

static void buf_put(Buf *b, const char *s, int64_t n) {
    if (b->len + n > b->cap) {
        int64_t cap = b->cap ? b->cap * 2 : 64;
        while (cap < b->len + n) cap *= 2;
        char *p = ppl_alloc(cap);
        if (b->len) memcpy(p, b->p, (size_t)b->len);
        b->p = p;
        b->cap = cap;
    }
    if (n) memcpy(b->p + b->len, s, (size_t)n);
    b->len += n;
}

static void buf_puts(Buf *b, const char *s) { buf_put(b, s, (int64_t)strlen(s)); }

static void buf_char(Buf *b, uint32_t c) {
    char tmp[4];
    buf_put(b, tmp, utf8_encode(c, tmp));
}

/* Shortest digits in fixed notation (never an exponent). */
static void put_shortest_fixed(Buf *b, double a, bool is_f32) {
    if (a == 0.0) {
        buf_puts(b, "0");
        return;
    }
    char digits[24];
    int n;
    int k = shortest_digits(a, is_f32, digits, &n);
    if (n <= 0) {
        buf_puts(b, "0.");
        for (int i = 0; i < -n; i++) buf_puts(b, "0");
        buf_put(b, digits, k);
    } else if (n >= k) {
        buf_put(b, digits, k);
        for (int i = 0; i < n - k; i++) buf_puts(b, "0");
    } else {
        buf_put(b, digits, n);
        buf_puts(b, ".");
        buf_put(b, digits + n, k - n);
    }
}

/* Rust-style exponent: "1.5e3", "1e-7" (no '+', no zero padding). */
static void put_exp(Buf *b, const char *mant, int mlen, int e, bool upper) {
    char tmp[16];
    buf_put(b, mant, mlen);
    snprintf(tmp, sizeof tmp, "%c%d", upper ? 'E' : 'e', e);
    buf_puts(b, tmp);
}

static void put_float_body(Buf *b, double a, bool is_f32, const PplFmt *f) {
    char tmp[512];
    int p = f->precision;
    switch (f->kind) {
    case 8: /* f */
        if (p >= 0) {
            snprintf(tmp, sizeof tmp, "%.*f", p, a);
            buf_puts(b, tmp);
        } else {
            put_shortest_fixed(b, a, is_f32);
        }
        break;
    case 6: /* e */
    case 7: /* E */
        if (p >= 0) {
            snprintf(tmp, sizeof tmp, "%.*e", p, a);
            char *e = strchr(tmp, 'e');
            put_exp(b, tmp, (int)(e - tmp), atoi(e + 1), f->kind == 7);
        } else if (a == 0.0) {
            put_exp(b, "0", 1, 0, f->kind == 7);
        } else {
            char digits[24], mant[32];
            int n;
            int k = shortest_digits(a, is_f32, digits, &n);
            int m = 0;
            mant[m++] = digits[0];
            if (k > 1) {
                mant[m++] = '.';
                memcpy(mant + m, digits + 1, (size_t)(k - 1));
                m += k - 1;
            }
            put_exp(b, mant, m, n - 1, f->kind == 7);
        }
        break;
    case 9: { /* % */
        double y = a * 100.0;
        if (p >= 0) {
            snprintf(tmp, sizeof tmp, "%.*f", p, y);
            buf_puts(b, tmp);
        } else {
            put_shortest_fixed(b, y, false);
        }
        buf_puts(b, "%");
        break;
    }
    default:
        if (p >= 0) {
            snprintf(tmp, sizeof tmp, "%.*f", p, a);
            buf_puts(b, tmp);
        } else {
            int n = format_shortest(a, is_f32, tmp);
            buf_put(b, tmp, n);
        }
    }
}

/* Applies sign, prefix, width, fill and alignment around `body`. */
static PplStr *pad(bool neg, const char *prefix, const Buf *body, bool numeric, const PplFmt *f) {
    const char *sign = neg ? "-" : (numeric && f->sign == 1) ? "+" : (numeric && f->sign == 2) ? " " : "";
    int64_t len = (int64_t)strlen(sign) + (int64_t)strlen(prefix) + count_chars(body->p, body->len);
    int64_t width = f->width < 0 ? 0 : f->width;
    Buf out = {0};
    if (len >= width) {
        buf_puts(&out, sign);
        buf_puts(&out, prefix);
        buf_put(&out, body->p, body->len);
    } else if (f->zero && numeric) {
        buf_puts(&out, sign);
        buf_puts(&out, prefix);
        for (int64_t i = 0; i < width - len; i++) buf_puts(&out, "0");
        buf_put(&out, body->p, body->len);
    } else {
        int align = f->align ? f->align : (numeric ? 2 : 1);
        int64_t missing = width - len;
        int64_t left = align == 1 ? 0 : align == 2 ? missing : missing / 2;
        for (int64_t i = 0; i < left; i++) buf_char(&out, f->fill);
        buf_puts(&out, sign);
        buf_puts(&out, prefix);
        buf_put(&out, body->p, body->len);
        for (int64_t i = 0; i < missing - left; i++) buf_char(&out, f->fill);
    }
    return ppl_str_new(out.p, out.len);
}

static PplStr *fmt_int(bool neg, uint64_t mag, const PplFmt *f, double as_float) {
    Buf body = {0};
    char tmp[80];
    if (f->kind >= 6 && f->kind <= 9) {
        put_float_body(&body, fabs(as_float), false, f);
        return pad(as_float < 0, "", &body, true, f);
    }
    const char *prefix = "";
    switch (f->kind) {
    case 2:
        snprintf(tmp, sizeof tmp, "%" PRIx64, mag);
        if (f->alternate) prefix = "0x";
        break;
    case 3:
        snprintf(tmp, sizeof tmp, "%" PRIX64, mag);
        if (f->alternate) prefix = "0x";
        break;
    case 4:
        snprintf(tmp, sizeof tmp, "%" PRIo64, mag);
        if (f->alternate) prefix = "0o";
        break;
    case 5: {
        int n = 0;
        do {
            tmp[n++] = (char)('0' + (mag & 1));
            mag >>= 1;
        } while (mag);
        for (int i = 0; i < n / 2; i++) {
            char c = tmp[i];
            tmp[i] = tmp[n - 1 - i];
            tmp[n - 1 - i] = c;
        }
        tmp[n] = '\0';
        if (f->alternate) prefix = "0b";
        break;
    }
    default:
        snprintf(tmp, sizeof tmp, "%" PRIu64, mag);
    }
    buf_puts(&body, tmp);
    return pad(neg, prefix, &body, true, f);
}

PplStr *ppl_fmt_i64(int64_t v, const PplFmt *f) {
    uint64_t mag = v < 0 ? (uint64_t)0 - (uint64_t)v : (uint64_t)v;
    return fmt_int(v < 0, mag, f, (double)v);
}

PplStr *ppl_fmt_u64(uint64_t v, const PplFmt *f) { return fmt_int(false, v, f, (double)v); }

PplStr *ppl_fmt_f64(double v, int32_t is_f32, const PplFmt *f) {
    Buf body = {0};
    bool neg = false;
    if (isnan(v)) {
        buf_puts(&body, "NaN");
    } else {
        neg = v < 0;
        if (isinf(v)) buf_puts(&body, "Infinity");
        else put_float_body(&body, fabs(v), is_f32 != 0, f);
    }
    return pad(neg, "", &body, true, f);
}

/* `"…"` / `'…'` with escapes (debug format). */
static void put_quoted(Buf *b, const char *s, int64_t len, char q) {
    char tmp[16];
    buf_put(b, &q, 1);
    for (int64_t i = 0; i < len;) {
        int n = utf8_len((unsigned char)s[i]);
        uint32_t cp = utf8_decode((const unsigned char *)s + i);
        if (cp == '\\') buf_puts(b, "\\\\");
        else if (cp == '\n') buf_puts(b, "\\n");
        else if (cp == '\r') buf_puts(b, "\\r");
        else if (cp == '\t') buf_puts(b, "\\t");
        else if (cp == 0) buf_puts(b, "\\0");
        else if (cp == (uint32_t)q) {
            buf_puts(b, "\\");
            buf_put(b, &q, 1);
        } else if (cp < 0x20 || cp == 0x7F) {
            snprintf(tmp, sizeof tmp, "\\u{%x}", cp);
            buf_puts(b, tmp);
        } else {
            buf_put(b, s + i, n);
        }
        i += n;
    }
    buf_put(b, &q, 1);
}

PplStr *ppl_fmt_str(const PplStr *s, const PplFmt *f) {
    Buf body = {0};
    if (f->kind == 10) {
        put_quoted(&body, s->data, s->byte_len, '"');
    } else if (f->precision >= 0 && f->precision < s->char_len) {
        int64_t b = 0;
        for (int32_t c = 0; c < f->precision; c++) b += utf8_len((unsigned char)s->data[b]);
        buf_put(&body, s->data, b);
    } else {
        buf_put(&body, s->data, s->byte_len);
    }
    return pad(false, "", &body, false, f);
}

PplStr *ppl_fmt_char(uint32_t c, const PplFmt *f) {
    Buf body = {0};
    char tmp[4];
    int n = utf8_encode(c, tmp);
    if (f->kind == 10) put_quoted(&body, tmp, n, '\'');
    else buf_put(&body, tmp, n);
    return pad(false, "", &body, false, f);
}

PplStr *ppl_fmt_bool(int32_t v, const PplFmt *f) {
    Buf body = {0};
    buf_puts(&body, v ? "true" : "false");
    return pad(false, "", &body, false, f);
}

/* ---------------------------------------------------------------- panics */

/* Panics unwind through the regions guarded by `try … finally` (innermost last): each guard is
 * a jmp_buf in the guarded function's frame. The report waits until no guard is left. */
#define PPL_MAX_GUARDS 4096
#define PPL_MAX_FRAMES 64
static _Thread_local void *guards[PPL_MAX_GUARDS];
/* The shadow-stack top when each guard was pushed, restored when a panic jumps to it. */
static _Thread_local PplFrame *guard_frames[PPL_MAX_GUARDS];
static _Thread_local int32_t guard_arenas[PPL_MAX_GUARDS];
static _Thread_local int nguards;
static _Thread_local char *panic_msg;
static _Thread_local int64_t panic_msg_len;
static _Thread_local const char *panic_file;
static _Thread_local int64_t panic_file_len;
static _Thread_local int32_t panic_line, panic_col;
static _Thread_local void *panic_frames[PPL_MAX_FRAMES];
static _Thread_local int panic_nframes;

void ppl_guard_push(void *buf) {
    if (nguards == PPL_MAX_GUARDS) {
        fflush(stdout);
        fputs("fatal: too many nested `finally` regions\n", stderr);
        abort();
    }
    guard_frames[nguards] = ppl_gc_top;
    guard_arenas[nguards] = ppl_arena_level();
    guards[nguards++] = buf;
}

void ppl_guard_pop(void) {
    if (nguards > 0) nguards--;
}

/* A frame's function name: ppl functions are `ppl.<name>` (exported in debug builds), the
 * top level is `ppl_entry`; anything else (the runtime, libc) is NULL. */
static const char *frame_name(void *addr) {
#if PPL_BACKTRACE
    Dl_info info, self;
    /* A return address can point just past its function (after a call to a noreturn
     * function), so look up the byte before it. */
    if (dladdr((char *)addr - 1, &info)) {
        const char *n = info.dli_sname;
        if (n) {
            if (n[0] == '_' && strncmp(n + 1, "ppl", 3) == 0) n++;
            if (strncmp(n, "ppl.", 4) == 0) return n + 4;
            if (strcmp(n, "ppl_entry") == 0) return "<main>";
            return NULL;
        }
        /* No symbol outside the executable (glibc's static `__libc_start_call_main`): not
         * ours. The runtime is linked into the executable, so this function's object is it. */
        if (dladdr((void *)frame_name, &self) && info.dli_fbase != self.dli_fbase) return NULL;
    }
#else
    (void)addr;
#endif
    return "";
}

static _Noreturn void report_panic(void) {
    /* In a worker: it ends, and its parent's `onerror` gets the message. */
    ppl_worker_failed(panic_msg ? panic_msg : "", panic_msg_len);
    fflush(stdout);
    fputs("panic: ", stderr);
    fwrite(panic_msg, 1, (size_t)panic_msg_len, stderr);
    fputc('\n', stderr);
    /* (The runtime's own checks, like the arena write barrier, know no source location.) */
    if (panic_file_len > 0) {
        fputs("  at ", stderr);
        fwrite(panic_file, 1, (size_t)panic_file_len, stderr);
        fprintf(stderr, ":%d:%d\n", panic_line, panic_col);
    }
    if (panic_nframes > 0) {
        fputs("stack trace:\n", stderr);
        int k = 0;
        for (int i = 0; i < panic_nframes; i++) {
            const char *n = frame_name(panic_frames[i]);
            if (!n) continue;
            if (*n) {
                fprintf(stderr, "  %d: %s\n", k++, n);
            } else {
                /* No symbol (release builds): the raw address, for `ppl symbolize`. */
                fprintf(stderr, "  %d: %p\n", k++, panic_frames[i]);
            }
        }
    }
    fflush(stderr);
    ppl_exit(PPL_PANIC_EXIT_CODE);
}

static _Noreturn void unwind(void) {
    if (nguards > 0) {
        void *buf = guards[--nguards];
        ppl_gc_top = guard_frames[nguards];
        /* (Arenas opened inside the guarded region are dropped unchecked.) */
        ppl_arena_release(guard_arenas[nguards]);
        ppl_longjmp(buf, 1);
    }
    report_panic();
}

_Noreturn void ppl_resume(void) { unwind(); }

/* A `__catch_panic` handler stops the panic that jumped to it: its message. */
PplStr *ppl_panic_taken(void) {
    PplStr *s = ppl_str_new(panic_msg ? panic_msg : "", panic_msg ? panic_msg_len : 0);
    panic_nframes = 0;
    return s;
}

_Noreturn void ppl_panic(const char *msg, int64_t msg_len, const char *file, int64_t file_len,
                         int32_t line, int32_t col) {
    free(panic_msg);
    panic_msg = malloc((size_t)msg_len + 1);
    if (panic_msg) memcpy(panic_msg, msg, (size_t)msg_len);
    panic_msg_len = panic_msg ? msg_len : 0;
    panic_file = file;
    panic_file_len = file_len;
    panic_line = line;
    panic_col = col;
#if PPL_BACKTRACE
    panic_nframes = backtrace(panic_frames, PPL_MAX_FRAMES);
#endif
    unwind();
}

_Noreturn void ppl_panic_str(const PplStr *msg, const char *file, int64_t file_len, int32_t line,
                             int32_t col) {
    ppl_panic(msg->data, msg->byte_len, file, file_len, line, col);
}

_Noreturn void ppl_uncaught(const PplStr *cls, const PplStr *msg) {
    {
        /* In a worker: it ends, and its parent's `onerror` hears about it. */
        size_t n = 9 + (size_t)cls->byte_len + 2 + (size_t)msg->byte_len;
        char *text = malloc(n);
        if (text) {
            memcpy(text, "uncaught ", 9);
            memcpy(text + 9, cls->data, (size_t)cls->byte_len);
            memcpy(text + 9 + cls->byte_len, ": ", 2);
            memcpy(text + 11 + cls->byte_len, msg->data, (size_t)msg->byte_len);
            ppl_worker_failed(text, (int64_t)n);
            free(text);
        }
    }
    fflush(stdout);
    fputs("uncaught ", stderr);
    fwrite(cls->data, 1, (size_t)cls->byte_len, stderr);
    fputs(": ", stderr);
    fwrite(msg->data, 1, (size_t)msg->byte_len, stderr);
    fputs("\n", stderr);
    fflush(stderr);
    ppl_exit(1);
}

/* ---------------------------------------------------------------- integer power */

static bool fits_s(__int128 v, int32_t bits) {
    __int128 lim = (__int128)1 << (bits - 1);
    return v >= -lim && v < lim;
}

int64_t ppl_ipow_s(int64_t base, int64_t exp, int32_t bits, int32_t *overflow) {
    __int128 b = base, acc = 1;
    *overflow = 0;
    while (exp > 0) {
        if (exp & 1) {
            acc *= b;
            if (!fits_s(acc, bits)) {
                *overflow = 1;
                return 0;
            }
        }
        exp >>= 1;
        if (exp > 0) {
            b *= b;
            /* |b| >= 2 and b*b out of range: the result is out of range too. */
            if (!fits_s(b, bits)) {
                *overflow = 1;
                return 0;
            }
        }
    }
    return (int64_t)acc;
}

uint64_t ppl_ipow_u(uint64_t base, uint64_t exp, int32_t bits, int32_t *overflow) {
    unsigned __int128 b = base, acc = 1;
    unsigned __int128 lim = (unsigned __int128)1 << bits;
    *overflow = 0;
    while (exp > 0) {
        if (exp & 1) {
            acc *= b;
            if (acc >= lim) {
                *overflow = 1;
                return 0;
            }
        }
        exp >>= 1;
        if (exp > 0) {
            b *= b;
            if (b >= lim) {
                *overflow = 1;
                return 0;
            }
        }
    }
    return (uint64_t)acc;
}

/* ---------------------------------------------------------------- entry */

/* Ends the program with `code`, its output flushed. On WASI a code other than 0 goes through
 * `wasi:cli/exit`'s `exit-with-code`: the preview 1 adapter would make every failure 1. */
_Noreturn void ppl_exit(int code) {
    fflush(stdout);
    fflush(stderr);
#if defined(__wasi__)
    if (code != 0) {
        extern void wasi_cli_exit_exit_with_code(uint8_t status_code);
        wasi_cli_exit_exit_with_code((uint8_t)code);
    }
#endif
    exit(code);
}

/* The program's arguments, for `std/process` (`args`). */
int ppl_argc;
char **ppl_argv;
/* When the program started (monotonic nanoseconds), for `performance.now()`. */
int64_t ppl_start_ns;

int main(int argc, char **argv) {
    ppl_argc = argc;
    ppl_argv = argv;
#if defined(_WIN32)
    /* `\n` stays `\n`: the CRT's text mode would write `\r\n`. */
    _setmode(0, _O_BINARY);
    _setmode(1, _O_BINARY);
    _setmode(2, _O_BINARY);
#endif
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    ppl_start_ns = (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
    ppl_entry();
    fflush(stdout);
    return 0;
}
