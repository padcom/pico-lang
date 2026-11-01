/* Workers (spec §10.2–10.3): OS threads, each with its own heap and module instances (all
 * runtime and program state is thread-local), talking through message queues. A message
 * is deep-copied: encoded from the sender's heap into a malloc'd buffer (the object graph
 * with its sharing and cycles, pointers as object indices), then decoded into the
 * receiver's heap. Code, type maps, class metadata and string literals are shared.
 *
 * The ppl side (`Worker`, `std/worker`, the event loop) calls `ppl_worker_op` and the
 * post/take functions; a thread blocked in the event loop is woken when a message comes. */
#define _DEFAULT_SOURCE
#define _DARWIN_C_SOURCE
#include "ppl_rt.h"

#include <pthread.h>
#include <setjmp.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Generated: runs worker `k`'s entry (library modules' code, the worker module's, the loop).
 * (The weak default is for programs linked without generated code, like the runtime's
 * tests.) */
__attribute__((weak)) void ppl_worker_main(int32_t k) { (void)k; }

/* ppl_gc.c / ppl_loop.c */
void *ppl_gc_find_object(const void *p, int64_t *size, const PplLayout **layout);
void *ppl_loop_waker(void);
void ppl_loop_wake(void *waker);

enum { MSG_DATA = 0, MSG_ERROR = 1, MSG_EXIT = 2 };

typedef struct Msg {
    struct Msg *next;
    int kind;
    char *data; /* MSG_DATA: an encoded value; MSG_ERROR: the message text */
    size_t len;
} Msg;

typedef struct {
    pthread_mutex_t mu;
    Msg *head, *tail;
} Chan;

typedef struct Thread {
    int32_t entry;
    pthread_t th;
    Chan to_worker, to_parent;
    void *waker;        /* the worker's event loop */
    void *parent_waker; /* the parent's */
    _Atomic int terminate;
    _Atomic(int32_t *) requested; /* the worker's `ppl_gc_requested` (to interrupt it) */
    _Atomic int started;
    /* Seen by the parent only: */
    int live;  /* not exited or terminated (as far as the parent knows) */
    int unref; /* doesn't keep the parent's loop running (an idle inline worker) */
} Thread;

/* Handles are indices + 1 into this table (shared by all threads). */
#define MAX_WORKERS 4096
static Thread *table[MAX_WORKERS];
static _Atomic int nthreads;

/* This thread as a worker (NULL in the main thread), and how its code ends. */
static _Thread_local Thread *self_thread;
static _Thread_local jmp_buf worker_exit;

static void chan_init(Chan *c) {
    pthread_mutex_init(&c->mu, NULL);
    c->head = c->tail = NULL;
}

static void chan_push(Chan *c, int kind, char *data, size_t len) {
    Msg *m = malloc(sizeof *m);
    if (!m) abort();
    m->next = NULL;
    m->kind = kind;
    m->data = data;
    m->len = len;
    pthread_mutex_lock(&c->mu);
    if (c->tail) {
        c->tail->next = m;
    } else {
        c->head = m;
    }
    c->tail = m;
    pthread_mutex_unlock(&c->mu);
}

static Msg *chan_peek(Chan *c) {
    pthread_mutex_lock(&c->mu);
    Msg *m = c->head;
    pthread_mutex_unlock(&c->mu);
    return m;
}

static Msg *chan_pop(Chan *c) {
    pthread_mutex_lock(&c->mu);
    Msg *m = c->head;
    if (m) {
        c->head = m->next;
        if (!c->head) c->tail = NULL;
    }
    pthread_mutex_unlock(&c->mu);
    return m;
}

/* ---------------------------------------------------------------- encoding */

typedef struct {
    char *buf;
    size_t len, cap;
    /* Heap objects seen: their address (data) and index; open addressing. */
    const void **keys;
    uint32_t *vals;
    size_t slots, n;
    /* Objects to encode: address, size, layout. */
    struct {
        const char *data;
        int64_t size;
        const PplLayout *layout;
    } *objs;
    size_t nobjs, capobjs;
} Enc;

