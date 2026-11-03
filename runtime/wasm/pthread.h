/* pthreads for the runtime on wasm32-wasi (P8.5a), which is single-threaded: wasi-libc has no
 * threads. Locks do nothing, waiting on a condition can't succeed (nothing else could signal
 * it), and starting a thread fails, so `Worker` and the I/O threads aren't available there.
 * Only the runtime's wasm builds see this header (`-isystem runtime/wasm`). */
#ifndef PPL_WASM_PTHREAD_H
#define PPL_WASM_PTHREAD_H

#include <errno.h>
#include <stddef.h>
#include <stdlib.h>
#include <sys/types.h> /* the pthread types (musl's, as wasi-libc defines them) */

#define PTHREAD_MUTEX_INITIALIZER {{{0}}}
#define PTHREAD_COND_INITIALIZER {{{0}}}
#define PTHREAD_ONCE_INIT 0
#define PTHREAD_CREATE_DETACHED 1
#define PTHREAD_CREATE_JOINABLE 0

static inline int pthread_mutex_init(pthread_mutex_t *m, const pthread_mutexattr_t *a) {
    (void)m, (void)a;
    return 0;
}
static inline int pthread_mutex_destroy(pthread_mutex_t *m) { (void)m; return 0; }
static inline int pthread_mutex_lock(pthread_mutex_t *m) { (void)m; return 0; }
static inline int pthread_mutex_trylock(pthread_mutex_t *m) { (void)m; return 0; }
static inline int pthread_mutex_unlock(pthread_mutex_t *m) { (void)m; return 0; }

static inline int pthread_cond_init(pthread_cond_t *c, const pthread_condattr_t *a) {
    (void)c, (void)a;
    return 0;
}
static inline int pthread_cond_destroy(pthread_cond_t *c) { (void)c; return 0; }
static inline int pthread_cond_signal(pthread_cond_t *c) { (void)c; return 0; }
static inline int pthread_cond_broadcast(pthread_cond_t *c) { (void)c; return 0; }
/* Nobody else could ever signal it. */
static inline int pthread_cond_wait(pthread_cond_t *c, pthread_mutex_t *m) {
    (void)c, (void)m;
    abort();
}

static inline int pthread_attr_init(pthread_attr_t *a) { (void)a; return 0; }
static inline int pthread_attr_destroy(pthread_attr_t *a) { (void)a; return 0; }
static inline int pthread_attr_setdetachstate(pthread_attr_t *a, int d) {
    (void)a, (void)d;
    return 0;
}
static inline int pthread_attr_setstacksize(pthread_attr_t *a, size_t n) {
    (void)a, (void)n;
    return 0;
}
static inline int pthread_create(pthread_t *t, const pthread_attr_t *a, void *(*f)(void *),
                                 void *arg) {
    (void)t, (void)a, (void)f, (void)arg;
    return EAGAIN;
}
static inline int pthread_detach(pthread_t t) { (void)t; return 0; }
static inline pthread_t pthread_self(void) { return (pthread_t)1; }

static inline int pthread_once(pthread_once_t *o, void (*f)(void)) {
    if (!*o) {
        *o = 1;
        f();
    }
    return 0;
}

#endif
