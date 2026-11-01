/* The ppl heap (spec §9): allocation, roots and the collector. */
#include "ppl_rt.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

PplFrame *ppl_gc_top;
int32_t ppl_gc_requested;
static const PplGlobalRoots *globals;

/* ---------------------------------------------------------------- heap layout
 *
 * The heap is a set of 64 KiB blocks. A small block holds cells of one size class, each an
 * object header (`Obj`) followed by the payload; cells are handed out from a bump pointer
 * while the block is fresh, then from a free list rebuilt by sweeping. A large object gets
 * its own run of blocks. A table maps block numbers (address >> 16) to their `Block`, so
 * the collector can tell heap pointers (also into the middle of an object) from anything
 * else. Nothing ever moves. */

#define BLOCK_SHIFT 16
#define BLOCK_SIZE ((size_t)1 << BLOCK_SHIFT)
#define MAX_SMALL 8192
/* Empty blocks kept for reuse instead of being returned to the system. */
#define SPARE_BLOCKS 16

typedef struct Obj {
    const PplLayout *layout; /* NULL: no references */
    uint32_t size;           /* payload bytes */
    uint8_t flags;
    uint8_t pad[3];
} Obj;

enum { F_ALLOC = 1, F_MARK = 2 };

typedef struct Block {
    struct Block *next, *prev; /* every block */
    struct Block *avail_next;  /* the size class's blocks with free cells */
    char *cells, *bump, *end;
    Obj *free;
    size_t cell;  /* cell size; 0 for a large object */
    size_t bytes; /* the allocation's size */
    int64_t live; /* allocated cells */
    int cls;
    int in_avail;
} Block;

static const uint32_t class_sizes[] = {32,   48,   64,   80,   96,   112,  128,  160,  192,
                                       224,  256,  320,  384,  448,  512,  640,  768,  1024,
                                       1280, 1536, 2048, 2560, 3072, 4096, 5120, 6144, 8192};
#define NCLASSES ((int)(sizeof class_sizes / sizeof class_sizes[0]))

static struct {
    Block *cur;   /* allocating from */
    Block *avail; /* other blocks with room */
} classes[NCLASSES];
static uint8_t class_of_granule[MAX_SMALL / 16 + 1];
static int classes_ready;

static Block *all_blocks;
static Block *spare; /* empty small blocks (linked through `next`) */
static int nspare;

/* Statistics (the `GcStats` fields). */
static int64_t heap_bytes, used_bytes, allocated_bytes, ncollections, nyoung;
static int64_t last_pause, max_pause, total_pause;
/* A full collection is due when `used_bytes` reaches this. */
#define MIN_THRESHOLD ((int64_t)8 << 20)
static int64_t threshold = MIN_THRESHOLD;
static int paused;
static int stress_mode;

static _Noreturn void out_of_memory(void) {
    fflush(stdout);
    fputs("fatal: out of memory\n", stderr);
    abort();
}

static size_t header_size(void) { return (sizeof(Block) + 15) & ~(size_t)15; }

/* ---------------------------------------------------------------- the block table */

static struct {
    uintptr_t *keys; /* 0: empty, 1: deleted */
    Block **vals;
    size_t cap, used;
} table;

static size_t slot_of(uintptr_t k, size_t cap) {
    return (size_t)((k * 0x9E3779B97F4A7C15ull) >> 32) & (cap - 1);
}

static void table_put(uintptr_t k, Block *b);

static void table_grow(void) {
    uintptr_t *keys = table.keys;
    Block **vals = table.vals;
    size_t cap = table.cap;
    table.cap = cap ? cap * 2 : 1024;
    table.keys = calloc(table.cap, sizeof *table.keys);
    table.vals = calloc(table.cap, sizeof *table.vals);
    if (!table.keys || !table.vals) out_of_memory();
    table.used = 0;
    for (size_t i = 0; i < cap; i++) {
        if (keys[i] > 1) table_put(keys[i], vals[i]);
    }
    free(keys);
    free(vals);
}

static void table_put(uintptr_t k, Block *b) {
    if ((table.used + 1) * 2 > table.cap) table_grow();
    size_t i = slot_of(k, table.cap);
    while (table.keys[i] > 1) i = (i + 1) & (table.cap - 1);
    if (table.keys[i] == 0) table.used++;
    table.keys[i] = k;
    table.vals[i] = b;
}