static void put(Enc *e, const void *p, size_t n) {
    if (e->len + n > e->cap) {
        while (e->len + n > e->cap) e->cap = e->cap ? e->cap * 2 : 256;
        e->buf = realloc(e->buf, e->cap);
        if (!e->buf) abort();
    }
    memcpy(e->buf + e->len, p, n);
    e->len += n;
}

static size_t hash_ptr(const void *p, size_t slots) {
    uintptr_t x = (uintptr_t)p;
    x ^= x >> 17;
    x *= 0x9e3779b97f4a7c15ULL;
    return (size_t)(x >> 7) & (slots - 1);
}

static void grow_map(Enc *e);

/* The index of the object at `data` (encoded on first sight). */
static uint32_t object_index(Enc *e, const char *data, int64_t size, const PplLayout *layout) {
    if (e->n * 2 >= e->slots) grow_map(e);
    size_t i = hash_ptr(data, e->slots);
    while (e->keys[i]) {
        if (e->keys[i] == data) return e->vals[i];
        i = (i + 1) & (e->slots - 1);
    }
    uint32_t idx = (uint32_t)e->nobjs;
    e->keys[i] = data;
    e->vals[i] = idx;
    e->n++;
    if (e->nobjs == e->capobjs) {
        e->capobjs = e->capobjs ? e->capobjs * 2 : 16;
        e->objs = realloc(e->objs, e->capobjs * sizeof *e->objs);
        if (!e->objs) abort();
    }
    e->objs[e->nobjs].data = data;
    e->objs[e->nobjs].size = size;
    e->objs[e->nobjs].layout = layout;
    e->nobjs++;
    return idx;
}

static void grow_map(Enc *e) {
    size_t old = e->slots;
    const void **keys = e->keys;
    uint32_t *vals = e->vals;
    e->slots = old ? old * 2 : 64;
    e->keys = calloc(e->slots, sizeof *e->keys);
    e->vals = calloc(e->slots, sizeof *e->vals);
    if (!e->keys || !e->vals) abort();
    for (size_t i = 0; i < old; i++) {
        if (!keys[i]) continue;
        size_t j = hash_ptr(keys[i], e->slots);
        while (e->keys[j]) j = (j + 1) & (e->slots - 1);
        e->keys[j] = keys[i];
        e->vals[j] = vals[i];
    }
    free(keys);
    free(vals);
}

/* An encoded reference: odd (pointers are even), the object's index and the offset into it. */
static uint64_t encode_ref(Enc *e, void *p) {
    int64_t size;
    const PplLayout *layout;
    char *data = ppl_gc_find_object(p, &size, &layout);
    /* Not a heap object (a literal, or no pointer at all): kept as it is. */
    if (!data) return (uint64_t)(uintptr_t)p;
    uint32_t idx = object_index(e, data, size, layout);
    return ((uint64_t)idx << 32) | ((uint64_t)((char *)p - data) << 1) | 1;
}

/* Copies `bytes` of values laid out as `l` to the buffer, references encoded. */
static void encode_values(Enc *e, const char *src, int64_t bytes, const PplLayout *l) {
    size_t at = e->len;
    put(e, src, (size_t)bytes);
    if (!l) return;
    for (int64_t base = 0; base + l->size <= bytes; base += l->size) {
        for (int32_t i = 0; i < l->nrefs + l->nweak; i++) {
            size_t off = (size_t)base + (l->refs[i] & ~PPL_REF_HIDDEN);
            void *p;
            memcpy(&p, e->buf + at + off, sizeof p);
            /* Weak references aren't sent. */
            uint64_t w = i < l->nrefs && p ? encode_ref(e, p) : 0;
            memcpy(e->buf + at + off, &w, sizeof w);
        }
    }
}

/* `ArrayBuffer`s (spec §10.3) are objects `{ class, handle, byteLength }` whose bytes live
 * off the heap at `handle`. Generated code defines this as their class metadata. */
__attribute__((weak)) const void *const ppl_arraybuffer_class = NULL;

int64_t ppl_buf_op(int64_t op, int64_t a, int64_t b, int64_t c);

static int transferred(const PplArray *transfer, int64_t h) {
    if (!transfer) return 0;
    const int64_t *hs = (const int64_t *)transfer->data;
    for (int64_t i = 0; i < transfer->len; i++) {
        if (hs[i] == h) return 1;
    }
    return 0;
}

