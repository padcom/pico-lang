/* The ppl heap (spec §9): allocation, roots and the collector. */
#include "ppl_rt.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

PplFrame *ppl_gc_top;
int32_t ppl_gc_requested;
static const PplGlobalRoots *globals;

static _Noreturn void out_of_memory(void) {
    fflush(stdout);
    fputs("fatal: out of memory\n", stderr);
    abort();
}

/* ---------------------------------------------------------------- allocation */

void *ppl_gc_alloc(int64_t size, const PplLayout *layout) {
    (void)layout;
    void *p = calloc(1, size > 0 ? (size_t)size : 1);
    if (!p) out_of_memory();
    return p;
}

void *ppl_alloc(int64_t size) { return ppl_gc_alloc(size, NULL); }

void *ppl_alloc_zeroed(int64_t size) { return ppl_gc_alloc(size, NULL); }

void ppl_gc_write(void *slot) { (void)slot; }

void ppl_gc_safepoint(void) { ppl_gc_requested = 0; }

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

/* ---------------------------------------------------------------- std/gc */

void ppl_gc_collect(void) {}
void ppl_gc_collect_young(void) {}
void ppl_gc_pause(void) {}
void ppl_gc_resume(void) {}
int64_t ppl_gc_stat(int64_t which) {
    (void)which;
    return 0;
}
