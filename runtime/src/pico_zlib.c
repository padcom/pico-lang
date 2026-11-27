/* Raw DEFLATE streams over miniz (third_party/miniz), for WebSocket permessage-deflate
 * (RFC 7692, #67): `__zlib_op` 41 makes a stream (`a` 0 to compress, 1 to decompress; `b`
 * the most bytes one message may decompress to, 0 for no limit), 42 runs a message through it
 * (`a` the stream, `b` the address of its bytes, `c` how many; the result's length, or -1 when
 * it's corrupt or over the limit), 43 hands over that result (malloc'd: an `ArrayBuffer`
 * adopts it), 44 frees the stream, 45 the last result's length, 46 frees memory `a` (the
 * interpreter's copy).
 *
 * A compressor starts afresh for each message (no context takeover: always allowed) and the
 * flush's trailing 00 00 ff ff is removed; a decompressor keeps its window across messages
 * (the peer may take over its context) and gets the trailer put back. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "miniz.h"

typedef struct Zs {
    mz_stream z;
    int inflate;
    int64_t limit;
    unsigned char *out;
    size_t len, cap;
} Zs;

static void out_grow(Zs *s, size_t need) {
    if (s->len + need <= s->cap) return;
    size_t cap = s->cap ? s->cap : 256;
    while (cap < s->len + need) cap *= 2;
    s->out = realloc(s->out, cap);
    if (!s->out) abort();
    s->cap = cap;
}

static int64_t zs_new(int64_t kind, int64_t limit) {
    Zs *s = calloc(1, sizeof *s);
    if (!s) abort();
    s->inflate = kind == 1;
    s->limit = limit;
    int r = s->inflate ? mz_inflateInit2(&s->z, -MZ_DEFAULT_WINDOW_BITS)
                       : mz_deflateInit2(&s->z, MZ_DEFAULT_LEVEL, MZ_DEFLATED,
                                         -MZ_DEFAULT_WINDOW_BITS, 8, MZ_DEFAULT_STRATEGY);
    if (r != MZ_OK) abort();
    return (int64_t)(uintptr_t)s;
}

/* Runs `n` bytes at `in` through `s` with a sync flush; the output's length, or -1. */
static int64_t zs_run(Zs *s, const unsigned char *in, size_t n) {
    static const unsigned char tail[4] = {0, 0, 0xff, 0xff};
    s->len = 0;
    if (!s->inflate) {
        if (mz_deflateReset(&s->z) != MZ_OK) return -1;
        s->z.next_in = in;
        s->z.avail_in = (unsigned int)n;
        for (;;) {
            out_grow(s, 4096);
            s->z.next_out = s->out + s->len;
            s->z.avail_out = (unsigned int)(s->cap - s->len);
            int r = mz_deflate(&s->z, MZ_SYNC_FLUSH);
            s->len = s->cap - s->z.avail_out;
            if (r != MZ_OK && r != MZ_BUF_ERROR) return -1;
            if (s->z.avail_in == 0 && s->z.avail_out != 0) break;
        }
        /* (The sync flush ends with an empty stored block: RFC 7692 §7.2.1 drops it.) */
        if (s->len >= 4 && memcmp(s->out + s->len - 4, tail, 4) == 0) s->len -= 4;
        return (int64_t)s->len;
    }
    for (int part = 0; part < 2; part++) {
        s->z.next_in = part == 0 ? in : tail;
        s->z.avail_in = part == 0 ? (unsigned int)n : 4;
        while (s->z.avail_in > 0) {
            out_grow(s, 4096);
            s->z.next_out = s->out + s->len;
            s->z.avail_out = (unsigned int)(s->cap - s->len);
            int r = mz_inflate(&s->z, MZ_SYNC_FLUSH);
            s->len = s->cap - s->z.avail_out;
            if (r != MZ_OK && r != MZ_BUF_ERROR && r != MZ_STREAM_END) return -1;
            if (s->limit > 0 && (int64_t)s->len > s->limit) return -1;
            if (r == MZ_BUF_ERROR && s->z.avail_out != 0) break; /* (no progress) */
        }
    }
    /* What's still buffered inside miniz comes out with more room. */
    for (;;) {
        out_grow(s, 4096);
        s->z.next_out = s->out + s->len;
        s->z.avail_out = (unsigned int)(s->cap - s->len);
        int r = mz_inflate(&s->z, MZ_SYNC_FLUSH);
        size_t got = s->cap - s->z.avail_out - s->len;
        s->len += got;
        if (s->limit > 0 && (int64_t)s->len > s->limit) return -1;
        if (got == 0 || r != MZ_OK) break;
    }
    return (int64_t)s->len;
}

int64_t pico_zlib_op(int64_t op, int64_t a, int64_t b, int64_t c) {
    Zs *s = (Zs *)(intptr_t)a;
    switch (op) {
    case 41: return zs_new(a, b);
    case 42: return zs_run(s, (const unsigned char *)(intptr_t)b, c > 0 ? (size_t)c : 0);
    case 43: {
        char *m = malloc(s->len ? s->len : 1);
        if (!m) abort();
        if (s->len) memcpy(m, s->out, s->len);
        return (int64_t)(uintptr_t)m;
    }
    case 44:
        if (s->inflate) mz_inflateEnd(&s->z);
        else mz_deflateEnd(&s->z);
        free(s->out);
        free(s);
        return 0;
    case 45: return (int64_t)s->len;
    case 46: free(s); return 0;
    default: return 0;
    }
}