/* The message: [n objects][value][objects: layout, size, bytes]… An `ArrayBuffer` not in
 * `transfer` gets a copy of its bytes; a transferred one keeps its handle (moved). */
static char *encode(const void *slot, int64_t size, const PplLayout *layout, size_t *len,
                    const PplArray *transfer) {
    Enc e = {0};
    uint64_t n = 0;
    put(&e, &n, sizeof n);
    encode_values(&e, slot, size, layout);
    for (size_t k = 0; k < e.nobjs; k++) {
        const PplLayout *l = e.objs[k].layout;
        int64_t sz = e.objs[k].size;
        put(&e, &l, sizeof l);
        put(&e, &sz, sizeof sz);
        size_t at = e.len;
        encode_values(&e, e.objs[k].data, sz, l);
        const void *cls;
        memcpy(&cls, e.objs[k].data, sizeof cls);
        if (ppl_arraybuffer_class && cls == ppl_arraybuffer_class && sz >= 24) {
            int64_t h, bytes;
            memcpy(&h, e.buf + at + 8, sizeof h);
            memcpy(&bytes, e.buf + at + 16, sizeof bytes);
            if (h && !transferred(transfer, h)) {
                int64_t copy = ppl_buf_op(3, h, 0, bytes);
                memcpy(e.buf + at + 8, &copy, sizeof copy);
            }
        }
    }
    n = e.nobjs;
    memcpy(e.buf, &n, sizeof n);
    free(e.keys);
    free(e.vals);
    free(e.objs);
    *len = e.len;
    return e.buf;
}

static void *decode_ref(uint64_t w, char **objs) {
    if (!(w & 1)) return (void *)(uintptr_t)w;
    return objs[w >> 32] + ((w & 0xffffffffu) >> 1);
}

static void fix_values(char *dst, int64_t bytes, const PplLayout *l, char **objs) {
    if (!l) return;
    for (int64_t base = 0; base + l->size <= bytes; base += l->size) {
        for (int32_t i = 0; i < l->nrefs + l->nweak; i++) {
            size_t off = (size_t)base + (l->refs[i] & ~PPL_REF_HIDDEN);
            uint64_t w;
            memcpy(&w, dst + off, sizeof w);
            void *p = decode_ref(w, objs);
            memcpy(dst + off, &p, sizeof p);
        }
    }
}

/* Allocates the message's objects in this thread's heap and writes the value to `slot`.
 * (Allocation never collects, so nothing moves or dies meanwhile.) */
static void decode(const char *buf, void *slot, int64_t size, const PplLayout *layout) {
    uint64_t n;
    memcpy(&n, buf, sizeof n);
    const char *value = buf + sizeof n;
    const char *p = value + size;
    char **objs = n ? malloc(n * sizeof *objs) : NULL;
    const char **srcs = n ? malloc(n * sizeof *srcs) : NULL;
    if (n && (!objs || !srcs)) abort();
    for (uint64_t k = 0; k < n; k++) {
        const PplLayout *l;
        int64_t sz;
        memcpy(&l, p, sizeof l);
        memcpy(&sz, p + sizeof l, sizeof sz);
        srcs[k] = p + sizeof l + sizeof sz;
        objs[k] = ppl_gc_alloc(sz, l);
        p = srcs[k] + sz;
    }
    for (uint64_t k = 0; k < n; k++) {
        int64_t sz;
        const PplLayout *l;
        memcpy(&l, srcs[k] - sizeof sz - sizeof l, sizeof l);
        memcpy(&sz, srcs[k] - sizeof sz, sizeof sz);
        memcpy(objs[k], srcs[k], (size_t)sz);
        fix_values(objs[k], sz, l, objs);
    }
    memcpy(slot, value, (size_t)size);
    fix_values(slot, size, layout, objs);
    free(objs);
    free(srcs);
}

/* ---------------------------------------------------------------- threads */

static Thread *thread_of(int64_t h) {
    if (h < 1 || h > atomic_load(&nthreads)) return NULL;
    return table[h - 1];
}