static Block *table_get(uintptr_t k) {
    if (!table.cap) return NULL;
    size_t i = slot_of(k, table.cap);
    while (table.keys[i] != 0) {
        if (table.keys[i] == k) return table.vals[i];
        i = (i + 1) & (table.cap - 1);
    }
    return NULL;
}

static void table_del(uintptr_t k) {
    size_t i = slot_of(k, table.cap);
    while (table.keys[i] != 0) {
        if (table.keys[i] == k) {
            table.keys[i] = 1;
            return;
        }
        i = (i + 1) & (table.cap - 1);
    }
}

/* The object containing `p`, or NULL when `p` isn't inside a live heap object. */
static Obj *find_object(const void *p) {
    Block *b = table_get((uintptr_t)p >> BLOCK_SHIFT);
    if (!b) return NULL;
    const char *c = p;
    if (c < b->cells) return NULL;
    Obj *o;
    if (b->cell == 0) {
        if (c >= b->end) return NULL;
        o = (Obj *)b->cells;
    } else {
        if (c >= b->bump) return NULL;
        o = (Obj *)(b->cells + (size_t)(c - b->cells) / b->cell * b->cell);
    }
    return (o->flags & F_ALLOC) ? o : NULL;
}

/* ---------------------------------------------------------------- blocks */

static void link_block(Block *b) {
    b->prev = NULL;
    b->next = all_blocks;
    if (all_blocks) all_blocks->prev = b;
    all_blocks = b;
}

static void unlink_block(Block *b) {
    if (b->prev) {
        b->prev->next = b->next;
    } else {
        all_blocks = b->next;
    }
    if (b->next) b->next->prev = b->prev;
}

static Block *new_block(int cls) {
    Block *b = spare;
    if (b) {
        spare = b->next;
        nspare--;
    } else {
        b = aligned_alloc(BLOCK_SIZE, BLOCK_SIZE);
        if (!b) out_of_memory();
        b->bytes = BLOCK_SIZE;
        heap_bytes += BLOCK_SIZE;
        table_put((uintptr_t)b >> BLOCK_SHIFT, b);
    }
    b->cls = cls;
    b->cell = class_sizes[cls];
    b->cells = (char *)b + header_size();
    b->bump = b->cells;
    b->end = b->cells + (BLOCK_SIZE - header_size()) / b->cell * b->cell;
    b->free = NULL;
    b->live = 0;
    b->in_avail = 0;
    b->avail_next = NULL;
    link_block(b);
    return b;
}

/* An empty small block goes back to the spare list (or the system). */
static void retire_block(Block *b) {
    unlink_block(b);
    if (nspare < SPARE_BLOCKS) {
        b->next = spare;
        spare = b;
        nspare++;
        return;
    }
    table_del((uintptr_t)b >> BLOCK_SHIFT);
    heap_bytes -= (int64_t)b->bytes;
    free(b);
}

static void free_large(Block *b) {
    unlink_block(b);
    for (size_t i = 0; i < b->bytes / BLOCK_SIZE; i++) {
        table_del(((uintptr_t)b >> BLOCK_SHIFT) + i);
    }
    heap_bytes -= (int64_t)b->bytes;
    free(b);
}

static void init_classes(void) {
    int c = 0;
    for (int g = 0; g <= MAX_SMALL / 16; g++) {
        while ((uint32_t)g * 16 > class_sizes[c]) c++;
        class_of_granule[g] = (uint8_t)c;
    }
    classes_ready = 1;
    /* Debugging: PPL_GC_STRESS=1 collects at every safepoint after an allocation. */
    const char *stress = getenv("PPL_GC_STRESS");
    if (stress && *stress && *stress != '0') {
        stress_mode = 1;
        threshold = 0;
    }
}

static Obj *alloc_small(int c) {
    Block *b = classes[c].cur;
    for (;;) {
        if (b) {
            if (b->free) {
                Obj *o = b->free;
                memcpy(&b->free, (const void *)(o + 1), sizeof(Obj *));
                b->live++;
                return o;
            }
            if (b->bump + b->cell <= b->end) {
                Obj *o = (Obj *)b->bump;
                b->bump += b->cell;
                b->live++;
                return o;
            }
        }
        b = classes[c].avail;
        if (b) {
            classes[c].avail = b->avail_next;
            b->in_avail = 0;
        } else {
            b = new_block(c);
        }
        classes[c].cur = b;
    }
}

