/* The ppl runtime, v0: printing, number formatting, panics and `main`. */
#include "ppl_rt.h"

#include <inttypes.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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

/* Finds the shortest "%.*e" rendering that round-trips, then re-lays it out. */
static int format_shortest(double v, bool is_f32, char *out) {
    int n = special(v, out);
    if (n >= 0) return n;
    char sci[40];
    int max = is_f32 ? 9 : 17;
    for (int p = 1; p <= max; p++) {
        snprintf(sci, sizeof sci, "%.*e", p - 1, v);
        if (is_f32 ? (strtof(sci, NULL) == (float)v) : (strtod(sci, NULL) == v)) break;
    }
    /* sci is "[-]d[.ddd]e[+-]xx" */
    const char *s = sci;
    bool neg = false;
    if (*s == '-') {
        neg = true;
        s++;
    }
    char digits[24];
    int k = 0;
    while (*s && *s != 'e') {
        if (*s != '.') digits[k++] = *s;
        s++;
    }
    /* strip trailing zeros (snprintf may pad when p digits are needed only for rounding) */
    while (k > 1 && digits[k - 1] == '0') k--;
    int exp = atoi(s + 1);
    return js_layout(neg, digits, k, exp + 1, out);
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

/* ---------------------------------------------------------------- panics */

_Noreturn void ppl_panic(const char *msg, int64_t msg_len, const char *file, int64_t file_len,
                         int32_t line, int32_t col) {
    fflush(stdout);
    fputs("panic: ", stderr);
    fwrite(msg, 1, (size_t)msg_len, stderr);
    fputs("\n  at ", stderr);
    fwrite(file, 1, (size_t)file_len, stderr);
    fprintf(stderr, ":%d:%d\n", line, col);
    fflush(stderr);
    exit(PPL_PANIC_EXIT_CODE);
}

_Noreturn void ppl_panic_str(const PplStr *msg, const char *file, int64_t file_len, int32_t line,
                             int32_t col) {
    ppl_panic(msg->data, msg->byte_len, file, file_len, line, col);
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

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    ppl_entry();
    fflush(stdout);
    return 0;
}