static void *thread_main(void *arg) {
    Thread *t = arg;
    self_thread = t;
    t->waker = ppl_loop_waker();
    atomic_store(&t->requested, &ppl_gc_requested);
    atomic_store(&t->started, 1);
    if (setjmp(worker_exit) == 0) {
        ppl_worker_main(t->entry);
    }
    fflush(stdout);
    if (!atomic_load(&t->terminate)) {
        chan_push(&t->to_parent, MSG_EXIT, NULL, 0);
        ppl_loop_wake(t->parent_waker);
    }
    atomic_store(&t->requested, NULL);
    return NULL;
}

/* A panic or an uncaught error in a worker: tells the parent and ends the thread. Returns
 * if this is the main thread. */
void ppl_worker_failed(const char *msg, int64_t len) {
    Thread *t = self_thread;
    if (!t) return;
    fflush(stdout);
    char *copy = malloc((size_t)len + 1);
    if (!copy) abort();
    memcpy(copy, msg, (size_t)len);
    chan_push(&t->to_parent, MSG_ERROR, copy, (size_t)len);
    ppl_loop_wake(t->parent_waker);
    atomic_store(&t->terminate, 1); /* (no exit message after the error) */
    longjmp(worker_exit, 1);
}

/* Called at safepoints and when the loop wakes: a terminated worker stops right there. */
void ppl_worker_check(void) {
    Thread *t = self_thread;
    if (t && atomic_load(&t->terminate)) longjmp(worker_exit, 1);
}

static int64_t spawn(int32_t entry) {
    Thread *t = calloc(1, sizeof *t);
    if (!t) abort();
    t->entry = entry;
    chan_init(&t->to_worker);
    chan_init(&t->to_parent);
    t->parent_waker = ppl_loop_waker();
    t->live = 1;
    int i = atomic_fetch_add(&nthreads, 1);
    if (i >= MAX_WORKERS) {
        fflush(stdout);
        fputs("fatal: too many workers\n", stderr);
        abort();
    }
    table[i] = t;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, (size_t)64 << 20);
    if (pthread_create(&t->th, &attr, thread_main, t) != 0) {
        fflush(stdout);
        perror("fatal: worker thread");
        abort();
    }
    pthread_attr_destroy(&attr);
    pthread_detach(t->th);
    return i + 1;
}

/* The channel messages for this thread arrive on from `h` (0: the parent). */
static Chan *inbox(int64_t h) {
    if (h == 0) return self_thread ? &self_thread->to_worker : NULL;
    Thread *t = thread_of(h);
    return t && t->live ? &t->to_parent : NULL;
}

/* The workers this thread started, and whether its own inbox has something. */
static _Thread_local int64_t *children;
static _Thread_local size_t nchildren, capchildren;

int64_t ppl_worker_op(int64_t op, int64_t arg) {
    switch (op) {
    case 1: { /* spawn worker module `arg` */
        int64_t h = spawn((int32_t)arg);
        if (nchildren == capchildren) {
            capchildren = capchildren ? capchildren * 2 : 8;
            children = realloc(children, capchildren * sizeof *children);
            if (!children) abort();
        }
        children[nchildren++] = h;
        return h;
    }
    case 2: { /* a channel with a message waiting (0: from the parent), or -1 */
        ppl_worker_check();
        Chan *own = inbox(0);
        if (own && chan_peek(own)) return 0;
        for (size_t i = 0; i < nchildren; i++) {
            Chan *c = inbox(children[i]);
            if (c && chan_peek(c)) return children[i];
        }
        return -1;
    }
    case 3: { /* the kind of channel `arg`'s next message (-1: none) */
        Chan *c = inbox(arg);
        Msg *m = c ? chan_peek(c) : NULL;
        return m ? m->kind : -1;
    }
    case 4: { /* terminate worker `arg` */
        Thread *t = thread_of(arg);
        if (!t || !t->live) return 0;
        t->live = 0;
        atomic_store(&t->terminate, 1);
        int32_t *req = atomic_load(&t->requested);
        if (req) __atomic_store_n(req, 1, __ATOMIC_RELAXED);
        if (atomic_load(&t->started)) ppl_loop_wake(t->waker);
        return 1;
    }
    case 5: { /* drop channel `arg`'s next message (an exit: the worker is gone) */
        Chan *c = inbox(arg);
        Msg *m = c ? chan_pop(c) : NULL;
        if (m) {
            if (m->kind != MSG_DATA && arg != 0) {
                Thread *t = thread_of(arg);
                if (t) t->live = 0;
            }
            free(m->data);
            free(m);
        }
        return 0;
    }
    case 6: { /* the live workers this thread started */
        int64_t n = 0;
        for (size_t i = 0; i < nchildren; i++) {
            Thread *t = thread_of(children[i]);
            if (t && t->live && !t->unref) n++;
        }
        return n;
    }
    case 7: /* whether this thread is a worker */
        return self_thread != NULL;
    case 8: { /* spawn an inline worker (it waits for its setup message) */
        int64_t h = ppl_worker_op(1, 0);
        return h;
    }
    case 9:  /* worker `arg` doesn't keep this thread's loop running */
    case 10: { /* it does again */
        Thread *t = thread_of(arg);
        if (t) t->unref = op == 9;
        return 0;
    }
    case 11: { /* the number of processors */
        long n = sysconf(_SC_NPROCESSORS_ONLN);
        return n > 0 ? n : 1;
    }
    default:
        return 0;
    }
}