static Obj *alloc_large(size_t payload) {
    size_t bytes = (header_size() + sizeof(Obj) + payload + BLOCK_SIZE - 1) & ~(BLOCK_SIZE - 1);
    Block *b = aligned_alloc(BLOCK_SIZE, bytes);
    if (!b) out_of_memory();
    memset(b, 0, sizeof *b);
    b->bytes = bytes;
    b->cells = (char *)b + header_size();
    b->bump = b->cells;
    b->end = b->cells + sizeof(Obj) + payload;
    b->live = 1;
    heap_bytes += (int64_t)bytes;
    for (size_t i = 0; i < bytes / BLOCK_SIZE; i++) {
        table_put(((uintptr_t)b >> BLOCK_SHIFT) + i, b);
    }
    link_block(b);
    return (Obj *)b->cells;
}

/* ---------------------------------------------------------------- allocation */

void *ppl_gc_alloc(int64_t size, const PplLayout *layout) {
    if (size < 0) size = 0;
    if (size > (int64_t)UINT32_MAX) out_of_memory();
    if (!classes_ready) init_classes();
    size_t need = sizeof(Obj) + (((size_t)size + 15) & ~(size_t)15);
    size_t cell;
    Obj *o;
    if (need <= MAX_SMALL) {
        int c = class_of_granule[need / 16];
        o = alloc_small(c);
        cell = class_sizes[c];
    } else {
        o = alloc_large((size_t)size);
        cell = sizeof(Obj) + (size_t)size;
    }
    o->layout = layout;
    o->size = (uint32_t)size;
    o->flags = F_ALLOC;
    memset(o + 1, 0, (size_t)size);
    used_bytes += (int64_t)cell;
    allocated_bytes += (int64_t)cell;
    if (used_bytes >= threshold && !paused) ppl_gc_requested = 1;
    return o + 1;
}

void *ppl_alloc(int64_t size) { return ppl_gc_alloc(size, NULL); }

void *ppl_alloc_zeroed(int64_t size) { return ppl_gc_alloc(size, NULL); }

void ppl_gc_write(void *slot) { (void)slot; }

/* ---------------------------------------------------------------- roots */

void ppl_gc_set_globals(const PplGlobalRoots *g) { globals = g; }

/* Whether a root holds a reference that `gc.dumpRoots()` shows. */
static int visible_refs(const char *base, const PplLayout *l) {
    for (int32_t i = 0; i < l->nrefs; i++) {
        uint32_t r = l->refs[i];
        if (r & PPL_REF_HIDDEN) continue;
        void *p;
        memcpy(&p, base + r, sizeof p);
        if (p) return 1;
    }
    return 0;
}

static void dump_root(const char *title, const char *name, const char *at,
                      const PplLayout *layout, int *any) {
    if (!name || !visible_refs(at, layout)) return;
    if (*any) {
        printf(", %s", name);
    } else {
        printf("  %s: %s", title, name);
    }
    *any = 1;
}

void ppl_gc_dump_roots(void) {
    printf("gc roots:\n");
    for (PplFrame *f = ppl_gc_top; f; f = f->prev) {
        int any = 0;
        for (int64_t i = 0; i < f->map->nroots; i++) {
            const PplRootDesc *r = &f->map->roots[i];
            dump_root(f->map->fn, r->name, (const char *)f + r->offset, r->layout, &any);
        }
        if (any) putchar('\n');
    }
    if (globals) {
        int any = 0;
        for (int64_t i = 0; i < globals->n; i++) {
            const PplGlobalRoot *r = &globals->roots[i];
            dump_root("globals", r->name, r->addr, r->layout, &any);
        }
        if (any) putchar('\n');
    }
}

/* ---------------------------------------------------------------- marking */

static Obj **mark_stack;
static size_t mark_len, mark_cap;

static void push(Obj *o) {
    if (mark_len == mark_cap) {
        mark_cap = mark_cap ? mark_cap * 2 : 4096;
        mark_stack = realloc(mark_stack, mark_cap * sizeof *mark_stack);
        if (!mark_stack) out_of_memory();
    }
    mark_stack[mark_len++] = o;
}

static void mark_ptr(const void *p) {
    Obj *o = find_object(p);
    if (!o || (o->flags & F_MARK)) return;
    o->flags |= F_MARK;
    push(o);
}

