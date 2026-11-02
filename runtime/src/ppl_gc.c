/* The ppl heap (spec §9): allocation, roots and the collector. */
#define _POSIX_C_SOURCE 200809L
#include "ppl_rt.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

_Thread_local PplFrame *ppl_gc_top;
_Thread_local int32_t ppl_gc_requested;
static _Thread_local const PplGlobalRoots *globals;

/* ---------------------------------------------------------------- heap layout
 *
 * The heap is a set of 64 KiB blocks. A small block holds cells of one size class, each an
 * object header (`Obj`) followed by the payload; cells are handed out from a bump pointer
 * while the block is fresh, then from a free list rebuilt by sweeping. A large object gets
 * its own run of blocks. A table maps block numbers (address >> 16) to their `Block`, so
 * the collector can tell heap pointers (also into the middle of an object) from anything
 * else. Nothing ever moves.
 *
 * Generations without moving ("sticky" marks): objects are young until they survive a
 * nursery collection, which marks from the roots and from the remembered set, never entering
 * old objects, and sweeps only the objects allocated since (kept in a list). Survivors become
 * old in place. The write barrier remembers old small objects that are written to, and the
 * written 512-byte cards of old large objects (big arrays), so a nursery collection scans
 * only those.
 *
 * The old generation is collected incrementally, a slice at a safepoint at a time. Marking
 * colors reached objects with the cycle's color (two colors alternate, so marks never need
 * clearing); the barrier shades what a store into an old object makes it point to (Dijkstra),
 * objects promoted meanwhile join grey, large objects are scanned in chunks, and the short
 * finishing pause runs a nursery collection, rescans the roots and drains the mark stack.
 * Sweeping is incremental too: objects promoted while it runs get the color that survives.
 *
 * Weak references: objects whose layout has weak words are kept in a list. Weak words aren't
 * traced; once a nursery collection has marked, the weak words pointing to young objects it
 * didn't reach are cleared, and once a major marking has finished, those pointing to old
 * objects without the cycle's color. Both happen before the targets are swept, so a cleared
 * address is never reused while a weak word still holds it.
 *
 * Scoped arenas (spec §9.4) are chunks of 64 KiB-aligned memory in the block table (`cls`
 * -1, `live` = the arena's level) that the collector never marks or sweeps: allocations
 * bump through them while the arena is open, their objects are roots (they may point to heap
 * objects), and closing the arena frees the chunks. References must not outlive it: the
 * compiler rejects the visible escapes, the write barrier panics when one is stored into an
 * older object, and closing panics when a frame or a global still holds one. */

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

enum {
    F_ALLOC = 1,
    F_MARK = 2, /* reached by the running nursery collection */
    F_OLD = 4,
    F_REM = 8, /* in the remembered set */
    /* An old object's color: the major cycle that last reached it. */
    F_COLOR_A = 16,
    F_COLOR_B = 32,
    F_COLORS = F_COLOR_A | F_COLOR_B,
    F_ARENA = 64 /* in a scoped arena */
};

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
    uint8_t *cards; /* large objects: the written cards, once old */
} Block;

static const uint32_t class_sizes[] = {32,   48,   64,   80,   96,   112,  128,  160,  192,
                                       224,  256,  320,  384,  448,  512,  640,  768,  1024,
                                       1280, 1536, 2048, 2560, 3072, 4096, 5120, 6144, 8192};
#define NCLASSES ((int)(sizeof class_sizes / sizeof class_sizes[0]))

static _Thread_local struct {
    Block *cur;   /* allocating from */
    Block *avail; /* other blocks with room */
} classes[NCLASSES];
static _Thread_local uint8_t class_of_granule[MAX_SMALL / 16 + 1];
static _Thread_local int classes_ready;

static _Thread_local Block *all_blocks;
static _Thread_local Block *spare; /* empty small blocks (linked through `next`) */
static _Thread_local int nspare;

/* Statistics (the `GcStats` fields). */
static _Thread_local int64_t heap_bytes, used_bytes, allocated_bytes, ncollections, nyoung;
static _Thread_local int64_t last_pause, max_pause, total_pause;

/* A nursery collection is due after this many bytes of allocation. */
#define NURSERY ((int64_t)256 << 10)
/* Old-generation marking starts when the old bytes reach this (at least the minimum, else
 * twice what survived the last major collection). */
#define MIN_MAJOR ((int64_t)8 << 20)
/* While marking or sweeping, a step runs after this much allocation and works for at most
 * STEP_NS. */