void ppl_worker_post(int64_t h, const void *slot, int64_t size, const PplLayout *layout,
                     const struct PplArray *transfer) {
    Chan *c;
    void *waker;
    if (h == 0) {
        if (!self_thread) return; /* (the main thread has no parent) */
        c = &self_thread->to_parent;
        waker = self_thread->parent_waker;
    } else {
        Thread *t = thread_of(h);
        if (!t || !t->live) return;
        c = &t->to_worker;
        /* Not running yet: it reads its inbox once its loop starts. (`started` is set after
         * `waker`.) */
        waker = atomic_load(&t->started) ? t->waker : NULL;
    }
    size_t len;
    char *buf = encode(slot, size, layout, &len, transfer);
    chan_push(c, MSG_DATA, buf, len);
    if (waker) ppl_loop_wake(waker);
}

void ppl_worker_take(int64_t h, void *slot, int64_t size, const PplLayout *layout) {
    Chan *c = inbox(h);
    Msg *m = c ? chan_pop(c) : NULL;
    if (!m || m->kind != MSG_DATA) {
        memset(slot, 0, (size_t)size);
        return;
    }
    decode(m->data, slot, size, layout);
    free(m->data);
    free(m);
}

/* The error text of channel `h`'s next message (a panicked worker), as a ppl string. */
PplStr *ppl_worker_error(int64_t h) {
    Chan *c = inbox(h);
    Msg *m = c ? chan_pop(c) : NULL;
    if (!m) return ppl_str_new("", 0);
    Thread *t = thread_of(h);
    if (t) t->live = 0;
    PplStr *s = ppl_str_new(m->data ? m->data : "", (int64_t)m->len);
    free(m->data);
    free(m);
    return s;
}

/* ---------------------------------------------------------------- ArrayBuffer memory */

/* 1 allocate `a` zeroed bytes (a handle), 2 free `a`, 3 a copy of `c` bytes of `a` from
 * offset `b`, 11–18 read a little-endian unsigned integer of 1–8 bytes at `a + b`, 21–28
 * write `c` as one. (Bounds are checked by the ppl side.) */
int64_t ppl_buf_op(int64_t op, int64_t a, int64_t b, int64_t c) {
    char *p = (char *)(intptr_t)a;
    if (op == 1) {
        void *m = calloc(a > 0 ? (size_t)a : 1, 1);
        if (!m) abort();
        return (int64_t)(intptr_t)m;
    }
    if (op == 2) {
        free(p);
        return 0;
    }
    if (op == 3) {
        char *m = malloc(c > 0 ? (size_t)c : 1);
        if (!m) abort();
        if (c > 0) memcpy(m, p + b, (size_t)c);
        return (int64_t)(intptr_t)m;
    }
    if (op >= 11 && op <= 18) {
        uint64_t x = 0;
        for (int k = (int)(op - 10) - 1; k >= 0; k--) x = (x << 8) | (unsigned char)p[b + k];
        return (int64_t)x;
    }
    if (op >= 21 && op <= 28) {
        for (int k = 0; k < op - 20; k++) p[b + k] = (char)((uint64_t)c >> (8 * k));
        return 0;
    }
    return 0;
}