/* Marks what the reference words of `bytes` bytes of values at `base` point to. */
static void scan(const char *base, const PplLayout *l, int64_t bytes) {
    if (!l) return;
    for (int64_t at = 0; at + l->size <= bytes; at += l->size) {
        for (int32_t i = 0; i < l->nrefs; i++) {
            void *p;
            memcpy(&p, base + at + (l->refs[i] & ~PPL_REF_HIDDEN), sizeof p);
            if (p) mark_ptr(p);
        }
    }
}

static void scan_roots(void) {
    for (PplFrame *f = ppl_gc_top; f; f = f->prev) {
        for (int64_t i = 0; i < f->map->nroots; i++) {
            const PplRootDesc *r = &f->map->roots[i];
            scan((const char *)f + r->offset, r->layout, r->layout->size);
        }
    }
    if (globals) {
        for (int64_t i = 0; i < globals->n; i++) {
            const PplGlobalRoot *r = &globals->roots[i];
            scan(r->addr, r->layout, r->layout->size);
        }
    }
}

static void drain(void) {
    while (mark_len > 0) {
        Obj *o = mark_stack[--mark_len];
        scan((const char *)(o + 1), o->layout, o->size);
    }
}

/* ---------------------------------------------------------------- sweeping */

/* Frees the unmarked objects and clears the marks; rebuilds the size classes' lists. */
static void sweep(void) {
    for (int c = 0; c < NCLASSES; c++) classes[c].avail = NULL;
    Block *b = all_blocks;
    while (b) {
        Block *next = b->next;
        if (b->cell == 0) {
            Obj *o = (Obj *)b->cells;
            if (o->flags & F_MARK) {
                o->flags &= (uint8_t)~F_MARK;
            } else {
                used_bytes -= (int64_t)(b->end - b->cells);
                free_large(b);
            }
            b = next;
            continue;
        }
        for (char *c = b->cells; c < b->bump; c += b->cell) {
            Obj *o = (Obj *)c;
            if (!(o->flags & F_ALLOC)) continue;
            if (o->flags & F_MARK) {
                o->flags &= (uint8_t)~F_MARK;
                continue;
            }
            o->flags = 0;
            memcpy((void *)(o + 1), &b->free, sizeof(Obj *));
            b->free = o;
            b->live--;
            used_bytes -= (int64_t)b->cell;
        }
        b->in_avail = 0;
        if (b->live == 0) {
            /* Fresh again: bump allocation from the start. */
            b->free = NULL;
            b->bump = b->cells;
            if (classes[b->cls].cur != b) retire_block(b);
        } else if (classes[b->cls].cur != b && (b->free || b->bump < b->end)) {
            b->avail_next = classes[b->cls].avail;
            classes[b->cls].avail = b;
            b->in_avail = 1;
        }
        b = next;
    }
}

static int64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

static void record_pause(int64_t start) {
    int64_t d = now_ns() - start;
    last_pause = d;
    if (d > max_pause) max_pause = d;
    total_pause += d;
}

static void full_collection(void) {
    int64_t start = now_ns();
    scan_roots();
    drain();
    sweep();
    ncollections++;
    threshold = used_bytes * 2 > MIN_THRESHOLD ? used_bytes * 2 : MIN_THRESHOLD;
    if (stress_mode) threshold = 0;
    record_pause(start);
}

void ppl_gc_safepoint(void) {
    ppl_gc_requested = 0;
    if (paused) return;
    if (used_bytes >= threshold) full_collection();
}

/* ---------------------------------------------------------------- std/gc */

void ppl_gc_collect(void) {
    ppl_gc_requested = 0;
    full_collection();
}

void ppl_gc_collect_young(void) { ppl_gc_collect(); }

void ppl_gc_pause(void) { paused++; }

void ppl_gc_resume(void) {
    if (paused > 0) paused--;
    if (!paused && used_bytes >= threshold) ppl_gc_requested = 1;
}

int64_t ppl_gc_stat(int64_t which) {
    switch (which) {
    case 0: return heap_bytes;
    case 1: return used_bytes;
    case 2: return allocated_bytes;
    case 3: return ncollections;
    case 4: return nyoung;
    case 5: return last_pause;
    case 6: return max_pause;
    case 7: return total_pause;
    default: return 0;
    }
}