#define STEP_BYTES ((int64_t)64 << 10)
#define STEP_NS ((int64_t)250000)
/* Large objects are marked this many bytes at a time, and remembered by cards. */
#define CHUNK_BYTES ((int64_t)16 << 10)
#define CARD_SHIFT 9
static _Thread_local int64_t young_bytes, old_bytes, step_bytes;
static _Thread_local int64_t major_threshold = MIN_MAJOR;
enum { IDLE, MARKING, SWEEPING };
static _Thread_local int phase = IDLE;
static _Thread_local uint8_t color = F_COLOR_A; /* of the running (or last) major cycle */
static _Thread_local int paused;
static _Thread_local int stress_mode;
static _Thread_local int trace_mode;

static _Noreturn void out_of_memory(void) {
    fflush(stdout);
    fputs("fatal: out of memory\n", stderr);
    abort();
}

static size_t header_size(void) { return (sizeof(Block) + 15) & ~(size_t)15; }

/* ---------------------------------------------------------------- the block table
 *
 * A two-level radix table from block number to block: the high bits pick a leaf of 2^16
 * entries (4 GiB of address space), created when first needed. */

#define LEAF_BITS 16
#define ROOT_BITS 16 /* 48-bit addresses */
/* Per thread (each has its own heap): allocated on first use. */
static _Thread_local Block ***block_table;

static void table_put(uintptr_t k, Block *b) {
    size_t hi = (size_t)(k >> LEAF_BITS) & (((size_t)1 << ROOT_BITS) - 1);
    if (!block_table) {
        block_table = calloc((size_t)1 << ROOT_BITS, sizeof(Block **));
        if (!block_table) out_of_memory();
    }
    if (!block_table[hi]) {
        block_table[hi] = calloc((size_t)1 << LEAF_BITS, sizeof(Block *));
        if (!block_table[hi]) out_of_memory();
    }
    block_table[hi][k & (((uintptr_t)1 << LEAF_BITS) - 1)] = b;
}

static Block *table_get(uintptr_t k) {
    if (k >> (LEAF_BITS + ROOT_BITS) || !block_table) return NULL;
    Block **leaf = block_table[k >> LEAF_BITS];
    return leaf ? leaf[k & (((uintptr_t)1 << LEAF_BITS) - 1)] : NULL;
}

static void table_del(uintptr_t k) { table_put(k, NULL); }

/* The object containing `p`, or NULL when `p` isn't inside a live heap object. */
static Obj *find_object(const void *p) {
    Block *b = table_get((uintptr_t)p >> BLOCK_SHIFT);
    if (!b || b->cls < 0) return NULL;
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

/* The next block the incremental sweep visits (declared here: freeing a block moves it). */
static _Thread_local Block *sweep_cursor;

/* An empty small block goes back to the spare list (or the system). */
static void retire_block(Block *b) {
    if (sweep_cursor == b) sweep_cursor = b->next;
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
    /* (A young large object dying while the old generation is swept.) */
    if (sweep_cursor == b) sweep_cursor = b->next;
    unlink_block(b);
    free(b->cards);
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
    const char *trace = getenv("PPL_GC_TRACE");
    trace_mode = trace && *trace && *trace != '0';
    const char *stress = getenv("PPL_GC_STRESS");
    if (stress && *stress && *stress != '0') {
        stress_mode = 1;
    }
}

static Obj *alloc_small(int c) {
    Block *b = classes[c].cur;
    for (;;) {
        if (b) {
            Obj *o = NULL;
            if (b->free) {
                o = b->free;
                memcpy(&b->free, (const void *)(o + 1), sizeof(Obj *));
            } else if (b->bump + b->cell <= b->end) {
                o = (Obj *)b->bump;
                b->bump += b->cell;
            }
            if (o) {
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

/* ---------------------------------------------------------------- scoped arenas */

static void scan(const char *base, const PplLayout *l, int64_t bytes, void (*visit)(const void *));
static void *heap_object(size_t need, int64_t size, const PplLayout *layout);

#define MAX_ARENAS 64
#define ARENA_CHUNK BLOCK_SIZE
/* Per open arena (level i + 1): its chunks, newest first (linked through `next`). */
static _Thread_local Block *arena_chunks[MAX_ARENAS];
static _Thread_local int arena_depth;

/* The level of the arena `p` points into, or 0. */
static int arena_level_of(const void *p) {
    Block *b = table_get((uintptr_t)p >> BLOCK_SHIFT);
    return b && b->cls < 0 ? (int)b->live : 0;
}

static Obj *arena_alloc(int level, size_t need) {
    Block *b = arena_chunks[level - 1];
    if (!b || b->bump + need > b->end) {
        size_t bytes = (header_size() + need + ARENA_CHUNK - 1) & ~(size_t)(ARENA_CHUNK - 1);
        b = aligned_alloc(BLOCK_SIZE, bytes);
        if (!b) out_of_memory();
        memset(b, 0, sizeof *b);
        b->cls = -1;
        b->live = level;
        b->bytes = bytes;
        b->cells = b->bump = (char *)b + header_size();
        b->end = (char *)b + bytes;
        b->next = arena_chunks[level - 1];
        arena_chunks[level - 1] = b;
        for (size_t i = 0; i < bytes / BLOCK_SIZE; i++) {
            table_put(((uintptr_t)b >> BLOCK_SHIFT) + i, b);
        }
    }
    Obj *o = (Obj *)b->bump;
    b->bump += need;
    return o;
}

/* Visits every object of the open arenas. */
static void scan_arenas(void (*visit)(const void *)) {
    for (int i = 0; i < arena_depth; i++) {
        for (Block *b = arena_chunks[i]; b; b = b->next) {
            for (char *c = b->cells; c < b->bump;) {
                const Obj *o = (const Obj *)c;
                scan((const char *)(o + 1), o->layout, o->size, visit);
                c += sizeof(Obj) + (((size_t)o->size + 15) & ~(size_t)15);
            }
        }
    }
}

void ppl_arena_enter(void) {
    if (arena_depth == MAX_ARENAS) {
        fflush(stdout);
        fputs("fatal: too many nested arenas\n", stderr);
        abort();
    }
    arena_chunks[arena_depth++] = NULL;
}

int32_t ppl_arena_level(void) { return arena_depth; }

static void forget_weak_in_arenas(int level);

void ppl_arena_release(int32_t level) {
    if (level < 0) level = 0;
    if (arena_depth <= level) return;
    forget_weak_in_arenas(level + 1);
    while (arena_depth > level) {
        Block *b = arena_chunks[--arena_depth];
        while (b) {
            Block *next = b->next;
            for (size_t i = 0; i < b->bytes / BLOCK_SIZE; i++) {
                table_del(((uintptr_t)b >> BLOCK_SHIFT) + i);
            }
            free(b);
            b = next;
        }
        arena_chunks[arena_depth] = NULL;
    }
}

static _Thread_local int escaped;
static void check_escape(const void *p) {
    if (arena_level_of(p) >= arena_depth) escaped = 1;
}

static _Noreturn void arena_escape(const char *file, int64_t file_len, int32_t line,
                                   int32_t col) {
    static const char msg[] = "a reference into an arena outlives the arena";
    ppl_panic(msg, (int64_t)sizeof msg - 1, file, file_len, line, col);
}

void ppl_arena_exit(const char *file, int64_t file_len, int32_t line, int32_t col) {
    if (arena_depth == 0) return;
    escaped = 0;
    for (PplFrame *f = ppl_gc_top; f; f = f->prev) {
        for (int64_t i = 0; i < f->map->nroots; i++) {
            const PplRootDesc *r = &f->map->roots[i];
            scan((const char *)f + r->offset, r->layout, r->layout->size, check_escape);
        }
    }
    if (globals) {
        for (int64_t i = 0; i < globals->n; i++) {
            const PplGlobalRoot *r = &globals->roots[i];
            scan(r->addr, r->layout, r->layout->size, check_escape);
        }
    }
    if (escaped) {
        ppl_arena_release(arena_depth - 1);
        arena_escape(file, file_len, line, col);
    }
    ppl_arena_release(arena_depth - 1);
}

/* Moving objects out of the closing arena (`level`): copies on the heap, remembered so
 * shared objects are copied once. */
typedef struct {
    const Obj *from;
    char *to;
} Moved;
static _Thread_local struct {
    Moved *items;
    size_t len, cap;
} moved;

/* The object of an arena at `level` or above that contains `p`. */
static Obj *arena_object_at(const void *p, int level) {
    for (int i = level - 1; i < arena_depth; i++) {
        for (Block *b = arena_chunks[i]; b; b = b->next) {
            if ((const char *)p < b->cells || (const char *)p >= b->bump) continue;
            for (char *c = b->cells; c < b->bump;) {
                Obj *o = (Obj *)c;
                char *next = c + sizeof(Obj) + (((size_t)o->size + 15) & ~(size_t)15);
                if ((const char *)p >= (const char *)(o + 1) && (const char *)p < next) return o;
                c = next;
            }
        }
    }
    return NULL;
}

static void *evacuate(void *p, int level) {
    if (!p || arena_level_of(p) < level) return p;
    Obj *o = arena_object_at(p, level);
    if (!o) return p;
    size_t offset = (size_t)((char *)p - (char *)(o + 1));
    for (size_t i = 0; i < moved.len; i++) {
        if (moved.items[i].from == o) return moved.items[i].to + offset;
    }
    size_t need = sizeof(Obj) + (((size_t)o->size + 15) & ~(size_t)15);
    char *copy = heap_object(need, o->size, o->layout);
    memcpy(copy, o + 1, o->size);
    if (moved.len == moved.cap) {
        moved.cap = moved.cap ? moved.cap * 2 : 16;
        moved.items = realloc(moved.items, moved.cap * sizeof *moved.items);
        if (!moved.items) out_of_memory();
    }
    moved.items[moved.len++] = (Moved){o, copy};
    const PplLayout *l = o->layout;
    if (l) {
        for (int64_t at = 0; at + l->size <= (int64_t)o->size; at += l->size) {
            for (int32_t i = 0; i < l->nrefs + l->nweak; i++) {
                char *slot = copy + at + (l->refs[i] & ~PPL_REF_HIDDEN);
                void *q;
                memcpy(&q, slot, sizeof q);
                /* Weak words into the arena would dangle: cleared. */
                q = i < l->nrefs ? evacuate(q, level) : (arena_level_of(q) >= level ? NULL : q);
                memcpy(slot, &q, sizeof q);
            }
        }
    }
    return copy + offset;
}

void ppl_arena_exit_rescue(void **slot, const char *file, int64_t file_len, int32_t line,
                           int32_t col) {
    if (arena_depth == 0) return;
    if (slot) *slot = evacuate(*slot, arena_depth);
    moved.len = 0;
    ppl_arena_exit(file, file_len, line, col);
}

/* ---------------------------------------------------------------- raw arenas (`Arena`) */

typedef struct RawChunk {
    struct RawChunk *next;
    size_t used, cap;
    _Alignas(16) char data[];
} RawChunk;

typedef struct {
    RawChunk *chunks;
    size_t capacity;
} RawArena;

static RawChunk *raw_chunk(size_t cap) {
    RawChunk *c = calloc(1, sizeof(RawChunk) + cap);
    if (!c) out_of_memory();
    c->cap = cap;
    return c;
}

int64_t ppl_raw_arena_new(int64_t capacity) {
    RawArena *a = calloc(1, sizeof *a);
    if (!a) out_of_memory();
    a->capacity = capacity > 0 ? (size_t)capacity : 4096;
    a->chunks = raw_chunk(a->capacity);
    return (int64_t)(intptr_t)a;
}

void *ppl_raw_arena_alloc(int64_t arena, int64_t bytes, int64_t align) {
    RawArena *a = (RawArena *)(intptr_t)arena;
    if (!a || bytes < 0) return NULL;
    size_t al = align > 0 ? (size_t)align : 1;
    RawChunk *c = a->chunks;
    size_t at = c ? (c->used + al - 1) & ~(al - 1) : 0;
    if (!c || at + (size_t)bytes > c->cap) {
        /* A new chunk: the capacity again, or more for a big request. */
        size_t cap = a->capacity > (size_t)bytes + al ? a->capacity : (size_t)bytes + al;
        c = raw_chunk(cap);
        c->next = a->chunks;
        a->chunks = c;
        at = 0;
    }
    c->used = at + (size_t)bytes;
    return c->data + at;
}

void ppl_raw_arena_free(int64_t arena) {
    RawArena *a = (RawArena *)(intptr_t)arena;
    if (!a) return;
    for (RawChunk *c = a->chunks; c;) {
        RawChunk *next = c->next;
        free(c);
        c = next;
    }
    free(a);
}

/* ---------------------------------------------------------------- allocation */

typedef struct {
    Obj **items;
    size_t len, cap;
} Stack;

static void push(Stack *st, Obj *o) {
    if (st->len == st->cap) {
        st->cap = st->cap ? st->cap * 2 : 4096;
        st->items = realloc(st->items, st->cap * sizeof *st->items);
        if (!st->items) out_of_memory();
    }
    st->items[st->len++] = o;
}

/* The objects allocated since the last nursery collection. */
static _Thread_local Stack young_objects;
/* The objects with weak words. */
static _Thread_local Stack weak_objects;

/* The block an object is in (blocks are aligned; a large object's header is in its first). */
static Block *block_of(const Obj *o) { return (Block *)((uintptr_t)o & ~(uintptr_t)(BLOCK_SIZE - 1)); }

static void *arena_object(int level, size_t need, int64_t size, const PplLayout *layout);
static void *heap_object(size_t need, int64_t size, const PplLayout *layout);

void *ppl_gc_alloc(int64_t size, const PplLayout *layout) {
    if (size < 0) size = 0;
    if (size > (int64_t)UINT32_MAX) out_of_memory();
    if (!classes_ready) init_classes();
    size_t need = sizeof(Obj) + (((size_t)size + 15) & ~(size_t)15);
    if (arena_depth > 0) return arena_object(arena_depth, need, size, layout);
    return heap_object(need, size, layout);
}

void *ppl_gc_alloc_near(const void *owner, int64_t size, const PplLayout *layout) {
    if (size < 0) size = 0;
    if (size > (int64_t)UINT32_MAX) out_of_memory();
    if (!classes_ready) init_classes();
    size_t need = sizeof(Obj) + (((size_t)size + 15) & ~(size_t)15);
    int level = arena_depth > 0 ? arena_level_of(owner) : 0;
    if (level > 0) return arena_object(level, need, size, layout);
    return heap_object(need, size, layout);
}

static void *arena_object(int level, size_t need, int64_t size, const PplLayout *layout) {
    Obj *o = arena_alloc(level, need);
    o->layout = layout;
    o->size = (uint32_t)size;
    o->flags = F_ALLOC | F_ARENA;
    memset(o + 1, 0, (size_t)size);
    if (layout && layout->nweak) push(&weak_objects, o);
    allocated_bytes += (int64_t)need;
    return o + 1;
}

static void *heap_object(size_t need, int64_t size, const PplLayout *layout) {
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
    push(&young_objects, o);
    if (layout && layout->nweak) push(&weak_objects, o);
    used_bytes += (int64_t)cell;
    allocated_bytes += (int64_t)cell;
    young_bytes += (int64_t)cell;
    if (phase != IDLE) step_bytes += (int64_t)cell;
    if (!paused && (young_bytes >= NURSERY || step_bytes >= STEP_BYTES || stress_mode)) {
        __atomic_store_n(&ppl_gc_requested, 1, __ATOMIC_RELAXED);
    }
    return o + 1;
}

void *ppl_alloc(int64_t size) { return ppl_gc_alloc(size, NULL); }

void *ppl_alloc_zeroed(int64_t size) { return ppl_gc_alloc(size, NULL); }


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

/* Nursery marking. */
static _Thread_local Stack young_stack;
/* Old small objects written to since the last nursery collection. */
static _Thread_local Stack remembered;

/* The major marking's grey objects: an object and how far it has been scanned. */
typedef struct {
    Obj *o;
    int64_t from;
} Grey;
static _Thread_local struct {
    Grey *items;
    size_t len, cap;
} grey;

static void push_grey(Obj *o, int64_t from) {
    if (grey.len == grey.cap) {
        grey.cap = grey.cap ? grey.cap * 2 : 4096;
        grey.items = realloc(grey.items, grey.cap * sizeof *grey.items);
        if (!grey.items) out_of_memory();
    }
    grey.items[grey.len++] = (Grey){o, from};
}

/* Written cards of old large objects. */
typedef struct {
    Block *b;
    size_t card;
} Card;
static _Thread_local struct {
    Card *items;
    size_t len, cap;
} dirty;

static void push_card(Block *b, size_t card) {
    if (dirty.len == dirty.cap) {
        dirty.cap = dirty.cap ? dirty.cap * 2 : 1024;
        dirty.items = realloc(dirty.items, dirty.cap * sizeof *dirty.items);
        if (!dirty.items) out_of_memory();
    }
    dirty.items[dirty.len++] = (Card){b, card};
}

/* Nursery collections enter young objects only (old ones are reached through the
 * remembered set). */
static void visit_young(const void *p) {
    Obj *o = find_object(p);
    if (!o || (o->flags & (F_OLD | F_MARK))) return;
    o->flags |= F_MARK;
    push(&young_stack, o);
}

static void color_grey(Obj *o) {
    o->flags = (uint8_t)((o->flags & ~F_COLORS) | color);
    push_grey(o, 0);
}

/* The major marking enters old objects only (young ones join when promoted). */
static void visit_old(const void *p) {
    Obj *o = find_object(p);
    if (!o || !(o->flags & F_OLD) || (o->flags & F_COLORS) == color) return;
    color_grey(o);
}

/* Visits what the reference words of the values in [from, to) at `base` point to (`base`
 * holds a whole number of `l->size`-byte values). */
static void scan_range(const char *base, const PplLayout *l, int64_t from, int64_t to,
                       void (*visit)(const void *)) {
    if (!l) return;
    for (int64_t at = from; at + l->size <= to; at += l->size) {
        for (int32_t i = 0; i < l->nrefs; i++) {
            void *p;
            memcpy(&p, base + at + (l->refs[i] & ~PPL_REF_HIDDEN), sizeof p);
            if (p) visit(p);
        }
    }
}

static void scan(const char *base, const PplLayout *l, int64_t bytes, void (*visit)(const void *)) {
    scan_range(base, l, 0, bytes, visit);
}

static void scan_object(const Obj *o, void (*visit)(const void *)) {
    scan((const char *)(o + 1), o->layout, o->size, visit);
}

static void scan_roots(void (*visit)(const void *)) {
    for (PplFrame *f = ppl_gc_top; f; f = f->prev) {
        for (int64_t i = 0; i < f->map->nroots; i++) {
            const PplRootDesc *r = &f->map->roots[i];
            scan((const char *)f + r->offset, r->layout, r->layout->size, visit);
        }
    }
    if (globals) {
        for (int64_t i = 0; i < globals->n; i++) {
            const PplGlobalRoot *r = &globals->roots[i];
            scan(r->addr, r->layout, r->layout->size, visit);
        }
    }
    scan_arenas(visit);
}

/* The write barrier, after a value laid out as `layout` was stored at `slot`: an old object
 * written to is remembered (a large one by card), and while the old generation is marked,
 * the old objects the value points to are shaded. */
static _Thread_local int barrier_level;
static void check_store(const void *p) {
    if (arena_level_of(p) > barrier_level) escaped = 1;
}

void ppl_gc_write(void *slot, const PplLayout *layout) {
    if (!layout) return;
    if (arena_depth > 0) {
        /* A reference into an arena stored where it outlives the arena. */
        barrier_level = arena_level_of(slot);
        escaped = 0;
        scan(slot, layout, layout->size, check_store);
        if (escaped) arena_escape("", 0, 0, 0);
    }
    if (old_bytes == 0) return;
    Obj *o = find_object(slot);
    if (!o || !(o->flags & F_OLD)) return;
    if (phase == MARKING) scan(slot, layout, layout->size, visit_old);
    Block *b = block_of(o);
    if (b->cell) {
        if (!(o->flags & F_REM)) {
            o->flags |= F_REM;
            push(&remembered, o);
        }
        return;
    }
    if (!b->cards) {
        b->cards = calloc(((size_t)o->size >> CARD_SHIFT) + 1, 1);
        if (!b->cards) out_of_memory();
    }
    size_t card = (size_t)((char *)slot - (char *)(o + 1)) >> CARD_SHIFT;
    if (!b->cards[card]) {
        b->cards[card] = 1;
        push_card(b, card);
    }
}

/* The values of a large object that overlap one card. */
static void scan_card(Block *b, size_t card, void (*visit)(const void *)) {
    Obj *o = (Obj *)b->cells;
    const PplLayout *l = o->layout;
    if (!l) return;
    int64_t start = (int64_t)card << CARD_SHIFT;
    int64_t end = start + ((int64_t)1 << CARD_SHIFT);
    int64_t from = start / l->size * l->size;
    int64_t to = (end + l->size - 1) / l->size * l->size;
    scan_range((const char *)(o + 1), l, from, to < o->size ? to : o->size, visit);
}

/* ---------------------------------------------------------------- sweeping */

static void free_cell(Block *b, Obj *o) {
    o->flags = 0;
    memcpy((void *)(o + 1), &b->free, sizeof(Obj *));
    b->free = o;
    b->live--;
    used_bytes -= (int64_t)b->cell;
}

static void make_available(Block *b) {
    if (b->live == 0) {
        /* Fresh again: bump allocation from the start. */
        b->free = NULL;
        b->bump = b->cells;
    }
    if (!b->in_avail && classes[b->cls].cur != b && (b->free || b->bump < b->end)) {
        b->avail_next = classes[b->cls].avail;
        classes[b->cls].avail = b;
        b->in_avail = 1;
    }
}

/* A survivor of a nursery collection becomes old, with the current cycle's color: during
 * marking it joins grey (its children may be old objects not reached yet); during sweeping
 * the color keeps it; otherwise the next cycle's color differs. */
static void promote(Obj *o, int64_t bytes) {
    o->flags = (uint8_t)((o->flags & ~(F_MARK | F_COLORS)) | F_OLD | color);
    if (phase == MARKING) push_grey(o, 0);
    old_bytes += bytes;
}

/* Frees the young objects that weren't reached and promotes the others. */
static void sweep_young(void) {
    for (size_t i = 0; i < young_objects.len; i++) {
        Obj *o = young_objects.items[i];
        Block *b = block_of(o);
        if (b->cell == 0) {
            int64_t bytes = (int64_t)(b->end - b->cells);
            if (o->flags & F_MARK) {
                promote(o, bytes);
            } else {
                used_bytes -= bytes;
                free_large(b);
            }
            continue;
        }
        if (o->flags & F_MARK) {
            promote(o, (int64_t)b->cell);
        } else {
            free_cell(b, o);
            make_available(b);
        }
    }
    young_objects.len = 0;
}

/* Frees one block's old objects that the finished marking didn't reach. */
static void sweep_block(Block *b) {
    if (b->cell == 0) {
        Obj *o = (Obj *)b->cells;
        if ((o->flags & F_OLD) && (o->flags & F_COLORS) != color) {
            int64_t bytes = (int64_t)(b->end - b->cells);
            if (b->cards) {
                /* Forget its written cards. */
                size_t k = 0;
                for (size_t i = 0; i < dirty.len; i++) {
                    if (dirty.items[i].b != b) dirty.items[k++] = dirty.items[i];
                }
                dirty.len = k;
            }
            used_bytes -= bytes;
            old_bytes -= bytes;
            free_large(b);
        }
        return;
    }
    for (char *c = b->cells; c < b->bump; c += b->cell) {
        Obj *o = (Obj *)c;
        if ((o->flags & (F_ALLOC | F_OLD)) == (F_ALLOC | F_OLD) && (o->flags & F_COLORS) != color) {
            free_cell(b, o);
            old_bytes -= (int64_t)b->cell;
        }
    }
    if (b->live == 0 && !b->in_avail && classes[b->cls].cur != b) {
        retire_block(b);
    } else {
        make_available(b);
    }
}

static int64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

/* Sweeps blocks until done (returns 1), the deadline passes or `limit` blocks are done
 * (0: no deadline or limit). Blocks created meanwhile are linked before the cursor and hold
 * only objects that survive. */
static int sweep_some(int64_t deadline, int64_t limit) {
    int64_t n = 0;
    while (sweep_cursor) {
        Block *b = sweep_cursor;
        sweep_cursor = b->next;
        sweep_block(b);
        n++;
        if (limit && n >= limit) break;
        if (deadline && n % 16 == 0 && now_ns() >= deadline) break;
    }
    if (sweep_cursor) return 0;
    phase = IDLE;
    ncollections++;
    major_threshold = old_bytes * 2 > MIN_MAJOR ? old_bytes * 2 : MIN_MAJOR;
    return 1;
}

/* ---------------------------------------------------------------- weak references */

/* Whether an object is dead after the running nursery collection's marking (`major` 0) or a
 * finished major marking. */
static int dead(const Obj *o, int major) {
    if (o->flags & F_ARENA) return 0; /* until its arena closes */
    if (major) return (o->flags & F_OLD) && (o->flags & F_COLORS) != color;
    return !(o->flags & (F_OLD | F_MARK));
}

/* Clears the weak words whose targets are dead, and forgets the dead weak objects. */
static void clear_weak(int major) {
    size_t k = 0;
    for (size_t i = 0; i < weak_objects.len; i++) {
        Obj *w = weak_objects.items[i];
        if (dead(w, major)) continue;
        weak_objects.items[k++] = w;
        const PplLayout *l = w->layout;
        for (int32_t j = 0; j < l->nweak; j++) {
            char *slot = (char *)(w + 1) + l->refs[l->nrefs + j];
            void *p;
            memcpy(&p, slot, sizeof p);
            if (!p) continue;
            Obj *t = find_object(p);
            if (t && dead(t, major)) memset(slot, 0, sizeof p);
        }
    }
    weak_objects.len = k;
}

/* Drops the weak objects in the arenas from `level` up (they're about to be freed). */
static void forget_weak_in_arenas(int level) {
    size_t k = 0;
    for (size_t i = 0; i < weak_objects.len; i++) {
        Obj *w = weak_objects.items[i];
        if ((w->flags & F_ARENA) && arena_level_of(w) >= level) continue;
        weak_objects.items[k++] = w;
    }
    weak_objects.len = k;
}

/* ---------------------------------------------------------------- collections */

/* `PPL_GC_TRACE=1` prints every pause to stderr: what ran and how long it took. */
static void record_pause(int64_t start, const char *what) {
    int64_t d = now_ns() - start;
    if (trace_mode) {
        fprintf(stderr, "gc: %-28s %8lld us  heap %lld KiB\n", what, (long long)(d / 1000),
                (long long)(heap_bytes >> 10));
    }
    last_pause = d;
    if (d > max_pause) max_pause = d;
    total_pause += d;
}

static void young_collection(void) {
    scan_roots(visit_young);
    for (size_t i = 0; i < remembered.len; i++) {
        Obj *o = remembered.items[i];
        /* (Swept since it was remembered: freed, or reused by a new object.) */
        if (!(o->flags & F_REM)) continue;
        o->flags &= (uint8_t)~F_REM;
        scan_object(o, visit_young);
    }
    remembered.len = 0;
    for (size_t i = 0; i < dirty.len; i++) {
        dirty.items[i].b->cards[dirty.items[i].card] = 0;
        scan_card(dirty.items[i].b, dirty.items[i].card, visit_young);
    }
    dirty.len = 0;
    while (young_stack.len > 0) scan_object(young_stack.items[--young_stack.len], visit_young);
    clear_weak(0);
    sweep_young();
    young_bytes = 0;
    nyoung++;
}

static void start_marking(void) {
    phase = MARKING;
    step_bytes = 0;
    color = color == F_COLOR_A ? F_COLOR_B : F_COLOR_A;
    scan_roots(visit_old);
}

/* Scans grey objects (large ones a chunk at a time) until none are left (returns 1), the
 * deadline passes, or about `limit` bytes are scanned (0: no deadline or limit). */
static int mark_some(int64_t deadline, int64_t limit) {
    int64_t work = 0, checked = 0;
    while (grey.len > 0) {
        Grey g = grey.items[--grey.len];
        int64_t to = g.o->size;
        if (to - g.from > CHUNK_BYTES && g.o->layout) {
            int64_t step = CHUNK_BYTES / g.o->layout->size * g.o->layout->size;
            if (step == 0) step = g.o->layout->size;
            to = g.from + step;
            push_grey(g.o, to);
        }
        scan_range((const char *)(g.o + 1), g.o->layout, g.from, to, visit_old);
        work += to - g.from + (int64_t)sizeof(Obj);
        if (limit && work >= limit) return grey.len == 0;
        if (deadline && work - checked >= 4096) {
            checked = work;
            if (now_ns() >= deadline) return 0;
        }
    }
    return 1;
}

/* The end of the marking: everything young that survives is promoted (and greyed), the
 * roots are scanned again, the rest is marked, and sweeping begins. */
static void finish_marking(void) {
    young_collection();
    scan_roots(visit_old);
    mark_some(0, 0);
    clear_weak(1);
    phase = SWEEPING;
    sweep_cursor = all_blocks;
}

/* For the message encoder: the heap object containing `p` (its data, size and layout). */
void *ppl_gc_find_object(const void *p, int64_t *size, const PplLayout **layout) {
    Obj *o = find_object(p);
    if (!o) return NULL;
    *size = o->size;
    *layout = o->layout;
    return o + 1;
}

void ppl_gc_safepoint(void) {
    __atomic_store_n(&ppl_gc_requested, 0, __ATOMIC_RELAXED);
    /* (A terminated worker stops at its next safepoint.) */
    ppl_worker_check();
    if (paused) return;
    int64_t start = now_ns();
    char what[64] = "";
    if (phase != IDLE && (step_bytes >= STEP_BYTES || stress_mode)) {
        step_bytes = 0;
        /* (Stress mode does little per step, so a cycle spans many safepoints.) */
        if (phase == MARKING) {
            strcat(what, "mark ");
            if (stress_mode ? mark_some(0, 256) : mark_some(start + STEP_NS, 0)) {
                finish_marking();
                strcat(what, "finish ");
            }
        } else {
            strcat(what, "sweep ");
            if (stress_mode) {
                sweep_some(0, 1);
            } else {
                sweep_some(start + STEP_NS, 0);
            }
        }
    }
    if (young_bytes >= NURSERY || (stress_mode && young_objects.len)) {
        young_collection();
        strcat(what, "young ");
    }
    if (phase == IDLE && (old_bytes >= major_threshold || stress_mode)) {
        start_marking();
        strcat(what, "start ");
    }
    record_pause(start, what);
}

/* ---------------------------------------------------------------- std/gc */

void ppl_gc_collect(void) {
    int64_t start = now_ns();
    if (phase == SWEEPING) sweep_some(0, 0);
    if (phase == IDLE) start_marking();
    finish_marking();
    sweep_some(0, 0);
    record_pause(start, "collect");
}

void ppl_gc_collect_young(void) {
    int64_t start = now_ns();
    young_collection();
    record_pause(start, "collectYoung");
}

void ppl_gc_pause(void) { paused++; }

void ppl_gc_resume(void) {
    if (paused > 0) paused--;
    if (!paused && (young_bytes >= NURSERY || step_bytes >= STEP_BYTES)) __atomic_store_n(&ppl_gc_requested, 1, __ATOMIC_RELAXED);
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
    case -1: /* `gc.resetStats()` */
        last_pause = max_pause = total_pause = 0;
        return 0;
    default: return 0;
    }
}
