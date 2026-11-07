/* File-system I/O for `std/fs` (spec §13.4): jobs run on a small thread pool; their results
 * go back to the thread that submitted them (its event loop polls them, and is woken when one
 * is done). The pico side is `__io(op, a, b, c, s, t)` and `__io_text(job)` (see
 * `stdlib/fs.pi` and the core event loop).
 *
 * Errors are portable kinds (the job's `r` is -kind, `r2` the errno), so messages are the same
 * in the interpreter, which runs the same operations with Rust's std::fs. */
#define _GNU_SOURCE
#if defined(_WIN32)
#define _CRT_RAND_S /* rand_s */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#endif
#include "pico_rt.h"
#include "pico_crypto.h"

#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#if defined(__APPLE__) || defined(__linux__)
#define PICO_IO_POSIX 1
#include <dirent.h>
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/types.h>
#include <unistd.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
#elif defined(__wasi__)
/* (wasi-libc's POSIX file API over preopened directories; no processes or sockets.) */
#include <dirent.h>
#include <fcntl.h>
#include <sys/random.h> /* getentropy */
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#endif

void *pico_loop_waker(void);
void pico_loop_wake(void *waker);

enum {
    K_NOT_FOUND = 1,
    K_ALREADY_EXISTS = 2,
    K_PERMISSION_DENIED = 3,
    K_NOT_A_DIRECTORY = 4,
    K_IS_A_DIRECTORY = 5,
    K_DIRECTORY_NOT_EMPTY = 6,
    K_INVALID_DATA = 7,
    K_OTHER = 8,
    /* std/net */
    K_CONN_REFUSED = 9,
    K_CONN_RESET = 10,
    K_ADDR_IN_USE = 11,
    K_ADDR_NOT_AVAIL = 12,
    K_TIMED_OUT = 13,
    K_HOST_NOT_FOUND = 14,
};

struct Queue;

typedef struct Job {
    int64_t id;
    int64_t op, a, b, c;
    char *s, *t; /* NUL-terminated copies */
    size_t tlen;
    /* Results. */
    int64_t r, r2;
    char *text;
    size_t text_len;
    struct Queue *owner;
    struct Job *next;
    /* WASI socket jobs (P8.5c): the pollable it waits on (-1: none) and its progress. */
    int32_t wasi_pollable;
    void *wasi;
} Job;

/* A thread's finished jobs, and the jobs it polled but hasn't freed. */
typedef struct Queue {
    pthread_mutex_t lock;
    Job *head, *tail;
    void *waker;
    int64_t pending; /* submitted, not yet polled (only the owner changes it) */
    Job *taken;      /* polled, not yet freed */
    int64_t next_id;
} Queue;

static _Thread_local Queue *my_queue;

static Queue *queue(void) {
    if (!my_queue) {
        my_queue = calloc(1, sizeof *my_queue);
        if (!my_queue) abort();
        pthread_mutex_init(&my_queue->lock, NULL);
        my_queue->waker = pico_loop_waker();
        my_queue->next_id = 1;
    }
    return my_queue;
}

/* Hands a finished job to the thread that submitted it (and wakes its loop). */
static void finish(Job *j) {
    j->next = NULL;
    Queue *q = j->owner;
    pthread_mutex_lock(&q->lock);
    if (q->tail) q->tail->next = j;
    else q->head = j;
    q->tail = j;
    void *w = q->waker;
    pthread_mutex_unlock(&q->lock);
    pico_loop_wake(w);
}

/* A control operation's result, polled already (read with 102/103/`__io_text`, freed with
 * 104). */
static Job *result_job(Queue *q) {
    Job *j = calloc(1, sizeof *j);
    if (!j) abort();
    j->id = q->next_id++;
    j->next = q->taken;
    q->taken = j;
    return j;
}

/* WHATWG's UTF-8 decoder: each maximal invalid subsequence becomes U+FFFD (or, when
 * `fatal`, the job fails with InvalidData and r2 = the offset of the first bad byte). */
static int64_t decode_utf8(Queue *q, const unsigned char *p, size_t n, int fatal) {
    Job *j = result_job(q);
    size_t cap = n + 16, len = 0;
    char *out = malloc(cap);
    if (!out) abort();
    size_t i = 0;
    while (i < n) {
        unsigned c = p[i];
        size_t need = 0;
        unsigned lo = 0x80, hi = 0xBF;
        if (c < 0x80) {
            need = 0;
        } else if (c >= 0xC2 && c <= 0xDF) {
            need = 1;
        } else if (c >= 0xE0 && c <= 0xEF) {
            need = 2;
            if (c == 0xE0) lo = 0xA0;
            if (c == 0xED) hi = 0x9F;
        } else if (c >= 0xF0 && c <= 0xF4) {
            need = 3;
            if (c == 0xF0) lo = 0x90;
            if (c == 0xF4) hi = 0x8F;
        } else {
            need = (size_t)-1;
        }
        size_t k = 1;
        int ok = need != (size_t)-1;
        if (ok) {
            for (; k <= need; k++) {
                if (i + k >= n) {
                    ok = 0;
                    break;
                }
                unsigned d = p[i + k];
                unsigned l = k == 1 ? lo : 0x80, h = k == 1 ? hi : 0xBF;
                if (d < l || d > h) {
                    ok = 0;
                    break;
                }
            }
        }
        if (len + 4 > cap) {
            cap = cap * 2 + 4;
            out = realloc(out, cap);
            if (!out) abort();
        }
        if (ok) {
            memcpy(out + len, p + i, need + 1);
            len += need + 1;
            i += need + 1;
        } else {
            if (fatal) {
                free(out);
                j->r = -K_INVALID_DATA;
                j->r2 = (int64_t)i;
                return j->id;
            }
            memcpy(out + len, "\xEF\xBF\xBD", 3);
            len += 3;
            i += k; /* (k bytes made up the maximal subpart) */
        }
    }
    out = realloc(out, len + 1);
    if (!out) abort();
    out[len] = 0;
    j->text = out;
    j->text_len = len;
    return j->id;
}

static char *cstr(const PicoStr *s, size_t *len) {
    size_t n = s ? (size_t)s->byte_len : 0;
    char *m = malloc(n + 1);
    if (!m) abort();
    if (n) memcpy(m, s->data, n);
    m[n] = 0;
    if (len) *len = n;
    return m;
}

static Job *taken(int64_t id) {
    for (Job *j = queue()->taken; j; j = j->next)
        if (j->id == id) return j;
    return NULL;
}

static int kind_of(int e) {
    switch (e) {
    case ENOENT: return K_NOT_FOUND;
    case EEXIST: return K_ALREADY_EXISTS;
    case EACCES:
    case EPERM: return K_PERMISSION_DENIED;
    case ENOTDIR: return K_NOT_A_DIRECTORY;
    case EISDIR: return K_IS_A_DIRECTORY;
    case ENOTEMPTY: return K_DIRECTORY_NOT_EMPTY;
    case ECONNREFUSED: return K_CONN_REFUSED;
    case ECONNRESET:
    case ECONNABORTED:
    case EPIPE: return K_CONN_RESET;
    case EADDRINUSE: return K_ADDR_IN_USE;
    case EADDRNOTAVAIL: return K_ADDR_NOT_AVAIL;
    case ETIMEDOUT: return K_TIMED_OUT;
    default: return K_OTHER;
    }
}

static void fail(Job *j) {
    int e = errno;
    j->r = -kind_of(e);
    j->r2 = e;
}

/* Cancellable reads (job 20, #100). A read registers while it waits; `Reader.close()` cancels it
 * by the owner's queue and job id (op 107), and the read then ends as at the end of the stream.
 * POSIX: the read polls its descriptor and a wake pipe of its own, which a cancel writes to.
 * Windows: a cancel interrupts the waiting thread's `ReadFile` (`CancelSynchronousIo`). A cancel
 * that comes before the read registers waits for it here (`cancelled`, no thread yet). WASI has
 * no threads: nothing to cancel. */
#if !defined(__wasi__)
typedef struct Cancel {
    Queue *q;
    int64_t id;
    int cancelled;
#if defined(_WIN32)
    HANDLE thread;
#elif PICO_IO_POSIX
    int wake[2];
#endif
    struct Cancel *next;
} Cancel;

static pthread_mutex_t cancel_lock = PTHREAD_MUTEX_INITIALIZER;
static Cancel *cancels;

/* The entry for job `id` of queue `q` (under `cancel_lock`), or NULL. */
static Cancel **cancel_find(Queue *q, int64_t id) {
    for (Cancel **c = &cancels; *c; c = &(*c)->next)
        if ((*c)->q == q && (*c)->id == id) return c;
    return NULL;
}

/* Registers job `j` as waiting: its entry, or NULL when it was cancelled already. */
static Cancel *cancel_register(Job *j) {
    pthread_mutex_lock(&cancel_lock);
    Cancel **found = cancel_find(j->owner, j->id);
    if (found) {
        /* Cancelled before it started. */
        Cancel *c = *found;
        *found = c->next;
        pthread_mutex_unlock(&cancel_lock);
        free(c);
        return NULL;
    }
    Cancel *c = calloc(1, sizeof *c);
    if (!c) abort();
    c->q = j->owner;
    c->id = j->id;
#if defined(_WIN32)
    if (!DuplicateHandle(GetCurrentProcess(), GetCurrentThread(), GetCurrentProcess(), &c->thread,
                         0, FALSE, DUPLICATE_SAME_ACCESS))
        c->thread = NULL;
#elif PICO_IO_POSIX
    if (pipe(c->wake) != 0) c->wake[0] = c->wake[1] = -1;
    else {
        fcntl(c->wake[0], F_SETFD, FD_CLOEXEC);
        fcntl(c->wake[1], F_SETFD, FD_CLOEXEC);
    }
#endif
    c->next = cancels;
    cancels = c;
    pthread_mutex_unlock(&cancel_lock);
    return c;
}

/* Unregisters `c` (from `cancel_register`); whether it was cancelled. */
static int cancel_unregister(Cancel *c) {
    pthread_mutex_lock(&cancel_lock);
    Cancel **at = cancel_find(c->q, c->id);
    if (at) *at = c->next;
    int cancelled = c->cancelled;
    pthread_mutex_unlock(&cancel_lock);
#if defined(_WIN32)
    if (c->thread) CloseHandle(c->thread);
#elif PICO_IO_POSIX
    if (c->wake[0] >= 0) {
        close(c->wake[0]);
        close(c->wake[1]);
    }
#endif
    free(c);
    return cancelled;
}
#endif

/* Op 107: cancels queue `q`'s read job `id` (one still running; a finished one is unaffected). */
static void cancel_read(Queue *q, int64_t id) {
#if defined(__wasi__)
    (void)q;
    (void)id;
#else
    pthread_mutex_lock(&cancel_lock);
    Cancel **found = cancel_find(q, id);
    if (found) {
        Cancel *c = *found;
        if (!c->cancelled) {
            c->cancelled = 1;
#if defined(_WIN32)
            if (c->thread) CancelSynchronousIo(c->thread);
#elif PICO_IO_POSIX
            if (c->wake[1] >= 0) {
                char b = 1;
                ssize_t w;
                do w = write(c->wake[1], &b, 1);
                while (w < 0 && errno == EINTR);
            }
#endif
        }
    } else {
        /* Not started yet: it finds this when it does. */
        Cancel *c = calloc(1, sizeof *c);
        if (!c) abort();
        c->q = q;
        c->id = id;
        c->cancelled = 1;
#if defined(_WIN32)
        c->thread = NULL;
#elif PICO_IO_POSIX
        c->wake[0] = c->wake[1] = -1;
#endif
        c->next = cancels;
        cancels = c;
    }
    pthread_mutex_unlock(&cancel_lock);
#endif
}

#if PICO_IO_POSIX
#include <poll.h>
/* Waits until `fd` can be read (or is closed, or fails) for job `j`; 1 when the wait was cancelled
 * instead (op 107). Without a wake pipe it doesn't wait, and the read blocks as before. */
static int read_wait(int fd, Job *j) {
    Cancel *c = cancel_register(j);
    if (!c) return 1;
    if (c->wake[0] >= 0) {
        struct pollfd p[2] = {{fd, POLLIN, 0}, {c->wake[0], POLLIN, 0}};
        for (;;) {
            int k = poll(p, 2, -1);
            if (k < 0 && errno == EINTR) continue;
            /* (An error from poll itself: let the read report it.) */
            if (k < 0 || p[0].revents || p[1].revents) break;
        }
    }
    return cancel_unregister(c);
}
#endif

/* ---- the platform's part (below) ---- */

#if defined(_WIN32)
#define PICO_SEP '\\'
#else
#define PICO_SEP '/'
#endif

/* What a path is: `kind` 'f' file, 'd' directory, 'l' symbolic link, 'o' other. */
typedef struct Info {
    char kind;
    int64_t size, mtime_ns;
    uint64_t ino;
} Info;

/* Facts about `path` (`follow`ing a final link or not); -1 with errno set. */
static int fs_info(const char *path, int follow, Info *out);
/* A directory's entries without `.` and `..`; NULL with errno set. Names are UTF-8 and
 * valid until the next call. */
typedef struct DirIter DirIter;
static DirIter *dir_open(const char *path);
static const char *dir_next(DirIter *d);
static void dir_close(DirIter *d);
/* Runs file job `j` on a pool thread. */
static void run(Job *j);
/* Runs socket job `j` (30–35; `std/net` below). */
static void net_run(Job *j);

/* Appends to the job's text result. */
static void text_add(Job *j, const char *p, size_t n) {
    char *m = realloc(j->text, j->text_len + n + 1);
    if (!m) abort();
    memcpy(m + j->text_len, p, n);
    j->text_len += n;
    m[j->text_len] = 0;
    j->text = m;
}

static int valid_utf8(const unsigned char *p, size_t n) {
    size_t i = 0;
    while (i < n) {
        unsigned char c = p[i];
        size_t len;
        uint32_t cp;
        if (c < 0x80) {
            i++;
            continue;
        } else if ((c & 0xE0) == 0xC0) {
            len = 2;
            cp = c & 0x1F;
        } else if ((c & 0xF0) == 0xE0) {
            len = 3;
            cp = c & 0x0F;
        } else if ((c & 0xF8) == 0xF0) {
            len = 4;
            cp = c & 0x07;
        } else {
            return 0;
        }
        if (i + len > n) return 0;
        for (size_t k = 1; k < len; k++) {
            if ((p[i + k] & 0xC0) != 0x80) return 0;
            cp = (cp << 6) | (p[i + k] & 0x3F);
        }
        if ((len == 2 && cp < 0x80) || (len == 3 && cp < 0x800) || (len == 4 && cp < 0x10000) ||
            cp > 0x10FFFF || (cp >= 0xD800 && cp < 0xE000))
            return 0;
        i += len;
    }
    return 1;
}

/* The pool. */
static pthread_mutex_t pool_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t pool_cond = PTHREAD_COND_INITIALIZER;
static Job *work_head, *work_tail;
static int pool_started;

static void *worker_main(void *arg) {
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&pool_lock);
        while (!work_head) pthread_cond_wait(&pool_cond, &pool_lock);
        Job *j = work_head;
        work_head = j->next;
        if (!work_head) work_tail = NULL;
        pthread_mutex_unlock(&pool_lock);
        run(j);
        finish(j);
    }
    return NULL;
}

static void start_pool(void) {
    pthread_mutex_lock(&pool_lock);
    if (!pool_started) {
        pool_started = 1;
        for (int i = 0; i < 4; i++) {
            pthread_t t;
            pthread_attr_t attr;
            pthread_attr_init(&attr);
            pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
            if (pthread_create(&t, &attr, worker_main, NULL) != 0) abort();
            pthread_attr_destroy(&attr);
        }
    }
    pthread_mutex_unlock(&pool_lock);
}

/* Jobs that may block for long (pipes, waiting for a child) get a thread each, so they
 * can't hold up the pool (a child may wait for input that another job writes). */
static void *solo_main(void *arg) {
    Job *j = arg;
    run(j);
    finish(j);
    return NULL;
}

static void run_solo(Job *j) {
    pthread_t t;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    if (pthread_create(&t, &attr, solo_main, j) != 0) abort();
    pthread_attr_destroy(&attr);
}

/* `watch` (spec §13.4): a watcher keeps a snapshot of the tree (path, kind, size, mtime,
 * inode per entry, sorted by path). While a "next" job (op 16) waits on it, its thread
 * compares a fresh scan with the snapshot and delivers the differences as events
 * ("c|m|r <relative path>\0" each, in path order). kqueue (macOS), inotify (Linux) or a
 * change notification (Windows) only wake the thread up; a timeout catches whatever they
 * miss. Between two `next` jobs changes aren't looked at, so they arrive as one difference
 * (the interpreter does the same). The platform parts are `fs_info`, `dir_*`, `wake_*` and
 * `wait_os`. */
typedef struct Entry {
    char *rel;
    char kind;
    int64_t size, mtime_ns;
    uint64_t ino;
} Entry;

typedef struct Snap {
    Entry *e;
    size_t n, cap;
} Snap;

typedef struct Watcher {
    int64_t id;
    char *root;
    int recursive;
    Snap snap;
    pthread_mutex_t lock;
    pthread_cond_t cond;
    Job *waiting; /* the "next" job, if one waits */
    int closed;
    intptr_t wake[2]; /* signalled on close, to end the OS wait (wake_*) */
    struct Watcher *next;
} Watcher;

static void wake_open(Watcher *w);
static void wake_signal(Watcher *w);
static void wake_close(Watcher *w);
/* Waits up to `ms` for a change in the tree or for the wake signal. */
static void wait_os(Watcher *w, int ms);

static pthread_mutex_t watch_lock = PTHREAD_MUTEX_INITIALIZER;
static Watcher *watchers;
static int64_t next_watcher = 1;

static void snap_free(Snap *s) {
    for (size_t i = 0; i < s->n; i++) free(s->e[i].rel);
    free(s->e);
    s->e = NULL;
    s->n = s->cap = 0;
}

static void snap_add(Snap *s, const char *rel, const Info *in) {
    if (s->n == s->cap) {
        s->cap = s->cap ? s->cap * 2 : 32;
        s->e = realloc(s->e, s->cap * sizeof *s->e);
        if (!s->e) abort();
    }
    Entry *e = &s->e[s->n++];
    e->rel = strdup(rel);
    if (!e->rel) abort();
    e->kind = in->kind;
    e->size = in->size;
    e->mtime_ns = in->mtime_ns;
    e->ino = in->ino;
}

/* `root/rel` (or `root` for ""), with the platform's separator, malloc'd. */
static char *join_rel(const char *root, const char *rel) {
    size_t n = strlen(root) + strlen(rel) + 2;
    char *m = malloc(n);
    if (!m) abort();
    if (*rel) snprintf(m, n, "%s%c%s", root, PICO_SEP, rel);
    else snprintf(m, n, "%s", root);
    return m;
}

static void scan_dir(Snap *s, const char *root, const char *rel, int recursive) {
    char *dir = join_rel(root, rel);
    DirIter *d = dir_open(dir);
    free(dir);
    if (!d) return;
    const char *name;
    while ((name = dir_next(d))) {
        char *child = *rel ? join_rel(rel, name) : strdup(name);
        if (!child) abort();
        char *full = join_rel(root, child);
        Info in;
        if (fs_info(full, 0, &in) == 0) {
            snap_add(s, child, &in);
            if (recursive && in.kind == 'd') scan_dir(s, root, child, recursive);
        }
        free(full);
        free(child);
    }
    dir_close(d);
}

static int entry_cmp(const void *a, const void *b) {
    return strcmp(((const Entry *)a)->rel, ((const Entry *)b)->rel);
}

/* The tree at `root`: its entries, or the file itself (as ""). -1 (errno set) if it can't
 * be looked at. */
static int scan(const char *root, int recursive, Snap *out) {
    Info in;
    if (fs_info(root, 1, &in) < 0) return -1;
    if (in.kind == 'd') scan_dir(out, root, "", recursive);
    else snap_add(out, "", &in);
    if (out->n > 1) qsort(out->e, out->n, sizeof *out->e, entry_cmp);
    return 0;
}

static void event(Job *j, char k, const char *rel) {
    char head[2] = {k, ' '};
    text_add(j, head, 2);
    text_add(j, rel, strlen(rel) + 1);
}

/* The differences from `a` to `b`, as events in `j`'s text; how many. */
static size_t diff(const Snap *a, const Snap *b, Job *j) {
    size_t i = 0, k = 0, n = 0;
    while (i < a->n || k < b->n) {
        int c = i == a->n ? 1 : k == b->n ? -1 : strcmp(a->e[i].rel, b->e[k].rel);
        if (c < 0) {
            event(j, 'r', a->e[i++].rel);
            n++;
        } else if (c > 0) {
            event(j, 'c', b->e[k++].rel);
            n++;
        } else {
            const Entry *x = &a->e[i++], *y = &b->e[k++];
            if (x->kind != y->kind || x->ino != y->ino) {
                event(j, 'r', x->rel);
                event(j, 'c', y->rel);
                n += 2;
            } else if (x->kind != 'd' && (x->size != y->size || x->mtime_ns != y->mtime_ns)) {
                event(j, 'm', y->rel);
                n++;
            }
        }
    }
    return n;
}

static Watcher *find_watcher(int64_t id) {
    for (Watcher *w = watchers; w; w = w->next)
        if (w->id == id) return w;
    return NULL;
}

/* Scans; if anything changed, hands the events to the waiting job. Whether it did. */
static int deliver(Watcher *w) {
    Snap now = {0};
    scan(w->root, w->recursive, &now);
    Job tmp = {0};
    if (diff(&w->snap, &now, &tmp) == 0) {
        snap_free(&now);
        return 0;
    }
    snap_free(&w->snap);
    w->snap = now;
    pthread_mutex_lock(&w->lock);
    Job *j = w->waiting;
    w->waiting = NULL;
    pthread_mutex_unlock(&w->lock);
    if (j) {
        j->r = 0;
        j->text = tmp.text;
        j->text_len = tmp.text_len;
        finish(j);
    } else {
        free(tmp.text);
    }
    return 1;
}

static void *watch_main(void *arg) {
    Watcher *w = arg;
    for (;;) {
        pthread_mutex_lock(&w->lock);
        while (!w->closed && !w->waiting) pthread_cond_wait(&w->cond, &w->lock);
        int closed = w->closed;
        pthread_mutex_unlock(&w->lock);
        if (closed) break;
        if (deliver(w)) continue;
        wait_os(w, 250);
    }
    /* Closed (and no longer listed): nobody else uses it now. */
    snap_free(&w->snap);
    wake_close(w);
    pthread_mutex_destroy(&w->lock);
    pthread_cond_destroy(&w->cond);
    free(w->root);
    free(w);
    return NULL;
}

static void watch_open(Job *j) {
    Snap s = {0};
    if (scan(j->s, (int)j->a, &s) < 0) {
        fail(j);
        return;
    }
    Watcher *w = calloc(1, sizeof *w);
    if (!w) abort();
    w->root = strdup(j->s);
    if (!w->root) abort();
    w->recursive = (int)j->a;
    w->snap = s;
    pthread_mutex_init(&w->lock, NULL);
    pthread_cond_init(&w->cond, NULL);
    wake_open(w);
    pthread_mutex_lock(&watch_lock);
    w->id = next_watcher++;
    w->next = watchers;
    watchers = w;
    pthread_mutex_unlock(&watch_lock);
    pthread_t t;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    if (pthread_create(&t, &attr, watch_main, w) != 0) abort();
    pthread_attr_destroy(&attr);
    j->r = w->id;
}

/* A "next" job (op 16, a = watcher): waits on the watcher; ends at once (r = 1) if it's
 * closed. */
static void watch_next(Job *j) {
    pthread_mutex_lock(&watch_lock);
    Watcher *w = find_watcher(j->a);
    int ok = 0;
    if (w) {
        pthread_mutex_lock(&w->lock);
        if (!w->closed && !w->waiting) {
            w->waiting = j;
            pthread_cond_signal(&w->cond);
            ok = 1;
        }
        pthread_mutex_unlock(&w->lock);
    }
    pthread_mutex_unlock(&watch_lock);
    if (!ok) {
        j->r = 1;
        finish(j);
    }
}

/* Closes watcher `id`: a waiting job ends (r = 1) and the thread stops. */
static void watch_close(int64_t id) {
    pthread_mutex_lock(&watch_lock);
    Watcher **p = &watchers;
    while (*p && (*p)->id != id) p = &(*p)->next;
    Watcher *w = *p;
    if (w) *p = w->next;
    Job *j = NULL;
    if (w) {
        pthread_mutex_lock(&w->lock);
        w->closed = 1;
        j = w->waiting;
        w->waiting = NULL;
        pthread_cond_signal(&w->cond);
        wake_signal(w);
        pthread_mutex_unlock(&w->lock);
    }
    pthread_mutex_unlock(&watch_lock);
    if (j) {
        j->r = 1;
        finish(j);
    }
}

#if PICO_IO_POSIX || defined(__wasi__)
/* ---- the POSIX operations (files, processes, sockets; files only on WASI, where jobs run
 * at once: there are no threads) ---- */


/* The whole file at `path` (malloc'd), or NULL with errno set. */
static char *read_all(const char *path, size_t *len) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return NULL;
    struct stat st;
    if (fstat(fd, &st) == 0 && S_ISDIR(st.st_mode)) {
        close(fd);
        errno = EISDIR;
        return NULL;
    }
    size_t cap = 4096, n = 0;
    char *buf = malloc(cap);
    if (!buf) abort();
    for (;;) {
        if (n == cap) {
            cap *= 2;
            buf = realloc(buf, cap);
            if (!buf) abort();
        }
        ssize_t k = read(fd, buf + n, cap - n);
        if (k < 0) {
            if (errno == EINTR) continue;
            int e = errno;
            free(buf);
            close(fd);
            errno = e;
            return NULL;
        }
        if (k == 0) break;
        n += (size_t)k;
    }
    close(fd);
    *len = n;
    return buf;
}

static int write_all(int fd, const char *p, size_t n) {
    /* A TLS connection's bytes go through its session. */
    ptrdiff_t t = pico_tls_write(fd, p, n);
    if (t != -2) return t < 0 ? -1 : 0;
    while (n > 0) {
        ssize_t k = write(fd, p, n);
        if (k < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        p += k;
        n -= (size_t)k;
    }
    return 0;
}

static int64_t mtime_ms(const struct stat *st) {
#if defined(__APPLE__)
    return (int64_t)st->st_mtimespec.tv_sec * 1000 + st->st_mtimespec.tv_nsec / 1000000;
#else
    return (int64_t)st->st_mtim.tv_sec * 1000 + st->st_mtim.tv_nsec / 1000000;
#endif
}

static char kind_char(mode_t m) {
    if (S_ISREG(m)) return 'f';
    if (S_ISDIR(m)) return 'd';
    if (S_ISLNK(m)) return 'l';
    return 'o';
}

static int mkdir_all(char *path) {
    for (char *p = path + 1; *p; p++) {
        if (*p != '/') continue;
        *p = 0;
        int r = mkdir(path, 0777);
        *p = '/';
        if (r < 0 && errno != EEXIST) return -1;
    }
    if (mkdir(path, 0777) < 0) {
        struct stat st;
        if (errno == EEXIST && stat(path, &st) == 0 && S_ISDIR(st.st_mode)) return 0;
        return -1;
    }
    return 0;
}

static int remove_all(const char *path) {
    struct stat st;
    if (lstat(path, &st) < 0) return -1;
    if (!S_ISDIR(st.st_mode)) return unlink(path);
    DIR *d = opendir(path);
    if (!d) return -1;
    struct dirent *e;
    size_t plen = strlen(path);
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        size_t n = plen + strlen(e->d_name) + 2;
        char *child = malloc(n);
        if (!child) abort();
        snprintf(child, n, "%s/%s", path, e->d_name);
        int r = remove_all(child);
        free(child);
        if (r < 0) {
            int er = errno;
            closedir(d);
            errno = er;
            return -1;
        }
    }
    closedir(d);
    return rmdir(path);
}

static void watch_open(Job *j);
#if PICO_IO_POSIX
static void child_wait(Job *j);
#endif
static void net_run(Job *j);

static void run(Job *j) {
    j->r = 0;
    switch (j->op) {
    case 1: { /* readText */
        size_t n;
        char *b = read_all(j->s, &n);
        if (!b) {
            fail(j);
            return;
        }
        if (!valid_utf8((unsigned char *)b, n)) {
            free(b);
            j->r = -K_INVALID_DATA;
            return;
        }
        j->text = b;
        j->text_len = n;
        return;
    }
    case 2: { /* readBytes: a buffer handle */
        size_t n;
        char *b = read_all(j->s, &n);
        if (!b) {
            fail(j);
            return;
        }
        j->r = (int64_t)(uintptr_t)b;
        j->r2 = (int64_t)n;
        return;
    }
    case 3:   /* writeText (t), a: append */
    case 4: { /* writeBytes (t holds the bytes) */
        int fd = open(j->s, O_WRONLY | O_CREAT | (j->a ? O_APPEND : O_TRUNC), 0666);
        if (fd < 0) {
            fail(j);
            return;
        }
        if (write_all(fd, j->t, j->tlen) < 0) {
            fail(j);
            close(fd);
            return;
        }
        if (close(fd) < 0) fail(j);
        return;
    }
    case 5: { /* stat, a: don't follow links */
        struct stat st;
        if ((j->a ? lstat(j->s, &st) : stat(j->s, &st)) < 0) {
            fail(j);
            return;
        }
        char line[96];
        int n = snprintf(line, sizeof line, "%c %lld %lld", kind_char(st.st_mode),
                         (long long)st.st_size, (long long)mtime_ms(&st));
        text_add(j, line, (size_t)n);
        return;
    }
    case 6: /* mkdir, a: recursive */
        if ((j->a ? mkdir_all(j->s) : mkdir(j->s, 0777)) < 0) fail(j);
        return;
    case 7: { /* remove, a: recursive */
        struct stat st;
        if (lstat(j->s, &st) < 0) {
            fail(j);
            return;
        }
        int r = j->a ? remove_all(j->s) : S_ISDIR(st.st_mode) ? rmdir(j->s) : unlink(j->s);
        if (r < 0) fail(j);
        return;
    }
    case 8: /* rename */
        if (rename(j->s, j->t) < 0) fail(j);
        return;
    case 37: { /* copy file s to t (replacing it), keeping s's permissions (#64) */
        int in = open(j->s, O_RDONLY | O_CLOEXEC);
        if (in < 0) {
            fail(j);
            return;
        }
        struct stat st;
        if (fstat(in, &st) < 0 || S_ISDIR(st.st_mode)) {
            if (S_ISDIR(st.st_mode)) errno = EISDIR;
            fail(j);
            close(in);
            return;
        }
        int out = open(j->t, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, st.st_mode & 07777);
        if (out < 0) {
            fail(j);
            close(in);
            return;
        }
        char buf[65536];
        for (;;) {
            ssize_t k = read(in, buf, sizeof buf);
            if (k < 0 && errno == EINTR) continue;
            if (k < 0) {
                fail(j);
                break;
            }
            if (k == 0) break;
            if (write_all(out, buf, (size_t)k) < 0) {
                fail(j);
                break;
            }
        }
        close(in);
        if (close(out) < 0 && j->r == 0) fail(j);
        return;
    }
    case 38: /* symlink at s pointing to t (#64) */
        if (symlink(j->t, j->s) < 0) fail(j);
        return;
    case 39: { /* readlink(s): text = the link's target (#64) */
        char buf[4096];
        ssize_t k = readlink(j->s, buf, sizeof buf);
        if (k < 0) {
            fail(j);
            return;
        }
        text_add(j, buf, (size_t)k);
        return;
    }
    case 40: /* chmod(s, a) (#64) */
#if defined(__wasi__)
        errno = ENOTSUP;
        fail(j);
#else
        if (chmod(j->s, (mode_t)j->a) < 0) fail(j);
#endif
        return;
    case 9: { /* readDir: "<kind> <name>\0" per entry, in name order */
        DIR *d = opendir(j->s);
        if (!d) {
            fail(j);
            return;
        }
        struct dirent *e;
        char **names = NULL;
        size_t count = 0, cap = 0;
        while ((e = readdir(d))) {
            if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
            char k = 'o';
            struct stat st;
            size_t n = strlen(j->s) + strlen(e->d_name) + 2;
            char *full = malloc(n);
            if (!full) abort();
            snprintf(full, n, "%s/%s", j->s, e->d_name);
            if (lstat(full, &st) == 0) k = kind_char(st.st_mode);
            free(full);
            size_t len = strlen(e->d_name) + 3;
            char *entry = malloc(len);
            if (!entry) abort();
            snprintf(entry, len, "%c %s", k, e->d_name);
            if (count == cap) {
                cap = cap ? cap * 2 : 16;
                names = realloc(names, cap * sizeof *names);
                if (!names) abort();
            }
            names[count++] = entry;
        }
        closedir(d);
        /* Sorted by name (byte order), so both backends list the same way. */
        for (size_t i = 1; i < count; i++) {
            for (size_t k = i; k > 0 && strcmp(names[k - 1] + 2, names[k] + 2) > 0; k--) {
                char *x = names[k];
                names[k] = names[k - 1];
                names[k - 1] = x;
            }
        }
        text_add(j, "", 0);
        for (size_t i = 0; i < count; i++) {
            text_add(j, names[i], strlen(names[i]) + 1);
            free(names[i]);
        }
        free(names);
        return;
    }
    case 10: { /* open: a = flags (1 read, 2 write, 4 append, 8 create, 16 truncate, 32 new) */
        int f = j->a;
        int mode = (f & 1) && (f & 6) ? O_RDWR : (f & 6) ? O_WRONLY : O_RDONLY;
        if (f & 4) mode |= O_APPEND;
        if (f & 8) mode |= O_CREAT;
        if (f & 16) mode |= O_TRUNC;
        if (f & 32) mode |= O_CREAT | O_EXCL;
        int fd = open(j->s, mode, 0666);
        if (fd < 0) {
            fail(j);
            return;
        }
        struct stat st;
        if (fstat(fd, &st) == 0 && S_ISDIR(st.st_mode) && (f & 6)) {
            close(fd);
            errno = EISDIR;
            {
            fail(j);
            return;
        }
        }
        j->r = fd;
        return;
    }
    case 11:   /* read: a = fd, b = at most this many bytes; r = handle, r2 = count */
#if PICO_IO_POSIX
    case 20: /* (the same from a pipe) */
#endif
    {
        size_t n = j->b > 0 ? (size_t)j->b : 1;
        char *b = calloc(n, 1);
        if (!b) abort();
#if PICO_IO_POSIX
        if (j->op == 20 && !pico_tls_has((int)j->a) && read_wait((int)j->a, j)) {
            /* Cancelled (`Reader.close()`): as at the end. */
            j->r = (int64_t)(uintptr_t)b;
            j->r2 = 0;
            return;
        }
#endif
        ssize_t k = pico_tls_read((int)j->a, b, n);
        if (k == -2) {
            do k = read((int)j->a, b, n);
            while (k < 0 && errno == EINTR);
        }
        if (k < 0) {
            free(b);
            {
            fail(j);
            return;
        }
        }
        j->r = (int64_t)(uintptr_t)b;
        j->r2 = k;
        return;
    }
    case 12: /* write: a = fd, t = bytes */
#if PICO_IO_POSIX
    case 21: /* (the same to a pipe) */
#endif
        if (write_all((int)j->a, j->t, j->tlen) < 0) {
            fail(j);
            return;
        }
        j->r = (int64_t)j->tlen;
        return;
    case 13: /* close */
        if (close((int)j->a) < 0) fail(j);
        return;
    case 14: { /* seek: a = fd, b = position from the start */
        off_t p = lseek((int)j->a, (off_t)j->b, SEEK_SET);
        if (p < 0) {
            fail(j);
            return;
        }
        j->r = (int64_t)p;
        return;
    }
    case 15: /* watch: s = path, a = recursive; r = the watcher's id */
#if defined(__wasi__)
        /* (A watcher waits on a thread of its own.) */
        errno = ENOSYS;
        fail(j);
#else
        watch_open(j);
#endif
        return;
#if PICO_IO_POSIX
    case 22: { /* read a pipe to its end as text (UTF-8) */
        size_t cap = 4096, n = 0;
        char *buf = malloc(cap);
        if (!buf) abort();
        for (;;) {
            if (n == cap) {
                cap *= 2;
                buf = realloc(buf, cap);
                if (!buf) abort();
            }
            ssize_t k = pico_tls_read((int)j->a, buf + n, cap - n);
            if (k == -2) k = read((int)j->a, buf + n, cap - n);
            if (k < 0) {
                if (errno == EINTR) continue;
                free(buf);
                fail(j);
                return;
            }
            if (k == 0) break;
            n += (size_t)k;
        }
        if (!valid_utf8((unsigned char *)buf, n)) {
            free(buf);
            j->r = -K_INVALID_DATA;
            return;
        }
        j->text = buf;
        j->text_len = n;
        if (!n) text_add(j, "", 0);
        return;
    }
    case 23: /* wait for child a: r = exit code, r2 = the signal that ended it (or 0) */
        child_wait(j);
        return;
    case 36: { /* TLS handshake on socket a: b = mode (pico_tls_start), s = host, t = PEM
                * text, then NUL and a second PEM text (a server's key); text = the error */
        size_t n1 = j->t ? strlen(j->t) : 0;
        const char *pem2 = j->t && n1 < j->tlen ? j->t + n1 + 1 : "";
        size_t n2 = j->t && n1 < j->tlen ? j->tlen - n1 - 1 : 0;
        char err[512];
        if (pico_tls_start((int)j->a, (int)j->b, j->s ? j->s : "", j->t ? j->t : "", n1, pem2,
                          n2, err, sizeof err) < 0) {
            j->r = -15;
            text_add(j, err, strlen(err));
        }
        return;
    }
    case 30:
    case 31:
    case 32:
    case 33:
    case 34:
    case 35: net_run(j); return;
#endif
    default: j->r = -K_OTHER; j->r2 = EINVAL; return;
    }
}


/* ---- the hooks (see `Info`) ---- */

static int64_t mtime_ns(const struct stat *st) {
#if defined(__APPLE__)
    return (int64_t)st->st_mtimespec.tv_sec * 1000000000 + st->st_mtimespec.tv_nsec;
#else
    return (int64_t)st->st_mtim.tv_sec * 1000000000 + st->st_mtim.tv_nsec;
#endif
}

static int fs_info(const char *path, int follow, Info *out) {
    struct stat st;
    if ((follow ? stat(path, &st) : lstat(path, &st)) < 0) return -1;
    out->kind = kind_char(st.st_mode);
    out->size = (int64_t)st.st_size;
    out->mtime_ns = mtime_ns(&st);
    out->ino = (uint64_t)st.st_ino;
    return 0;
}

struct DirIter {
    DIR *d;
};

static DirIter *dir_open(const char *path) {
    DIR *d = opendir(path);
    if (!d) return NULL;
    DirIter *it = malloc(sizeof *it);
    if (!it) abort();
    it->d = d;
    return it;
}

static const char *dir_next(DirIter *it) {
    struct dirent *e;
    while ((e = readdir(it->d)))
        if (strcmp(e->d_name, ".") && strcmp(e->d_name, "..")) return e->d_name;
    return NULL;
}

static void dir_close(DirIter *it) {
    closedir(it->d);
    free(it);
}

#if defined(__wasi__)
/* (Watchers need threads: `watch` fails on WASI.) */
static void wake_open(Watcher *w) { (void)w; }
static void wake_signal(Watcher *w) { (void)w; }
static void wake_close(Watcher *w) { (void)w; }
static void wait_os(Watcher *w, int ms) { (void)w, (void)ms; }
#else
static void wake_open(Watcher *w) {
    int p[2];
    if (pipe(p) < 0) abort();
    w->wake[0] = p[0];
    w->wake[1] = p[1];
}

static void wake_signal(Watcher *w) {
    char c = 1;
    if (write((int)w->wake[1], &c, 1) < 0) { /* the thread still ends at its timeout */ }
}

static void wake_close(Watcher *w) {
    close((int)w->wake[0]);
    close((int)w->wake[1]);
}

#if defined(__APPLE__)
#include <sys/event.h>
#include <sys/time.h>

/* Waits up to `ms` for a change to the root or a snapshot entry (files only for small
 * trees: one descriptor each), or for the wake pipe. */
static void wait_os(Watcher *w, int ms) {
    int kq = kqueue();
    if (kq < 0) {
        usleep((useconds_t)ms * 1000);
        return;
    }
    size_t cap = w->snap.n + 2, n = 0;
    struct kevent *ch = calloc(cap, sizeof *ch);
    int *fds = calloc(cap, sizeof *fds);
    if (!ch || !fds) abort();
    EV_SET(&ch[n++], (int)w->wake[0], EVFILT_READ, EV_ADD, 0, 0, NULL);
    size_t nfds = 0;
    int files = w->snap.n <= 256;
    for (size_t i = 0; i <= w->snap.n; i++) {
        const char *rel = i == 0 ? "" : w->snap.e[i - 1].rel;
        if (i > 0 && !files && w->snap.e[i - 1].kind != 'd') continue;
        if (i > 0 && !*rel) continue; /* (a watched file is the root) */
        char *full = join_rel(w->root, rel);
        int fd = open(full, O_EVTONLY);
        free(full);
        if (fd < 0) continue;
        fds[nfds++] = fd;
        EV_SET(&ch[n++], fd, EVFILT_VNODE, EV_ADD | EV_CLEAR,
               NOTE_WRITE | NOTE_EXTEND | NOTE_DELETE | NOTE_RENAME | NOTE_ATTRIB, 0, NULL);
    }
    struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000};
    struct kevent ev[8];
    int got = kevent(kq, ch, (int)n, ev, 8, &ts);
    for (size_t i = 0; i < nfds; i++) close(fds[i]);
    free(fds);
    free(ch);
    close(kq);
    if (got > 0) usleep(20000); /* let a burst of changes finish */
}
#elif defined(__linux__)
#include <poll.h>
#include <sys/inotify.h>

/* Waits up to `ms` for a change to the root or a directory in the snapshot, or for the
 * wake pipe. */
static void wait_os(Watcher *w, int ms) {
    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (fd < 0) {
        usleep((useconds_t)ms * 1000);
        return;
    }
    uint32_t mask = IN_CREATE | IN_DELETE | IN_MODIFY | IN_MOVED_FROM | IN_MOVED_TO |
                    IN_ATTRIB | IN_CLOSE_WRITE | IN_DELETE_SELF | IN_MOVE_SELF;
    inotify_add_watch(fd, w->root, mask);
    for (size_t i = 0; i < w->snap.n; i++) {
        if (w->snap.e[i].kind != 'd') continue;
        char *full = join_rel(w->root, w->snap.e[i].rel);
        inotify_add_watch(fd, full, mask);
        free(full);
    }
    struct pollfd p[2] = {{fd, POLLIN, 0}, {(int)w->wake[0], POLLIN, 0}};
    int got = poll(p, 2, ms);
    close(fd);
    if (got > 0) usleep(20000); /* let a burst of changes finish */
}
#endif

#endif

#if PICO_IO_POSIX
/* ---- `std/process` (spec §13.3) ---- */

static char *cstr(const PicoStr *s, size_t *len);

extern int pico_argc;
extern char **pico_argv;
extern char **environ;

/* Children not yet reaped, so `kill` never signals a reused pid. */
static pthread_mutex_t child_lock = PTHREAD_MUTEX_INITIALIZER;
static pid_t *live;
static size_t nlive, caplive;

static void live_add(pid_t pid) {
    pthread_mutex_lock(&child_lock);
    if (nlive == caplive) {
        caplive = caplive ? caplive * 2 : 8;
        live = realloc(live, caplive * sizeof *live);
        if (!live) abort();
    }
    live[nlive++] = pid;
    pthread_mutex_unlock(&child_lock);
}

/* (With child_lock held.) */
static int live_find(pid_t pid) {
    for (size_t i = 0; i < nlive; i++)
        if (live[i] == pid) return (int)i;
    return -1;
}

static void child_wait(Job *j) {
    pid_t pid = (pid_t)j->a;
    /* Wait without reaping, then reap while no `kill` can run. */
    siginfo_t info;
    int r;
    do r = waitid(P_PID, (id_t)pid, &info, WEXITED | WNOWAIT);
    while (r < 0 && errno == EINTR);
    pthread_mutex_lock(&child_lock);
    int st = 0;
    pid_t w;
    do w = waitpid(pid, &st, 0);
    while (w < 0 && errno == EINTR);
    int i = live_find(pid);
    if (i >= 0) live[i] = live[--nlive];
    pthread_mutex_unlock(&child_lock);
    if (w < 0) {
        fail(j);
        return;
    }
    if (WIFSIGNALED(st)) {
        j->r = 128 + WTERMSIG(st);
        j->r2 = WTERMSIG(st);
    } else {
        j->r = WEXITSTATUS(st);
        j->r2 = 0;
    }
}

/* Spawns `prog` (op 110). `t` holds NUL-separated fields: the working directory ("" for
 * this one), `argc` arguments, then `envc` "NAME=value" variables. `modes` has 2 bits per
 * stream (stdin, stdout, stderr: 0 inherit, 1 pipe, 2 null) and 64 for a cleared
 * environment. r = pid (or -kind, r2 = errno); text = "<stdin fd> <stdout fd> <stderr fd>"
 * (-1 where not piped). */
static void spawn_child(Job *j, int64_t argc, int64_t envc, int64_t modes, const char *prog,
                        const char *t, size_t tlen) {
    /* What the program printed comes before the child's output (#14). */
    fflush(stdout);
    fflush(stderr);
    static int sigpipe_ignored;
    if (!sigpipe_ignored) {
        /* A child that stops reading makes writes fail (EPIPE) instead of ending us. */
        signal(SIGPIPE, SIG_IGN);
        sigpipe_ignored = 1;
    }
    size_t nf = 1 + (size_t)argc + (size_t)envc;
    const char **fields = calloc(nf, sizeof *fields);
    if (!fields) abort();
    size_t k = 0, start = 0;
    for (size_t i = 0; i <= tlen && k < nf; i++) {
        if (i == tlen || t[i] == 0) {
            fields[k++] = t + start;
            start = i + 1;
        }
    }
    while (k < nf) fields[k++] = "";
    char **argv = calloc((size_t)argc + 2, sizeof *argv);
    if (!argv) abort();
    argv[0] = (char *)prog;
    for (int64_t i = 0; i < argc; i++) argv[i + 1] = (char *)fields[1 + i];
    /* The environment: ours (unless cleared) with the overrides replacing or adding. */
    size_t nenv = 0;
    if (!(modes & 64))
        while (environ[nenv]) nenv++;
    char **envp = calloc(nenv + (size_t)envc + 1, sizeof *envp);
    if (!envp) abort();
    size_t ne = 0;
    for (size_t i = 0; i < nenv; i++) {
        const char *e = environ[i];
        const char *eq = strchr(e, '=');
        size_t klen = eq ? (size_t)(eq - e) : strlen(e);
        int replaced = 0;
        for (int64_t v = 0; v < envc && !replaced; v++) {
            const char *o = fields[1 + argc + v];
            if (!strncmp(o, e, klen) && o[klen] == '=') replaced = 1;
        }
        if (!replaced) envp[ne++] = (char *)e;
    }
    for (int64_t v = 0; v < envc; v++) envp[ne++] = (char *)fields[1 + argc + v];
    envp[ne] = NULL;

    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    int mine[3] = {-1, -1, -1}, theirs[3] = {-1, -1, -1};
    int err = 0;
    for (int i = 0; i < 3 && !err; i++) {
        int mode = (int)(modes >> (2 * i)) & 3;
        if (mode == 1) {
            int p[2];
            if (pipe(p) < 0) {
                err = errno;
                break;
            }
            fcntl(p[0], F_SETFD, FD_CLOEXEC);
            fcntl(p[1], F_SETFD, FD_CLOEXEC);
            mine[i] = i == 0 ? p[1] : p[0];
            theirs[i] = i == 0 ? p[0] : p[1];
            posix_spawn_file_actions_adddup2(&fa, theirs[i], i);
        } else if (mode == 2) {
            posix_spawn_file_actions_addopen(&fa, i, "/dev/null", i == 0 ? O_RDONLY : O_WRONLY, 0);
        }
    }
    if (!err && fields[0][0]) err = posix_spawn_file_actions_addchdir_np(&fa, fields[0]);
    posix_spawnattr_t attr;
    posix_spawnattr_init(&attr);
    sigset_t none, def;
    sigemptyset(&none);
    sigemptyset(&def);
    sigaddset(&def, SIGPIPE);
    posix_spawnattr_setsigmask(&attr, &none);
    posix_spawnattr_setsigdefault(&attr, &def);
    posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF);
    pid_t pid = 0;
    /* A bare name is looked up in the child's `PATH` when the environment gives it one, as
     * Node (and the interpreter) do (#45); posix_spawnp would search ours. */
    const char *child_path = NULL;
    for (size_t i = 0; i < ne && !strchr(prog, '/'); i++) {
        if (!strncmp(envp[i], "PATH=", 5)) child_path = envp[i] + 5;
    }
    char *found = NULL;
    if (!err && child_path) {
        size_t plen = strlen(prog);
        for (const char *d = child_path;; d++) {
            const char *end = strchr(d, ':');
            size_t dlen = end ? (size_t)(end - d) : strlen(d);
            char *cand = malloc(dlen + plen + 3);
            if (!cand) abort();
            if (dlen == 0) {
                snprintf(cand, plen + 3, "./%s", prog);
            } else {
                memcpy(cand, d, dlen);
                cand[dlen] = '/';
                memcpy(cand + dlen + 1, prog, plen + 1);
            }
            if (access(cand, X_OK) == 0) {
                found = cand;
                break;
            }
            free(cand);
            if (!end) break;
            d = end;
        }
        if (!found) err = ENOENT;
    }
    if (!err && found) {
        err = posix_spawn(&pid, found, &fa, &attr, argv, envp);
        if (!err) live_add(pid);
    } else if (!err) {
        err = posix_spawnp(&pid, prog, &fa, &attr, argv, envp);
        if (!err) live_add(pid);
    }
    posix_spawnattr_destroy(&attr);
    posix_spawn_file_actions_destroy(&fa);
    for (int i = 0; i < 3; i++) {
        if (theirs[i] >= 0) close(theirs[i]);
        if (err && mine[i] >= 0) {
            close(mine[i]);
            mine[i] = -1;
        }
    }
    free(found);
    free(envp);
    free(argv);
    free(fields);
    if (err) {
        j->r = -kind_of(err);
        j->r2 = err;
        return;
    }
    j->r = pid;
    char line[64];
    int n = snprintf(line, sizeof line, "%d %d %d", mine[0], mine[1], mine[2]);
    text_add(j, line, (size_t)n);
}

static int64_t process_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PicoStr *s,
                          const PicoStr *t) {
    switch (op) {
    case 110: {
        Job *j = result_job(q);
        size_t tlen;
        char *prog = cstr(s, NULL), *fields = cstr(t, &tlen);
        spawn_child(j, a, b, c, prog, fields, tlen);
        free(prog);
        free(fields);
        return j->id;
    }
    case 111: { /* kill child a with signal b */
        pthread_mutex_lock(&child_lock);
        int r = live_find((pid_t)a) >= 0 ? kill((pid_t)a, (int)b) : 0;
        pthread_mutex_unlock(&child_lock);
        return r;
    }
    case 112: /* close fd a (a pipe's end) */
        return close((int)a);
    case 113: { /* the arguments, each followed by a NUL */
        Job *j = result_job(q);
        text_add(j, "", 0);
        for (int i = 1; i < pico_argc; i++) text_add(j, pico_argv[i], strlen(pico_argv[i]) + 1);
        return j->id;
    }
    case 114: { /* getenv(s): r = 1 when set, text = its value */
        Job *j = result_job(q);
        char *name = cstr(s, NULL);
        const char *v = getenv(name);
        free(name);
        j->r = v != NULL;
        if (v) text_add(j, v, strlen(v));
        return j->id;
    }
    case 115: { /* setenv(s, t) */
        char *name = cstr(s, NULL), *value = cstr(t, NULL);
        int r = setenv(name, value, 1);
        free(name);
        free(value);
        return r;
    }
    case 116: { /* unsetenv(s) */
        char *name = cstr(s, NULL);
        int r = unsetenv(name);
        free(name);
        return r;
    }
    case 117: { /* every variable: "NAME=value\0" each */
        Job *j = result_job(q);
        text_add(j, "", 0);
        for (char **e = environ; *e; e++) text_add(j, *e, strlen(*e) + 1);
        return j->id;
    }
    case 118: /* exit(a) */
        fflush(stdout);
        fflush(stderr);
        exit((int)a);
    case 119: return (int64_t)getpid();
    default: return -1;
    }
}

/* ---- the socket shims (see `std/net` below) ---- */

static void net_init(void) {
    static int done;
    if (!done) {
        signal(SIGPIPE, SIG_IGN);
        done = 1;
    }
}

typedef int sock_t;
#define SOCK_NONE ((int64_t)-1)

/* A new UDP socket's settings (none needed). */
static void sock_udp(int64_t s) { (void)s; }

/* errno after a failed socket call (already set on POSIX). */
static void sock_failed(void) {}

static void sock_close(int64_t s) { close((int)s); }

/* Not inherited by children. */
static void sock_private(int64_t s) { fcntl((int)s, F_SETFD, FD_CLOEXEC); }
#endif

#if defined(__wasi__)
/* ---- sockets on WASI preview 2 (P8.5c, decision 257) ----
 * Sockets are `wasi:sockets` resources behind ids from SOCK_BASE (so they're never taken for
 * file descriptors), driven through wit-bindgen's bindings (`runtime/wasi/imports.h`). There
 * are no threads, so a socket job is a little state machine: `net_step` advances it as far as
 * it goes without blocking and either finishes it or leaves it waiting on a pollable. The
 * event loop's wait (`pico_wasi_poll`, from `pico_loop_wait`) polls every waiting job's pollable
 * with the timeout and steps the ready ones. TLS runs on the same sockets, through
 * `pico_wasi_send`/`pico_wasi_recv` (non-blocking) and the stepped handshake (`pico_tls_step`). */
#include "imports.h"

/* What wit-bindgen's component-type object would define: the type is embedded at link time
 * instead (`pico-driver`'s `wasi::componentize`). */
void __component_type_object_force_link_imports(void) {}

#define SOCK_BASE ((int64_t)1 << 24)

typedef struct Sock {
    int udp;
    wasi_sockets_tcp_own_tcp_socket_t tcp;
    wasi_sockets_udp_own_udp_socket_t usock;
    int streams; /* TCP: in/out are set */
    wasi_io_streams_own_input_stream_t in;
    wasi_io_streams_own_output_stream_t out;
    int dstreams; /* UDP: din/dout are set */
    wasi_sockets_udp_own_incoming_datagram_stream_t din;
    wasi_sockets_udp_own_outgoing_datagram_stream_t dout;
    int shut_rd;
    int eof; /* the peer closed its side */
    /* Writes happen in the order they're made (a TLS record can't be interleaved): the one
     * in progress, and the ones queued behind it. */
    Job *writer;
    Job **queued;
    size_t nqueued;
} Sock;

static Sock **socks;
static size_t nsocks, capsocks;

static int64_t sock_add(Sock *k) {
    if (nsocks == capsocks) {
        capsocks = capsocks ? capsocks * 2 : 16;
        socks = realloc(socks, capsocks * sizeof *socks);
        if (!socks) abort();
    }
    socks[nsocks] = k;
    return SOCK_BASE + (int64_t)nsocks++;
}

static Sock *sock_of(int64_t id) {
    if (id < SOCK_BASE || (size_t)(id - SOCK_BASE) >= nsocks) return NULL;
    return socks[id - SOCK_BASE];
}

static wasi_sockets_network_borrow_network_t net(void) {
    static wasi_sockets_network_own_network_t n;
    static int have;
    if (!have) {
        n = wasi_sockets_instance_network_instance_network();
        have = 1;
    }
    return wasi_sockets_network_borrow_network(n);
}

static int net_errno(wasi_sockets_network_error_code_t e) {
    switch (e) {
    case WASI_SOCKETS_NETWORK_ERROR_CODE_WOULD_BLOCK: return EAGAIN;
    case WASI_SOCKETS_NETWORK_ERROR_CODE_CONNECTION_REFUSED: return ECONNREFUSED;
    case WASI_SOCKETS_NETWORK_ERROR_CODE_CONNECTION_RESET: return ECONNRESET;
    case WASI_SOCKETS_NETWORK_ERROR_CODE_ADDRESS_IN_USE: return EADDRINUSE;
    case WASI_SOCKETS_NETWORK_ERROR_CODE_ADDRESS_NOT_BINDABLE: return EADDRNOTAVAIL;
    case WASI_SOCKETS_NETWORK_ERROR_CODE_TIMEOUT: return ETIMEDOUT;
    case WASI_SOCKETS_NETWORK_ERROR_CODE_ACCESS_DENIED: return EACCES;
    default: return EIO;
    }
}

static void net_fail(Job *j, wasi_sockets_network_error_code_t e) {
    if (e == WASI_SOCKETS_NETWORK_ERROR_CODE_NAME_UNRESOLVABLE ||
        e == WASI_SOCKETS_NETWORK_ERROR_CODE_TEMPORARY_RESOLVER_FAILURE ||
        e == WASI_SOCKETS_NETWORK_ERROR_CODE_PERMANENT_RESOLVER_FAILURE) {
        j->r = -K_HOST_NOT_FOUND;
        j->r2 = e;
        return;
    }
    errno = net_errno(e);
    fail(j);
}

/* "a.b.c.d" as an IPv4 address. */
static int parse_ipv4(const char *s, wasi_sockets_network_ip_address_t *out) {
    unsigned v[4];
    char tail;
    if (sscanf(s, "%u.%u.%u.%u%c", &v[0], &v[1], &v[2], &v[3], &tail) != 4) return 0;
    for (int i = 0; i < 4; i++)
        if (v[i] > 255) return 0;
    out->tag = WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4;
    out->val.ipv4 = (wasi_sockets_network_ipv4_address_t){(uint8_t)v[0], (uint8_t)v[1],
                                                           (uint8_t)v[2], (uint8_t)v[3]};
    return 1;
}

/* An address as inet_ntop writes it (IPv6 with the longest run of zeros as "::"). */
static void ip_text(const wasi_sockets_network_ip_address_t *a, char *buf, size_t cap) {
    if (a->tag == WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4) {
        const wasi_sockets_network_ipv4_address_t *v = &a->val.ipv4;
        snprintf(buf, cap, "%u.%u.%u.%u", v->f0, v->f1, v->f2, v->f3);
        return;
    }
    const wasi_sockets_network_ipv6_address_t *v = &a->val.ipv6;
    uint16_t w[8] = {v->f0, v->f1, v->f2, v->f3, v->f4, v->f5, v->f6, v->f7};
    int best = -1, blen = 0;
    for (int i = 0; i < 8;) {
        if (w[i]) {
            i++;
            continue;
        }
        int k = i;
        while (k < 8 && !w[k]) k++;
        if (k - i > blen && k - i > 1) best = i, blen = k - i;
        i = k;
    }
    size_t n = 0;
    for (int i = 0; i < 8 && n < cap; i++) {
        if (i == best) {
            n += (size_t)snprintf(buf + n, cap - n, i == 0 ? "::" : ":");
            i += blen - 1;
            continue;
        }
        n += (size_t)snprintf(buf + n, cap - n, "%x%s", w[i], i < 7 && i + 1 != best ? ":" : "");
    }
}

static void sockaddr_text(Job *j, const wasi_sockets_network_ip_socket_address_t *a) {
    wasi_sockets_network_ip_address_t ip;
    int port;
    if (a->tag == WASI_SOCKETS_NETWORK_IP_SOCKET_ADDRESS_IPV4) {
        ip.tag = WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4;
        ip.val.ipv4 = a->val.ipv4.address;
        port = a->val.ipv4.port;
    } else {
        ip.tag = WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV6;
        ip.val.ipv6 = a->val.ipv6.address;
        port = a->val.ipv6.port;
    }
    char host[64], line[80];
    ip_text(&ip, host, sizeof host);
    int n = snprintf(line, sizeof line, "%s %d", host, port);
    text_add(j, line, (size_t)n);
}

static wasi_sockets_network_ip_socket_address_t sockaddr_of(const wasi_sockets_network_ip_address_t *ip,
                                                             int port) {
    wasi_sockets_network_ip_socket_address_t a;
    memset(&a, 0, sizeof a);
    if (ip->tag == WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4) {
        a.tag = WASI_SOCKETS_NETWORK_IP_SOCKET_ADDRESS_IPV4;
        a.val.ipv4.port = (uint16_t)port;
        a.val.ipv4.address = ip->val.ipv4;
    } else {
        a.tag = WASI_SOCKETS_NETWORK_IP_SOCKET_ADDRESS_IPV6;
        a.val.ipv6.port = (uint16_t)port;
        a.val.ipv6.address = ip->val.ipv6;
    }
    return a;
}

/* "local-host local-port remote-host remote-port" of a connected TCP socket. */
static void ends_text(Job *j, Sock *k) {
    wasi_sockets_network_ip_socket_address_t a;
    wasi_sockets_network_error_code_t e;
    wasi_sockets_tcp_borrow_tcp_socket_t b = wasi_sockets_tcp_borrow_tcp_socket(k->tcp);
    if (wasi_sockets_tcp_method_tcp_socket_local_address(b, &a, &e)) sockaddr_text(j, &a);
    else text_add(j, "? 0", 3);
    text_add(j, " ", 1);
    if (wasi_sockets_tcp_method_tcp_socket_remote_address(b, &a, &e)) sockaddr_text(j, &a);
    else text_add(j, "? 0", 3);
}

/* A job's progress across steps: the name lookup, the addresses it found, the socket. */
typedef struct NetState {
    int phase;
    wasi_sockets_ip_name_lookup_own_resolve_address_stream_t resolver;
    int resolving;
    wasi_sockets_network_ip_address_t addrs[16];
    int naddrs, next;
    Sock *k;
    size_t done; /* bytes written so far */
    int flushing;
    char *buf; /* reads */
    size_t cap, len;
} NetState;

/* Jobs waiting on a pollable (`j->wasi_pollable`, owned). */
static Job **waiting;
static size_t nwaiting, capwaiting;

static void wait_on(Job *j, wasi_io_poll_own_pollable_t p) {
    j->wasi_pollable = p.__handle;
    if (nwaiting == capwaiting) {
        capwaiting = capwaiting ? capwaiting * 2 : 16;
        waiting = realloc(waiting, capwaiting * sizeof *waiting);
        if (!waiting) abort();
    }
    waiting[nwaiting++] = j;
}

static void state_free(Job *j) {
    NetState *st = j->wasi;
    if (!st) return;
    if (st->resolving) wasi_sockets_ip_name_lookup_resolve_address_stream_drop_own(st->resolver);
    free(st->buf);
    free(st);
    j->wasi = NULL;
}

/* Starts (or continues) resolving `host`: 1 when the addresses are in, 0 while waiting, -1 on
 * failure (the job is failed). */
static int resolve_step(Job *j, NetState *st, const char *host) {
    if (!st->resolving && st->naddrs == 0) {
        if (!*host || !strcmp(host, "0.0.0.0")) {
            st->addrs[0].tag = WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4;
            memset(&st->addrs[0].val.ipv4, 0, sizeof st->addrs[0].val.ipv4);
            st->naddrs = 1;
            return 1;
        }
        if (parse_ipv4(host, &st->addrs[0])) {
            st->naddrs = 1;
            return 1;
        }
        imports_string_t name = {(uint8_t *)host, strlen(host)};
        wasi_sockets_network_error_code_t e;
        if (!wasi_sockets_ip_name_lookup_resolve_addresses(net(), &name, &st->resolver, &e)) {
            net_fail(j, e);
            return -1;
        }
        st->resolving = 1;
    }
    while (st->resolving) {
        wasi_sockets_ip_name_lookup_option_ip_address_t a;
        wasi_sockets_network_error_code_t e;
        wasi_sockets_ip_name_lookup_borrow_resolve_address_stream_t b =
            wasi_sockets_ip_name_lookup_borrow_resolve_address_stream(st->resolver);
        if (!wasi_sockets_ip_name_lookup_method_resolve_address_stream_resolve_next_address(b, &a, &e)) {
            if (e == WASI_SOCKETS_NETWORK_ERROR_CODE_WOULD_BLOCK) {
                wait_on(j, wasi_sockets_ip_name_lookup_method_resolve_address_stream_subscribe(b));
                return 0;
            }
            net_fail(j, e);
            return -1;
        }
        if (!a.is_some) {
            wasi_sockets_ip_name_lookup_resolve_address_stream_drop_own(st->resolver);
            st->resolving = 0;
            break;
        }
        if (st->naddrs < 16) st->addrs[st->naddrs++] = a.val;
    }
    if (st->naddrs == 0) {
        net_fail(j, WASI_SOCKETS_NETWORK_ERROR_CODE_NAME_UNRESOLVABLE);
        return -1;
    }
    return 1;
}

/* Reads from TCP socket `k` without blocking: bytes, 0 at the end, -1 (errno) on errors, -3
 * when it would block (then `*p` is set to wait on). TLS sessions come first. */
static ptrdiff_t sock_try_read(int64_t id, Sock *k, char *buf, size_t n, wasi_io_poll_own_pollable_t *p) {
    if (k->shut_rd || k->eof) return 0;
    ptrdiff_t t = pico_tls_try_read((int)id, buf, n);
    if (t != -2) {
        if (t == -3 || t == -4) {
            *p = t == -3 ? wasi_io_streams_method_input_stream_subscribe(
                               wasi_io_streams_borrow_input_stream(k->in))
                         : wasi_io_streams_method_output_stream_subscribe(
                               wasi_io_streams_borrow_output_stream(k->out));
            return -3;
        }
        return t;
    }
    imports_list_u8_t got;
    wasi_io_streams_stream_error_t e;
    wasi_io_streams_borrow_input_stream_t in = wasi_io_streams_borrow_input_stream(k->in);
    if (!wasi_io_streams_method_input_stream_read(in, n, &got, &e)) {
        if (e.tag == WASI_IO_STREAMS_STREAM_ERROR_CLOSED) {
            k->eof = 1;
            return 0;
        }
        wasi_io_error_error_drop_own(e.val.last_operation_failed);
        errno = ECONNRESET;
        return -1;
    }
    if (got.len == 0) {
        imports_list_u8_free(&got);
        *p = wasi_io_streams_method_input_stream_subscribe(in);
        return -3;
    }
    memcpy(buf, got.ptr, got.len);
    ptrdiff_t r = (ptrdiff_t)got.len;
    imports_list_u8_free(&got);
    return r;
}

/* Writes to TCP socket `k` without blocking: bytes taken (maybe fewer than `n`), -1 (errno),
 * -3 when nothing can be taken now (`*p` to wait on). */
static ptrdiff_t sock_try_write(int64_t id, Sock *k, const char *buf, size_t n, wasi_io_poll_own_pollable_t *p) {
    ptrdiff_t t = pico_tls_try_write((int)id, buf, n);
    if (t != -2) {
        if (t == -3 || t == -4) {
            *p = t == -3 ? wasi_io_streams_method_input_stream_subscribe(
                               wasi_io_streams_borrow_input_stream(k->in))
                         : wasi_io_streams_method_output_stream_subscribe(
                               wasi_io_streams_borrow_output_stream(k->out));
            return -3;
        }
        return t;
    }
    wasi_io_streams_borrow_output_stream_t out = wasi_io_streams_borrow_output_stream(k->out);
    uint64_t permit;
    wasi_io_streams_stream_error_t e;
    if (!wasi_io_streams_method_output_stream_check_write(out, &permit, &e)) {
        if (e.tag == WASI_IO_STREAMS_STREAM_ERROR_LAST_OPERATION_FAILED)
            wasi_io_error_error_drop_own(e.val.last_operation_failed);
        errno = EPIPE;
        return -1;
    }
    if (permit == 0) {
        *p = wasi_io_streams_method_output_stream_subscribe(out);
        return -3;
    }
    size_t m = n < permit ? n : (size_t)permit;
    imports_list_u8_t data = {(uint8_t *)buf, m};
    if (!wasi_io_streams_method_output_stream_write(out, &data, &e) ||
        !wasi_io_streams_method_output_stream_flush(out, &e)) {
        if (e.tag == WASI_IO_STREAMS_STREAM_ERROR_LAST_OPERATION_FAILED)
            wasi_io_error_error_drop_own(e.val.last_operation_failed);
        errno = EPIPE;
        return -1;
    }
    return (ptrdiff_t)m;
}

/* For pico_tls.c's BIO: raw bytes on socket `fd`, -1 with errno EAGAIN when it would block. */
ptrdiff_t pico_wasi_send(int fd, const unsigned char *buf, size_t len) {
    Sock *k = sock_of(fd);
    if (!k || !k->streams) {
        errno = EBADF;
        return -1;
    }
    wasi_io_streams_borrow_output_stream_t out = wasi_io_streams_borrow_output_stream(k->out);
    uint64_t permit;
    wasi_io_streams_stream_error_t e;
    if (!wasi_io_streams_method_output_stream_check_write(out, &permit, &e)) {
        if (e.tag == WASI_IO_STREAMS_STREAM_ERROR_LAST_OPERATION_FAILED)
            wasi_io_error_error_drop_own(e.val.last_operation_failed);
        errno = EPIPE;
        return -1;
    }
    if (permit == 0) {
        errno = EAGAIN;
        return -1;
    }
    size_t m = len < permit ? len : (size_t)permit;
    imports_list_u8_t data = {(uint8_t *)buf, m};
    if (!wasi_io_streams_method_output_stream_write(out, &data, &e) ||
        !wasi_io_streams_method_output_stream_flush(out, &e)) {
        if (e.tag == WASI_IO_STREAMS_STREAM_ERROR_LAST_OPERATION_FAILED)
            wasi_io_error_error_drop_own(e.val.last_operation_failed);
        errno = EPIPE;
        return -1;
    }
    return (ptrdiff_t)m;
}

ptrdiff_t pico_wasi_recv(int fd, unsigned char *buf, size_t len) {
    Sock *k = sock_of(fd);
    if (!k || !k->streams) {
        errno = EBADF;
        return -1;
    }
    if (k->shut_rd || k->eof) return 0;
    imports_list_u8_t got;
    wasi_io_streams_stream_error_t e;
    if (!wasi_io_streams_method_input_stream_read(wasi_io_streams_borrow_input_stream(k->in), len,
                                                  &got, &e)) {
        if (e.tag == WASI_IO_STREAMS_STREAM_ERROR_CLOSED) {
            k->eof = 1;
            return 0;
        }
        wasi_io_error_error_drop_own(e.val.last_operation_failed);
        errno = ECONNRESET;
        return -1;
    }
    if (got.len == 0) {
        imports_list_u8_free(&got);
        errno = EAGAIN;
        return -1;
    }
    memcpy(buf, got.ptr, got.len);
    ptrdiff_t r = (ptrdiff_t)got.len;
    imports_list_u8_free(&got);
    return r;
}

/* Blocks until socket `fd` can be read (or written): only TLS's close_notify does. */
void pico_wasi_wait(int fd, int write) {
    Sock *k = sock_of(fd);
    if (!k || !k->streams) return;
    wasi_io_poll_own_pollable_t p =
        write ? wasi_io_streams_method_output_stream_subscribe(wasi_io_streams_borrow_output_stream(k->out))
              : wasi_io_streams_method_input_stream_subscribe(wasi_io_streams_borrow_input_stream(k->in));
    wasi_io_poll_method_pollable_block(wasi_io_poll_borrow_pollable(p));
    wasi_io_poll_pollable_drop_own(p);
}

static void sock_close(Sock *k) {
    if (k->streams) {
        wasi_io_streams_input_stream_drop_own(k->in);
        wasi_io_streams_output_stream_drop_own(k->out);
    }
    if (k->dstreams) {
        wasi_sockets_udp_incoming_datagram_stream_drop_own(k->din);
        wasi_sockets_udp_outgoing_datagram_stream_drop_own(k->dout);
    }
    if (k->udp) wasi_sockets_udp_udp_socket_drop_own(k->usock);
    else wasi_sockets_tcp_tcp_socket_drop_own(k->tcp);
    free(k);
}

static int net_step(Job *j);

/* The write after `k`'s current one gets its turn. */
static void next_writer(Sock *k) {
    k->writer = NULL;
    while (k->nqueued > 0 && !k->writer) {
        Job *n = k->queued[0];
        memmove(k->queued, k->queued + 1, --k->nqueued * sizeof *k->queued);
        if (net_step(n)) { /* (it may finish at once, or become the writer and wait) */
            state_free(n);
            finish(n);
        }
    }
}

/* Advances socket job `j` (ops 20–22, 30–36): 1 when it's done (its results are set), 0 when
 * it waits on the pollable it registered. */
static int net_step(Job *j) {
    NetState *st = j->wasi;
    if (!st) {
        st = calloc(1, sizeof *st);
        if (!st) abort();
        j->wasi = st;
    }
    wasi_io_poll_own_pollable_t p;
    switch (j->op) {
    case 30: { /* listen (b = 0) or bind a UDP socket (b = 1): s = host, a = port */
        if (st->phase == 0) {
            int r = resolve_step(j, st, j->s);
            if (r <= 0) return r < 0;
            wasi_sockets_network_ip_address_t *ip = &st->addrs[0];
            wasi_sockets_network_ip_address_family_t fam =
                ip->tag == WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4 ? WASI_SOCKETS_NETWORK_IP_ADDRESS_FAMILY_IPV4
                                                                : WASI_SOCKETS_NETWORK_IP_ADDRESS_FAMILY_IPV6;
            Sock *k = calloc(1, sizeof *k);
            if (!k) abort();
            wasi_sockets_network_error_code_t e;
            wasi_sockets_network_ip_socket_address_t local = sockaddr_of(ip, (int)j->a);
            int ok;
            if (j->b) {
                k->udp = 1;
                ok = wasi_sockets_udp_create_socket_create_udp_socket(fam, &k->usock, &e) &&
                     wasi_sockets_udp_method_udp_socket_start_bind(
                         wasi_sockets_udp_borrow_udp_socket(k->usock), net(), &local, &e);
            } else {
                ok = wasi_sockets_tcp_create_socket_create_tcp_socket(fam, &k->tcp, &e) &&
                     wasi_sockets_tcp_method_tcp_socket_start_bind(
                         wasi_sockets_tcp_borrow_tcp_socket(k->tcp), net(), &local, &e);
            }
            if (!ok) {
                free(k);
                net_fail(j, e);
                return 1;
            }
            st->k = k;
            st->phase = 1;
        }
        Sock *k = st->k;
        wasi_sockets_network_error_code_t e;
        if (st->phase == 1) {
            int ok = k->udp ? wasi_sockets_udp_method_udp_socket_finish_bind(
                                  wasi_sockets_udp_borrow_udp_socket(k->usock), &e)
                            : wasi_sockets_tcp_method_tcp_socket_finish_bind(
                                  wasi_sockets_tcp_borrow_tcp_socket(k->tcp), &e);
            if (!ok && e == WASI_SOCKETS_NETWORK_ERROR_CODE_WOULD_BLOCK) {
                wait_on(j, k->udp ? wasi_sockets_udp_method_udp_socket_subscribe(
                                        wasi_sockets_udp_borrow_udp_socket(k->usock))
                                  : wasi_sockets_tcp_method_tcp_socket_subscribe(
                                        wasi_sockets_tcp_borrow_tcp_socket(k->tcp)));
                return 0;
            }
            if (!ok) {
                sock_close(k);
                net_fail(j, e);
                return 1;
            }
            if (k->udp) {
                wasi_sockets_udp_tuple2_own_incoming_datagram_stream_own_outgoing_datagram_stream_t ds;
                if (!wasi_sockets_udp_method_udp_socket_stream(wasi_sockets_udp_borrow_udp_socket(k->usock),
                                                               NULL, &ds, &e)) {
                    sock_close(k);
                    net_fail(j, e);
                    return 1;
                }
                k->din = ds.f0;
                k->dout = ds.f1;
                k->dstreams = 1;
                wasi_sockets_network_ip_socket_address_t a;
                if (wasi_sockets_udp_method_udp_socket_local_address(
                        wasi_sockets_udp_borrow_udp_socket(k->usock), &a, &e))
                    sockaddr_text(j, &a);
                j->r = sock_add(k);
                return 1;
            }
            wasi_sockets_tcp_borrow_tcp_socket_t b = wasi_sockets_tcp_borrow_tcp_socket(k->tcp);
            wasi_sockets_tcp_method_tcp_socket_set_listen_backlog_size(b, 128, &e);
            if (!wasi_sockets_tcp_method_tcp_socket_start_listen(b, &e)) {
                sock_close(k);
                net_fail(j, e);
                return 1;
            }
            st->phase = 2;
        }
        wasi_sockets_tcp_borrow_tcp_socket_t b = wasi_sockets_tcp_borrow_tcp_socket(k->tcp);
        if (!wasi_sockets_tcp_method_tcp_socket_finish_listen(b, &e)) {
            if (e == WASI_SOCKETS_NETWORK_ERROR_CODE_WOULD_BLOCK) {
                wait_on(j, wasi_sockets_tcp_method_tcp_socket_subscribe(b));
                return 0;
            }
            sock_close(k);
            net_fail(j, e);
            return 1;
        }
        wasi_sockets_network_ip_socket_address_t a;
        if (wasi_sockets_tcp_method_tcp_socket_local_address(b, &a, &e)) sockaddr_text(j, &a);
        j->r = sock_add(k);
        return 1;
    }
    case 31: { /* accept on listener a */
        Sock *l = sock_of(j->a);
        if (!l || l->udp) {
            errno = EBADF;
            fail(j);
            return 1;
        }
        if (l->shut_rd) {
            errno = EINVAL;
            fail(j);
            return 1;
        }
        wasi_sockets_tcp_borrow_tcp_socket_t b = wasi_sockets_tcp_borrow_tcp_socket(l->tcp);
        wasi_sockets_tcp_tuple3_own_tcp_socket_own_input_stream_own_output_stream_t got;
        wasi_sockets_network_error_code_t e;
        if (!wasi_sockets_tcp_method_tcp_socket_accept(b, &got, &e)) {
            if (e == WASI_SOCKETS_NETWORK_ERROR_CODE_WOULD_BLOCK) {
                wait_on(j, wasi_sockets_tcp_method_tcp_socket_subscribe(b));
                return 0;
            }
            net_fail(j, e);
            return 1;
        }
        Sock *k = calloc(1, sizeof *k);
        if (!k) abort();
        k->tcp = got.f0;
        k->in = got.f1;
        k->out = got.f2;
        k->streams = 1;
        ends_text(j, k);
        j->r = sock_add(k);
        return 1;
    }
    case 32: { /* connect to s, port a (trying each address) */
        for (;;) {
            if (st->phase == 0) {
                int r = resolve_step(j, st, j->s);
                if (r <= 0) return r < 0;
                st->phase = 1;
            }
            wasi_sockets_network_error_code_t e;
            if (st->phase == 1) {
                if (st->next >= st->naddrs) {
                    errno = ECONNREFUSED;
                    fail(j);
                    return 1;
                }
                wasi_sockets_network_ip_address_t *ip = &st->addrs[st->next++];
                wasi_sockets_network_ip_address_family_t fam =
                    ip->tag == WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4
                        ? WASI_SOCKETS_NETWORK_IP_ADDRESS_FAMILY_IPV4
                        : WASI_SOCKETS_NETWORK_IP_ADDRESS_FAMILY_IPV6;
                Sock *k = calloc(1, sizeof *k);
                if (!k) abort();
                wasi_sockets_network_ip_socket_address_t to = sockaddr_of(ip, (int)j->a);
                if (!wasi_sockets_tcp_create_socket_create_tcp_socket(fam, &k->tcp, &e)) {
                    free(k);
                    if (st->next < st->naddrs) continue;
                    net_fail(j, e);
                    return 1;
                }
                if (!wasi_sockets_tcp_method_tcp_socket_start_connect(
                        wasi_sockets_tcp_borrow_tcp_socket(k->tcp), net(), &to, &e)) {
                    sock_close(k);
                    if (st->next < st->naddrs) continue;
                    net_fail(j, e);
                    return 1;
                }
                st->k = k;
                st->phase = 2;
            }
            Sock *k = st->k;
            wasi_sockets_tcp_borrow_tcp_socket_t b = wasi_sockets_tcp_borrow_tcp_socket(k->tcp);
            wasi_sockets_tcp_tuple2_own_input_stream_own_output_stream_t io;
            if (!wasi_sockets_tcp_method_tcp_socket_finish_connect(b, &io, &e)) {
                if (e == WASI_SOCKETS_NETWORK_ERROR_CODE_WOULD_BLOCK) {
                    wait_on(j, wasi_sockets_tcp_method_tcp_socket_subscribe(b));
                    return 0;
                }
                sock_close(k);
                st->k = NULL;
                st->phase = 1;
                if (st->next < st->naddrs) continue;
                net_fail(j, e);
                return 1;
            }
            k->in = io.f0;
            k->out = io.f1;
            k->streams = 1;
            ends_text(j, k);
            j->r = sock_add(k);
            st->k = NULL;
            return 1;
        }
    }
    case 33: { /* receive a datagram on a, at most b bytes: r = handle, r2 = count */
        Sock *k = sock_of(j->a);
        if (!k || !k->dstreams) {
            errno = EBADF;
            fail(j);
            return 1;
        }
        wasi_sockets_udp_borrow_incoming_datagram_stream_t din =
            wasi_sockets_udp_borrow_incoming_datagram_stream(k->din);
        wasi_sockets_udp_list_incoming_datagram_t got;
        wasi_sockets_network_error_code_t e;
        if (!wasi_sockets_udp_method_incoming_datagram_stream_receive(din, 1, &got, &e)) {
            net_fail(j, e);
            return 1;
        }
        if (got.len == 0) {
            wasi_sockets_udp_list_incoming_datagram_free(&got);
            wait_on(j, wasi_sockets_udp_method_incoming_datagram_stream_subscribe(din));
            return 0;
        }
        size_t n = j->b > 0 ? (size_t)j->b : 1;
        size_t m = got.ptr[0].data.len < n ? got.ptr[0].data.len : n;
        char *buf = calloc(n, 1);
        if (!buf) abort();
        memcpy(buf, got.ptr[0].data.ptr, m);
        sockaddr_text(j, &got.ptr[0].remote_address);
        wasi_sockets_udp_list_incoming_datagram_free(&got);
        j->r = (int64_t)(uintptr_t)buf;
        j->r2 = (int64_t)m;
        return 1;
    }
    case 34: { /* send t's bytes from socket a to s = "port host" */
        Sock *k = sock_of(j->a);
        if (!k || !k->dstreams) {
            errno = EBADF;
            fail(j);
            return 1;
        }
        const char *sp = strchr(j->s, ' ');
        int r = resolve_step(j, st, sp ? sp + 1 : "");
        if (r <= 0) return r < 0;
        wasi_sockets_udp_borrow_outgoing_datagram_stream_t dout =
            wasi_sockets_udp_borrow_outgoing_datagram_stream(k->dout);
        uint64_t permit;
        wasi_sockets_network_error_code_t e;
        if (!wasi_sockets_udp_method_outgoing_datagram_stream_check_send(dout, &permit, &e)) {
            net_fail(j, e);
            return 1;
        }
        if (permit == 0) {
            wait_on(j, wasi_sockets_udp_method_outgoing_datagram_stream_subscribe(dout));
            return 0;
        }
        /* An address of the socket's family. */
        wasi_sockets_network_ip_socket_address_t mine;
        int v4 = !wasi_sockets_udp_method_udp_socket_local_address(
                     wasi_sockets_udp_borrow_udp_socket(k->usock), &mine, &e) ||
                 mine.tag == WASI_SOCKETS_NETWORK_IP_SOCKET_ADDRESS_IPV4;
        int pick = 0;
        for (int i = 0; i < st->naddrs; i++)
            if ((st->addrs[i].tag == WASI_SOCKETS_NETWORK_IP_ADDRESS_IPV4) == v4) {
                pick = i;
                break;
            }
        wasi_sockets_udp_outgoing_datagram_t d;
        d.data = (imports_list_u8_t){(uint8_t *)j->t, j->tlen};
        d.remote_address.is_some = true;
        d.remote_address.val = sockaddr_of(&st->addrs[pick], atoi(j->s));
        wasi_sockets_udp_list_outgoing_datagram_t list = {&d, 1};
        uint64_t sent;
        if (!wasi_sockets_udp_method_outgoing_datagram_stream_send(dout, &list, &sent, &e)) {
            net_fail(j, e);
            return 1;
        }
        j->r = (int64_t)j->tlen;
        return 1;
    }
    case 35: { /* resolve s: "address\0" each, without repeats */
        int r = resolve_step(j, st, j->s);
        if (r <= 0) return r < 0;
        for (int i = 0; i < st->naddrs; i++) {
            char host[64];
            ip_text(&st->addrs[i], host, sizeof host);
            int seen = 0;
            for (size_t o = 0; j->text && o < j->text_len; o += strlen(j->text + o) + 1)
                if (!strcmp(j->text + o, host)) seen = 1;
            if (!seen) text_add(j, host, strlen(host) + 1);
        }
        if (!j->text) text_add(j, "", 0);
        return 1;
    }
    case 20: { /* read: a = socket, b = at most this many bytes; r = handle, r2 = count */
        Sock *k = sock_of(j->a);
        if (!k || !k->streams) {
            errno = EBADF;
            fail(j);
            return 1;
        }
        size_t n = j->b > 0 ? (size_t)j->b : 1;
        if (!st->buf) {
            st->buf = calloc(n, 1);
            if (!st->buf) abort();
        }
        ptrdiff_t got = sock_try_read(j->a, k, st->buf, n, &p);
        if (got == -3) {
            wait_on(j, p);
            return 0;
        }
        if (got < 0) {
            fail(j);
            return 1;
        }
        j->r = (int64_t)(uintptr_t)st->buf;
        j->r2 = got;
        st->buf = NULL;
        return 1;
    }
    case 21: { /* write t's bytes to socket a (after the writes made before it) */
        Sock *k = sock_of(j->a);
        if (!k || !k->streams) {
            errno = EBADF;
            fail(j);
            return 1;
        }
        if (k->writer && k->writer != j) {
            k->queued = realloc(k->queued, (k->nqueued + 1) * sizeof *k->queued);
            if (!k->queued) abort();
            k->queued[k->nqueued++] = j; /* (stepped again when its turn comes) */
            return 0;
        }
        k->writer = j;
        while (st->done < j->tlen) {
            ptrdiff_t w = sock_try_write(j->a, k, j->t + st->done, j->tlen - st->done, &p);
            if (w == -3) {
                wait_on(j, p);
                return 0;
            }
            if (w < 0) {
                fail(j);
                next_writer(k);
                return 1;
            }
            st->done += (size_t)w;
        }
        j->r = (int64_t)j->tlen;
        next_writer(k);
        return 1;
    }
    case 22: { /* read socket a to its end as text (UTF-8) */
        Sock *k = sock_of(j->a);
        if (!k || !k->streams) {
            errno = EBADF;
            fail(j);
            return 1;
        }
        for (;;) {
            if (st->len == st->cap) {
                st->cap = st->cap ? st->cap * 2 : 4096;
                st->buf = realloc(st->buf, st->cap);
                if (!st->buf) abort();
            }
            ptrdiff_t got = sock_try_read(j->a, k, st->buf + st->len, st->cap - st->len, &p);
            if (got == -3) {
                wait_on(j, p);
                return 0;
            }
            if (got < 0) {
                fail(j);
                return 1;
            }
            if (got == 0) break;
            st->len += (size_t)got;
        }
        if (!valid_utf8((unsigned char *)st->buf, st->len)) {
            j->r = -K_INVALID_DATA;
            return 1;
        }
        text_add(j, st->buf ? st->buf : "", st->len);
        return 1;
    }
    case 36: { /* TLS handshake on socket a: b = mode, s = host, t = PEM text, NUL, PEM */
        Sock *k = sock_of(j->a);
        char err[512];
        if (!k || !k->streams) {
            errno = EBADF;
            fail(j);
            return 1;
        }
        if (st->phase == 0) {
            size_t n1 = j->t ? strlen(j->t) : 0;
            const char *pem2 = j->t && n1 < j->tlen ? j->t + n1 + 1 : "";
            size_t n2 = j->t && n1 < j->tlen ? j->tlen - n1 - 1 : 0;
            if (pico_tls_begin((int)j->a, (int)j->b, j->s ? j->s : "", j->t ? j->t : "", n1, pem2,
                              n2, err, sizeof err) < 0) {
                j->r = -15;
                text_add(j, err, strlen(err));
                return 1;
            }
            st->phase = 1;
        }
        int r = pico_tls_step((int)j->a, err, sizeof err);
        if (r == 1 || r == 2) {
            wait_on(j, r == 1 ? wasi_io_streams_method_input_stream_subscribe(
                                    wasi_io_streams_borrow_input_stream(k->in))
                              : wasi_io_streams_method_output_stream_subscribe(
                                    wasi_io_streams_borrow_output_stream(k->out)));
            return 0;
        }
        if (r < 0) {
            j->r = -15;
            text_add(j, err, strlen(err));
        }
        return 1;
    }
    default:
        errno = ENOSYS;
        fail(j);
        return 1;
    }
}

/* Starts socket job `j`. */
static void net_submit(Job *j) {
    j->wasi_pollable = -1;
    if (net_step(j)) {
        state_free(j);
        finish(j);
    }
}

/* Ends the jobs waiting on socket `id` (shut down for reading, or closed): reads see the end,
 * accepts fail. */
static void net_release(int64_t id, int closed) {
    for (size_t i = 0; i < nwaiting;) {
        Job *j = waiting[i];
        if (j->a != id || j->op == 21 || j->op == 34 || j->op == 36) {
            i++;
            continue;
        }
        waiting[i] = waiting[--nwaiting];
        wasi_io_poll_pollable_drop_own((wasi_io_poll_own_pollable_t){j->wasi_pollable});
        j->wasi_pollable = -1;
        if (closed || j->op == 31 || j->op == 33) {
            errno = closed ? EBADF : EINVAL;
            fail(j);
        } else if (j->op == 20) {
            NetState *st = j->wasi;
            j->r = (int64_t)(uintptr_t)st->buf;
            j->r2 = 0;
            st->buf = NULL;
        } else if (j->op == 22) {
            NetState *st = j->wasi;
            text_add(j, st->buf ? st->buf : "", st->len);
        }
        state_free(j);
        finish(j);
    }
}

/* The event loop's wait on WASI: up to `ms` (-1: no limit) for any waiting socket job, which
 * then takes its next steps. */
void pico_wasi_poll(int64_t ms) {
    if (nwaiting == 0) {
        if (ms > 0) {
            struct timespec t = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000};
            nanosleep(&t, NULL);
        }
        return;
    }
    size_t n = nwaiting;
    wasi_io_poll_borrow_pollable_t *list = calloc(n + 1, sizeof *list);
    if (!list) abort();
    for (size_t i = 0; i < n; i++)
        list[i] = wasi_io_poll_borrow_pollable((wasi_io_poll_own_pollable_t){waiting[i]->wasi_pollable});
    wasi_io_poll_own_pollable_t timer = {-1};
    size_t total = n;
    if (ms >= 0) {
        timer = wasi_clocks_monotonic_clock_subscribe_duration((uint64_t)ms * 1000000);
        list[total++] = wasi_io_poll_borrow_pollable(timer);
    }
    wasi_io_poll_list_borrow_pollable_t in = {list, total};
    imports_list_u32_t ready;
    wasi_io_poll_poll(&in, &ready);
    free(list);
    if (timer.__handle >= 0) wasi_io_poll_pollable_drop_own(timer);
    /* The ready jobs, taken off the list first (stepping may add jobs). */
    Job **todo = calloc(ready.len + 1, sizeof *todo);
    if (!todo) abort();
    size_t ntodo = 0;
    for (size_t r = 0; r < ready.len; r++)
        if (ready.ptr[r] < n) todo[ntodo++] = waiting[ready.ptr[r]];
    imports_list_u32_free(&ready);
    for (size_t t = 0; t < ntodo; t++) {
        for (size_t i = 0; i < nwaiting; i++)
            if (waiting[i] == todo[t]) {
                waiting[i] = waiting[--nwaiting];
                break;
            }
    }
    for (size_t t = 0; t < ntodo; t++) {
        Job *j = todo[t];
        wasi_io_poll_pollable_drop_own((wasi_io_poll_own_pollable_t){j->wasi_pollable});
        j->wasi_pollable = -1;
        if (net_step(j)) {
            state_free(j);
            finish(j);
        }
    }
    free(todo);
}

/* Shutdown (op 120) and close (121) of socket `a`. */
static int64_t net_control(int64_t op, int64_t a, int64_t b) {
    Sock *k = sock_of(a);
    if (!k) return -1;
    if (op == 120) {
        if (b != 0) pico_tls_close_notify((int)a);
        if (!k->udp) {
            wasi_sockets_network_error_code_t e;
            wasi_sockets_tcp_method_tcp_socket_shutdown(
                wasi_sockets_tcp_borrow_tcp_socket(k->tcp),
                b == 0 ? WASI_SOCKETS_TCP_SHUTDOWN_TYPE_RECEIVE
                : b == 1 ? WASI_SOCKETS_TCP_SHUTDOWN_TYPE_SEND
                         : WASI_SOCKETS_TCP_SHUTDOWN_TYPE_BOTH,
                &e);
        }
        if (b != 1) {
            k->shut_rd = 1;
            net_release(a, 0);
        }
        return 0;
    }
    pico_tls_free((int)a);
    net_release(a, 1);
    for (size_t i = 0; i < k->nqueued; i++) {
        errno = EBADF;
        fail(k->queued[i]);
        state_free(k->queued[i]);
        finish(k->queued[i]);
    }
    free(k->queued);
    sock_close(k);
    socks[a - SOCK_BASE] = NULL;
    return 0;
}

extern int pico_argc;
extern char **pico_argv;
extern char **environ;

/* This process on WASI (spec §13.3's availability table: args, env and exit; no Command). */
static int64_t process_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PicoStr *s,
                          const PicoStr *t) {
    (void)b, (void)c;
    switch (op) {
    case 113: { /* the arguments, each followed by a NUL */
        Job *j = result_job(q);
        text_add(j, "", 0);
        for (int i = 1; i < pico_argc; i++) text_add(j, pico_argv[i], strlen(pico_argv[i]) + 1);
        return j->id;
    }
    case 114: { /* getenv(s): r = 1 when set, text = its value */
        Job *j = result_job(q);
        char *name = cstr(s, NULL);
        const char *v = getenv(name);
        free(name);
        j->r = v != NULL;
        if (v) text_add(j, v, strlen(v));
        return j->id;
    }
    case 115: { /* setenv(s, t) */
        char *name = cstr(s, NULL), *value = cstr(t, NULL);
        int r = setenv(name, value, 1);
        free(name);
        free(value);
        return r;
    }
    case 116: { /* unsetenv(s) */
        char *name = cstr(s, NULL);
        int r = unsetenv(name);
        free(name);
        return r;
    }
    case 117: { /* every variable: "NAME=value\0" each */
        Job *j = result_job(q);
        text_add(j, "", 0);
        for (char **e = environ; e && *e; e++) text_add(j, *e, strlen(*e) + 1);
        return j->id;
    }
    case 118: /* exit(a) */
        pico_exit((int)a);
    case 119: return 1; /* (WASI has no process ids) */
    default: return -1;
    }
}
#endif

/* Starts job `j` (op < 100), which reports to its queue when it's done. */
static void os_submit(Job *j) {
    int64_t op = j->op;
    if (op == 16) {
        watch_next(j);
        return;
    }
#if defined(__wasi__)
    /* No threads: a file job runs now (it's polled on the loop's next turn); a socket job
     * goes as far as it can and waits on the loop for the rest. */
    if ((op >= 20 && op <= 22) || (op >= 30 && op <= 36)) {
        net_submit(j);
        return;
    }
    run(j);
    finish(j);
    return;
#endif
    if ((op >= 20 && op <= 23) || (op >= 30 && op <= 36)) {
        run_solo(j);
        return;
    }
    start_pool();
    pthread_mutex_lock(&pool_lock);
    if (work_tail) work_tail->next = j;
    else work_head = j;
    work_tail = j;
    pthread_cond_signal(&pool_cond);
    pthread_mutex_unlock(&pool_lock);
}

/* The POSIX control operations (op >= 100); -1 for unknown ones. */
static int64_t os_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PicoStr *s,
                     const PicoStr *t) {
    switch (op) {
    case 107: /* cancel read job a (job 20; `Reader.close()`, #100) */
        cancel_read(q, a);
        return 0;
    case 106: /* close watcher a */
        watch_close(a);
        return 0;
    case 110:
    case 111:
    case 112:
    case 113:
    case 114:
    case 115:
    case 116:
    case 117:
    case 118:
    case 119: return process_op(q, op, a, b, c, s, t);
    case 134: { /* read file s whole, now: r = a buffer handle (or -1), r2 = its size */
        Job *j = result_job(q);
        j->r = -1;
        char *path = cstr(s, NULL);
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        free(path);
        if (fd < 0) return j->id;
        size_t cap = 4096, n = 0;
        char *m = malloc(cap);
        if (!m) abort();
        for (;;) {
            if (n == cap) {
                cap *= 2;
                m = realloc(m, cap);
                if (!m) abort();
            }
            ssize_t k = read(fd, m + n, cap - n);
            if (k < 0 && errno == EINTR) continue;
            if (k <= 0) break;
            n += (size_t)k;
        }
        close(fd);
        j->r = (int64_t)(uintptr_t)m;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 135: { /* readlink(s): text = the link's target ("" when s isn't a link) */
        Job *j = result_job(q);
        char buf[4096];
        char *path = cstr(s, NULL);
        ssize_t k = readlink(path, buf, sizeof buf - 1);
        free(path);
        if (k > 0) text_add(j, buf, (size_t)k);
        return j->id;
    }
#if defined(__wasi__)
    case 120: /* shutdown socket a */
    case 121: /* close socket a */
        return net_control(op, a, b);
#endif
#if PICO_IO_POSIX
    case 120: /* shutdown socket a: b = 0 reading, 1 writing, 2 both (TLS: close_notify first) */
        if (b != 0) pico_tls_close_notify((int)a);
        return shutdown((int)a, b == 0 ? SHUT_RD : b == 1 ? SHUT_WR : SHUT_RDWR) < 0 ? -1 : 0;
    case 121: /* close socket a (and its TLS session) */
        pico_tls_free((int)a);
        return close((int)a) < 0 ? -1 : 0;
#endif
    case 105: { /* the working directory, as a polled job's text */
        Job *j = calloc(1, sizeof *j);
        if (!j) abort();
        j->id = q->next_id++;
        char buf[4096];
        if (getcwd(buf, sizeof buf)) text_add(j, buf, strlen(buf));
        j->next = q->taken;
        q->taken = j;
        return j->id;
    }
    default: return -1;
    }
}

#elif defined(_WIN32)
/* ---- Windows: files and `watch` (P8.3b) over the W functions, paths UTF-8 on the pico
 * side; processes and sockets come with P8.3c. Errors become the POSIX errno values the
 * kinds are mapped from (`win_errno`), so messages match the other platforms. */
#include <windows.h>
#include <shellapi.h>

static char *cstr(const PicoStr *s, size_t *len);

/* `s` (UTF-8) as UTF-16, malloc'd. */
static wchar_t *wide(const char *s) {
    int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, NULL, 0);
    wchar_t *w = malloc((size_t)(n > 0 ? n : 1) * sizeof *w);
    if (!w) abort();
    if (n <= 0 || !MultiByteToWideChar(CP_UTF8, 0, s, -1, w, n)) w[0] = 0;
    return w;
}

/* `w` as UTF-8, malloc'd. */
static char *narrow(const wchar_t *w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    char *s = malloc((size_t)(n > 0 ? n : 1));
    if (!s) abort();
    if (n <= 0 || !WideCharToMultiByte(CP_UTF8, 0, w, -1, s, n, NULL, NULL)) s[0] = 0;
    return s;
}

static int is_sep(char c) { return c == '/' || c == '\\'; }

static DWORD attributes(const char *path) {
    wchar_t *w = wide(path);
    DWORD a = GetFileAttributesW(w);
    free(w);
    return a;
}

/* Sets errno for Windows error `e` on `path`. A missing path under something that isn't a
 * directory is ENOTDIR, as on POSIX (Windows says the path wasn't found). */
static void win_errno(DWORD e, const char *path) {
    int en;
    switch (e) {
    case ERROR_FILE_NOT_FOUND:
    case ERROR_PATH_NOT_FOUND:
    case ERROR_INVALID_NAME:
    case ERROR_INVALID_DRIVE:
    case ERROR_BAD_NETPATH:
    case ERROR_BAD_NET_NAME: en = ENOENT; break;
    case ERROR_ALREADY_EXISTS:
    case ERROR_FILE_EXISTS: en = EEXIST; break;
    case ERROR_ACCESS_DENIED:
    case ERROR_SHARING_VIOLATION:
    case ERROR_LOCK_VIOLATION:
    case ERROR_WRITE_PROTECT: en = EACCES; break;
    case ERROR_DIRECTORY: en = ENOTDIR; break;
    case ERROR_DIR_NOT_EMPTY: en = ENOTEMPTY; break;
    default: en = EIO; break;
    }
    if (en == ENOENT && path) {
        size_t n = strlen(path);
        char *p = malloc(n + 1);
        if (!p) abort();
        memcpy(p, path, n + 1);
        for (size_t i = 1; i < n; i++) {
            if (!is_sep(p[i]) || p[i - 1] == ':') continue;
            p[i] = 0;
            DWORD a = attributes(p);
            p[i] = path[i];
            if (a != INVALID_FILE_ATTRIBUTES && !(a & FILE_ATTRIBUTE_DIRECTORY)) {
                en = ENOTDIR;
                break;
            }
        }
        free(p);
    }
    errno = en;
}

/* The last error on `path`, as `fail` reports it. */
static void win_fail(Job *j, const char *path) {
    win_errno(GetLastError(), path);
    fail(j);
}

static void fail_with(Job *j, int en) {
    errno = en;
    fail(j);
}

/* 100 ns since 1601 → ns since 1970. */
static int64_t unix_ns(FILETIME t) {
    int64_t v = (int64_t)(((uint64_t)t.dwHighDateTime << 32) | t.dwLowDateTime);
    return (v - 116444736000000000LL) * 100;
}

static int fs_info(const char *path, int follow, Info *out) {
    wchar_t *w = wide(path);
    WIN32_FILE_ATTRIBUTE_DATA d;
    if (!GetFileAttributesExW(w, GetFileExInfoStandard, &d)) {
        free(w);
        win_errno(GetLastError(), path);
        return -1;
    }
    DWORD attr = d.dwFileAttributes;
    uint64_t size = ((uint64_t)d.nFileSizeHigh << 32) | d.nFileSizeLow;
    FILETIME mtime = d.ftLastWriteTime;
    if ((attr & FILE_ATTRIBUTE_REPARSE_POINT) && follow) {
        /* What the link leads to. */
        HANDLE h = CreateFileW(w, FILE_READ_ATTRIBUTES,
                               FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL,
                               OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, NULL);
        BY_HANDLE_FILE_INFORMATION bi;
        if (h == INVALID_HANDLE_VALUE || !GetFileInformationByHandle(h, &bi)) {
            DWORD e = GetLastError();
            if (h != INVALID_HANDLE_VALUE) CloseHandle(h);
            free(w);
            win_errno(e, path);
            return -1;
        }
        CloseHandle(h);
        attr = bi.dwFileAttributes;
        size = ((uint64_t)bi.nFileSizeHigh << 32) | bi.nFileSizeLow;
        mtime = bi.ftLastWriteTime;
    }
    free(w);
    out->kind = (attr & FILE_ATTRIBUTE_REPARSE_POINT) && !follow ? 'l'
                : (attr & FILE_ATTRIBUTE_DIRECTORY)            ? 'd'
                                                               : 'f';
    out->size = out->kind == 'd' ? 0 : (int64_t)size;
    out->mtime_ns = unix_ns(mtime);
    out->ino = 0;
    return 0;
}

struct DirIter {
    HANDLE h;
    WIN32_FIND_DATAW data;
    int first;
    char *name;
};

static DirIter *dir_open(const char *path) {
    size_t n = strlen(path);
    char *pattern = malloc(n + 3);
    if (!pattern) abort();
    memcpy(pattern, path, n);
    size_t k = n;
    if (k > 0 && !is_sep(pattern[k - 1])) pattern[k++] = '\\';
    pattern[k++] = '*';
    pattern[k] = 0;
    wchar_t *w = wide(pattern);
    free(pattern);
    DirIter *it = calloc(1, sizeof *it);
    if (!it) abort();
    it->h = FindFirstFileW(w, &it->data);
    free(w);
    if (it->h == INVALID_HANDLE_VALUE) {
        DWORD e = GetLastError();
        free(it);
        DWORD a = attributes(path);
        if (a != INVALID_FILE_ATTRIBUTES && !(a & FILE_ATTRIBUTE_DIRECTORY)) errno = ENOTDIR;
        else win_errno(e, path);
        return NULL;
    }
    it->first = 1;
    return it;
}

static const char *dir_next(DirIter *it) {
    for (;;) {
        if (!it->first && !FindNextFileW(it->h, &it->data)) return NULL;
        it->first = 0;
        const wchar_t *f = it->data.cFileName;
        if (!wcscmp(f, L".") || !wcscmp(f, L"..")) continue;
        free(it->name);
        it->name = narrow(f);
        return it->name;
    }
}

static void dir_close(DirIter *it) {
    FindClose(it->h);
    free(it->name);
    free(it);
}

static void wake_open(Watcher *w) {
    HANDLE e = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (!e) abort();
    w->wake[0] = (intptr_t)e;
}

static void wake_signal(Watcher *w) { SetEvent((HANDLE)w->wake[0]); }

static void wake_close(Watcher *w) { CloseHandle((HANDLE)w->wake[0]); }

/* A change notification on the watched directory (a watched file's directory). */
static void wait_os(Watcher *w, int ms) {
    char *dir = NULL;
    if (w->snap.n == 1 && !*w->snap.e[0].rel) {
        size_t n = strlen(w->root);
        while (n > 0 && !is_sep(w->root[n - 1])) n--;
        dir = malloc(n + 2);
        if (!dir) abort();
        if (n == 0) {
            strcpy(dir, ".");
        } else {
            memcpy(dir, w->root, n);
            dir[n] = 0;
        }
    }
    wchar_t *wd = wide(dir ? dir : w->root);
    free(dir);
    HANDLE ch = FindFirstChangeNotificationW(
        wd, w->recursive && !dir,
        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE |
            FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_ATTRIBUTES);
    free(wd);
    HANDLE hs[2] = {(HANDLE)w->wake[0], ch};
    DWORD got = WaitForMultipleObjects(ch == INVALID_HANDLE_VALUE ? 1 : 2, hs, FALSE, (DWORD)ms);
    if (ch != INVALID_HANDLE_VALUE) FindCloseChangeNotification(ch);
    if (got == WAIT_OBJECT_0 + 1) Sleep(20); /* let a burst of changes finish */
}

/* The whole file at `path` (malloc'd), or NULL with errno set. */
static char *read_all(const char *path, size_t *len) {
    DWORD a = attributes(path);
    if (a != INVALID_FILE_ATTRIBUTES && (a & FILE_ATTRIBUTE_DIRECTORY)) {
        errno = EISDIR;
        return NULL;
    }
    wchar_t *w = wide(path);
    HANDLE h = CreateFileW(w, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    free(w);
    if (h == INVALID_HANDLE_VALUE) {
        win_errno(GetLastError(), path);
        return NULL;
    }
    size_t cap = 4096, n = 0;
    char *buf = malloc(cap);
    if (!buf) abort();
    for (;;) {
        if (n == cap) {
            cap *= 2;
            buf = realloc(buf, cap);
            if (!buf) abort();
        }
        DWORD k = 0;
        DWORD want = cap - n > 0x40000000 ? 0x40000000 : (DWORD)(cap - n);
        if (!ReadFile(h, buf + n, want, &k, NULL)) {
            win_errno(GetLastError(), path);
            free(buf);
            CloseHandle(h);
            return NULL;
        }
        if (k == 0) break;
        n += k;
    }
    CloseHandle(h);
    *len = n;
    return buf;
}

static int write_handle(HANDLE h, const char *p, size_t n) {
    while (n > 0) {
        DWORD k = 0;
        DWORD want = n > 0x40000000 ? 0x40000000 : (DWORD)n;
        if (!WriteFile(h, p, want, &k, NULL)) return -1;
        p += k;
        n -= k;
    }
    return 0;
}

static int make_dir(const char *path) {
    wchar_t *w = wide(path);
    BOOL ok = CreateDirectoryW(w, NULL);
    free(w);
    if (!ok) win_errno(GetLastError(), path);
    return ok ? 0 : -1;
}

static int mkdir_all(const char *path) {
    size_t n = strlen(path);
    char *p = malloc(n + 1);
    if (!p) abort();
    memcpy(p, path, n + 1);
    for (size_t i = 1; i < n; i++) {
        if (!is_sep(p[i]) || p[i - 1] == ':' || is_sep(p[i - 1])) continue;
        p[i] = 0;
        wchar_t *w = wide(p);
        CreateDirectoryW(w, NULL); /* (it may exist; the last step reports problems) */
        free(w);
        p[i] = path[i];
    }
    free(p);
    if (make_dir(path) < 0) {
        DWORD a = attributes(path);
        if (errno == EEXIST && a != INVALID_FILE_ATTRIBUTES && (a & FILE_ATTRIBUTE_DIRECTORY))
            return 0;
        return -1;
    }
    return 0;
}

/* Removes the file or (empty) directory `path`; a link to a directory is a directory to
 * Windows. */
static int remove_one(const char *path, const Info *in) {
    wchar_t *w = wide(path);
    DWORD a = GetFileAttributesW(w);
    BOOL ok = in->kind == 'd' || (a != INVALID_FILE_ATTRIBUTES && (a & FILE_ATTRIBUTE_DIRECTORY))
                  ? RemoveDirectoryW(w)
                  : DeleteFileW(w);
    free(w);
    if (!ok) win_errno(GetLastError(), path);
    return ok ? 0 : -1;
}

static int remove_all(const char *path) {
    Info in;
    if (fs_info(path, 0, &in) < 0) return -1;
    if (in.kind == 'd') {
        DirIter *d = dir_open(path);
        if (!d) return -1;
        const char *name;
        while ((name = dir_next(d))) {
            char *child = join_rel(path, name);
            int r = remove_all(child);
            free(child);
            if (r < 0) {
                int e = errno;
                dir_close(d);
                errno = e;
                return -1;
            }
        }
        dir_close(d);
    }
    return remove_one(path, &in);
}

/* ---- the socket shims (see `std/net` below) ---- */

typedef SOCKET sock_t;
#define SOCK_NONE ((int64_t)INVALID_SOCKET)
/* (mswsock.h: ignore ICMP "port unreachable" on UDP sockets, which would fail a later
 * receive.) */
#ifndef SIO_UDP_CONNRESET
#define SIO_UDP_CONNRESET _WSAIOW(IOC_VENDOR, 12)
#endif

static pthread_once_t wsa_once = PTHREAD_ONCE_INIT;

static void wsa_start(void) {
    WSADATA d;
    if (WSAStartup(MAKEWORD(2, 2), &d) != 0) abort();
}

static void net_init(void) { pthread_once(&wsa_once, wsa_start); }

/* A Winsock error as the POSIX errno the kinds come from. */
static int wsa_errno(int e) {
    switch (e) {
    case WSAECONNREFUSED: return ECONNREFUSED;
    case WSAECONNRESET:
    case WSAECONNABORTED:
    case WSAESHUTDOWN:
    case WSAENETRESET: return ECONNRESET;
    case WSAEADDRINUSE: return EADDRINUSE;
    case WSAEADDRNOTAVAIL: return EADDRNOTAVAIL;
    case WSAETIMEDOUT: return ETIMEDOUT;
    case WSAEACCES: return EACCES;
    case WSAEINTR: return EINTR;
    case WSAENETUNREACH: return ENETUNREACH;
    case WSAEHOSTUNREACH: return EHOSTUNREACH;
    default: return EIO;
    }
}

static void sock_failed(void) { errno = wsa_errno(WSAGetLastError()); }

static void sock_close(int64_t s) { closesocket((SOCKET)s); }

static void sock_private(int64_t s) { SetHandleInformation((HANDLE)(intptr_t)s, HANDLE_FLAG_INHERIT, 0); }

static void sock_udp(int64_t s) {
    BOOL off = FALSE;
    DWORD n = 0;
    WSAIoctl((SOCKET)s, SIO_UDP_CONNRESET, &off, sizeof off, NULL, 0, &n, NULL, NULL);
}

/* Whether handle `a` is a socket (pipe jobs read and write both). */
static int is_socket(int64_t a) {
    if (a <= 2) return 0;
    net_init();
    int type = 0, len = sizeof type;
    return getsockopt((SOCKET)a, SOL_SOCKET, SO_TYPE, (char *)&type, &len) == 0;
}

/* After `shutdown` or a cancelled wait (op 120), a read sees the end, as on POSIX. */
static int read_ended(int e) {
    return e == WSAESHUTDOWN || e == WSAEINTR || e == WSA_OPERATION_ABORTED ||
           e == WSAECONNABORTED;
}

/* Reads from socket `a` (TLS sessions first): bytes read, 0 at the end, -1 with errno set. */
static ptrdiff_t sock_read(int64_t a, char *b, size_t n) {
    ptrdiff_t k = pico_tls_read((int)a, b, n);
    if (k != -2) return k;
    int got = recv((SOCKET)a, b, n > 0x40000000 ? 0x40000000 : (int)n, 0);
    if (got >= 0) return got;
    int e = WSAGetLastError();
    if (read_ended(e)) return 0;
    errno = wsa_errno(e);
    return -1;
}

static int sock_write(int64_t a, const char *p, size_t n) {
    ptrdiff_t t = pico_tls_write((int)a, p, n);
    if (t != -2) return t < 0 ? -1 : 0;
    while (n > 0) {
        int k = send((SOCKET)a, p, n > 0x40000000 ? 0x40000000 : (int)n, 0);
        if (k < 0) {
            int e = WSAGetLastError();
            errno = e == WSAECONNRESET || e == WSAECONNABORTED || e == WSAESHUTDOWN ? EPIPE
                                                                                   : wsa_errno(e);
            return -1;
        }
        p += k;
        n -= (size_t)k;
    }
    return 0;
}

/* ---- `std/process` (spec §13.3) on Windows ----
 * Pipe ends are handles (passed to pico as integers in place of POSIX fds; 0, 1 and 2 still
 * mean this process's standard streams). A child inherits only its three standard handles
 * (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`), so pipes made for concurrent spawns never leak into
 * each other's children. Windows has no signals: `kill` terminates the child with exit code
 * 128 + the signal's number and remembers the signal, so its status reads as on POSIX. */

/* `a` as a handle: this process's standard streams for 0, 1 and 2. */
static HANDLE handle_of(int64_t a) {
    if (a == 0) return GetStdHandle(STD_INPUT_HANDLE);
    if (a == 1) return GetStdHandle(STD_OUTPUT_HANDLE);
    if (a == 2) return GetStdHandle(STD_ERROR_HANDLE);
    return (HANDLE)(intptr_t)a;
}

/* Children not waited for yet: their process handles, and the signal `kill` sent. */
typedef struct Child {
    DWORD pid;
    HANDLE h;
    int signal;
} Child;

static pthread_mutex_t child_lock = PTHREAD_MUTEX_INITIALIZER;
static Child *live;
static size_t nlive, caplive;

/* (With child_lock held.) */
static int live_find(DWORD pid) {
    for (size_t i = 0; i < nlive; i++)
        if (live[i].pid == pid) return (int)i;
    return -1;
}

/* A growable UTF-8 string. */
typedef struct Str {
    char *p;
    size_t n, cap;
} Str;

static void str_add(Str *s, const char *p, size_t n) {
    if (s->n + n + 1 > s->cap) {
        s->cap = (s->n + n + 1) * 2;
        s->p = realloc(s->p, s->cap);
        if (!s->p) abort();
    }
    memcpy(s->p + s->n, p, n);
    s->n += n;
    s->p[s->n] = 0;
}

/* `arg` quoted for the Microsoft C runtime's command-line parsing (what CommandLineToArgvW
 * and nearly every program use): backslashes are literal except before a quote. */
static void quote_arg(Str *s, const char *arg) {
    if (*arg && !strpbrk(arg, " \t\n\v\"")) {
        str_add(s, arg, strlen(arg));
        return;
    }
    str_add(s, "\"", 1);
    for (const char *p = arg;; p++) {
        size_t slashes = 0;
        while (*p == '\\') {
            slashes++;
            p++;
        }
        if (!*p) {
            for (size_t i = 0; i < slashes * 2; i++) str_add(s, "\\", 1);
            break;
        }
        size_t n = *p == '"' ? slashes * 2 + 1 : slashes;
        for (size_t i = 0; i < n; i++) str_add(s, "\\", 1);
        str_add(s, p, 1);
    }
    str_add(s, "\"", 1);
}

/* Whether environment entry `e` ("NAME=value") is for `name` (`nlen` bytes, up to '='),
 * ignoring case as Windows does. */
static int same_var(const wchar_t *e, const wchar_t *name, size_t nlen) {
    const wchar_t *eq = wcschr(e + 1, L'='); /* ("=C:=C:\x" entries start with '=') */
    size_t elen = eq ? (size_t)(eq - e) : wcslen(e);
    return elen == nlen && CompareStringOrdinal(e, (int)elen, name, (int)nlen, TRUE) == CSTR_EQUAL;
}

/* A handle for the child's stream `i` that it inherits: our own, the null device, or a pipe
 * end (the other end in `*mine`, not inheritable). NULL on failure (errno set). */
static HANDLE child_stream(int i, int mode, HANDLE *mine) {
    SECURITY_ATTRIBUTES sa = {sizeof sa, NULL, TRUE};
    *mine = NULL;
    if (mode == 1) {
        HANDLE r, w;
        if (!CreatePipe(&r, &w, &sa, 0)) {
            win_errno(GetLastError(), NULL);
            return NULL;
        }
        *mine = i == 0 ? w : r;
        SetHandleInformation(*mine, HANDLE_FLAG_INHERIT, 0);
        return i == 0 ? r : w;
    }
    HANDLE own = mode == 0 ? GetStdHandle(i == 0 ? STD_INPUT_HANDLE
                                          : i == 1 ? STD_OUTPUT_HANDLE
                                                   : STD_ERROR_HANDLE)
                           : NULL;
    HANDLE d = NULL;
    if (own && own != INVALID_HANDLE_VALUE &&
        DuplicateHandle(GetCurrentProcess(), own, GetCurrentProcess(), &d, 0, TRUE,
                        DUPLICATE_SAME_ACCESS))
        return d;
    /* "null", or no stream of ours to give. */
    d = CreateFileW(L"NUL", i == 0 ? GENERIC_READ : GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE, &sa, OPEN_EXISTING, 0, NULL);
    if (d == INVALID_HANDLE_VALUE) {
        win_errno(GetLastError(), NULL);
        return NULL;
    }
    return d;
}

/* Spawns `prog` (op 110; the fields as on POSIX). `modes` bit 128: the arguments go on the
 * command line verbatim (`Command.shell`'s `cmd.exe /s /c "…"`). */
static void spawn_child(Job *j, int64_t argc, int64_t envc, int64_t modes, const char *prog,
                        const char *t, size_t tlen) {
    /* What the program printed comes before the child's output (#14). */
    fflush(stdout);
    fflush(stderr);
    size_t nf = 1 + (size_t)argc + (size_t)envc;
    const char **fields = calloc(nf, sizeof *fields);
    if (!fields) abort();
    size_t k = 0, start = 0;
    for (size_t i = 0; i <= tlen && k < nf; i++) {
        if (i == tlen || t[i] == 0) {
            fields[k++] = t + start;
            start = i + 1;
        }
    }
    while (k < nf) fields[k++] = "";
    Str cl = {0};
    quote_arg(&cl, prog);
    for (int64_t i = 0; i < argc; i++) {
        str_add(&cl, " ", 1);
        const char *a = fields[1 + i];
        if (modes & 128) str_add(&cl, a, strlen(a));
        else quote_arg(&cl, a);
    }
    wchar_t *cmdline = wide(cl.p);
    free(cl.p);
    /* The environment: ours (unless cleared) without the overridden names, then the
     * overrides. NULL (inherit ours) when nothing changes. */
    wchar_t *block = NULL;
    if (envc > 0 || (modes & 64)) {
        wchar_t **over = calloc((size_t)envc + 1, sizeof *over);
        if (!over) abort();
        for (int64_t v = 0; v < envc; v++) over[v] = wide(fields[1 + argc + v]);
        size_t cap = 1, n = 0;
        wchar_t *ours = (modes & 64) ? NULL : GetEnvironmentStringsW();
        for (const wchar_t *e = ours; e && *e; e += wcslen(e) + 1) cap += wcslen(e) + 1;
        for (int64_t v = 0; v < envc; v++) cap += wcslen(over[v]) + 1;
        block = malloc((cap + 1) * sizeof *block);
        if (!block) abort();
        for (const wchar_t *e = ours; e && *e; e += wcslen(e) + 1) {
            int replaced = 0;
            for (int64_t v = 0; v < envc && !replaced; v++) {
                const wchar_t *eq = wcschr(over[v], L'=');
                size_t nlen = eq ? (size_t)(eq - over[v]) : wcslen(over[v]);
                replaced = same_var(e, over[v], nlen);
            }
            if (replaced) continue;
            size_t len = wcslen(e) + 1;
            memcpy(block + n, e, len * sizeof *block);
            n += len;
        }
        if (ours) FreeEnvironmentStringsW(ours);
        for (int64_t v = 0; v < envc; v++) {
            size_t len = wcslen(over[v]) + 1;
            memcpy(block + n, over[v], len * sizeof *block);
            n += len;
            free(over[v]);
        }
        free(over);
        if (n == 0) block[n++] = 0; /* (an empty block is two NULs) */
        block[n] = 0;
    }
    wchar_t *dir = fields[0][0] ? wide(fields[0]) : NULL;
    HANDLE theirs[3] = {NULL, NULL, NULL}, mine[3] = {NULL, NULL, NULL};
    int err = 0;
    for (int i = 0; i < 3 && !err; i++) {
        theirs[i] = child_stream(i, (int)(modes >> (2 * i)) & 3, &mine[i]);
        if (!theirs[i]) err = errno;
    }
    PROCESS_INFORMATION pi = {0};
    if (!err) {
        SIZE_T size = 0;
        InitializeProcThreadAttributeList(NULL, 1, 0, &size);
        LPPROC_THREAD_ATTRIBUTE_LIST list = malloc(size);
        if (!list) abort();
        STARTUPINFOEXW si;
        memset(&si, 0, sizeof si);
        si.StartupInfo.cb = sizeof si;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = theirs[0];
        si.StartupInfo.hStdOutput = theirs[1];
        si.StartupInfo.hStdError = theirs[2];
        si.lpAttributeList = list;
        if (!InitializeProcThreadAttributeList(list, 1, 0, &size) ||
            !UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, theirs,
                                       sizeof theirs, NULL, NULL) ||
            !CreateProcessW(NULL, cmdline, NULL, NULL, TRUE,
                            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT, block, dir,
                            &si.StartupInfo, &pi)) {
            win_errno(GetLastError(), NULL);
            err = errno;
        }
        DeleteProcThreadAttributeList(list);
        free(list);
    }
    for (int i = 0; i < 3; i++) {
        if (theirs[i]) CloseHandle(theirs[i]);
        if (err && mine[i]) {
            CloseHandle(mine[i]);
            mine[i] = NULL;
        }
    }
    free(cmdline);
    free(block);
    free(dir);
    free(fields);
    if (err) {
        j->r = -kind_of(err);
        j->r2 = err;
        return;
    }
    CloseHandle(pi.hThread);
    pthread_mutex_lock(&child_lock);
    if (nlive == caplive) {
        caplive = caplive ? caplive * 2 : 8;
        live = realloc(live, caplive * sizeof *live);
        if (!live) abort();
    }
    live[nlive++] = (Child){pi.dwProcessId, pi.hProcess, 0};
    pthread_mutex_unlock(&child_lock);
    j->r = pi.dwProcessId;
    char line[96];
    long long fd[3];
    for (int i = 0; i < 3; i++) fd[i] = mine[i] ? (long long)(intptr_t)mine[i] : -1;
    int n = snprintf(line, sizeof line, "%lld %lld %lld", fd[0], fd[1], fd[2]);
    text_add(j, line, (size_t)n);
}

/* Job 23: waits for child a; r = its exit code (128 + the signal `kill` sent), r2 = that
 * signal (or 0). */
static void child_wait(Job *j) {
    pthread_mutex_lock(&child_lock);
    int i = live_find((DWORD)j->a);
    HANDLE h = i >= 0 ? live[i].h : NULL;
    pthread_mutex_unlock(&child_lock);
    if (!h) {
        fail_with(j, ECHILD);
        return;
    }
    WaitForSingleObject(h, INFINITE);
    DWORD code = 0;
    GetExitCodeProcess(h, &code);
    pthread_mutex_lock(&child_lock);
    i = live_find((DWORD)j->a);
    int sig = i >= 0 ? live[i].signal : 0;
    if (i >= 0) live[i] = live[--nlive];
    pthread_mutex_unlock(&child_lock);
    CloseHandle(h);
    j->r = sig ? 128 + sig : (int64_t)code;
    j->r2 = sig;
}

/* Reads pipe, stream or socket `a` (job 20): r = buffer, r2 = count (0 at the end). */
static void pipe_read(Job *j) {
    size_t n = j->b > 0 ? (size_t)j->b : 1;
    char *b = calloc(n, 1);
    if (!b) abort();
    if (is_socket(j->a)) {
        ptrdiff_t got = sock_read(j->a, b, n);
        if (got < 0) {
            free(b);
            fail(j);
            return;
        }
        j->r = (int64_t)(uintptr_t)b;
        j->r2 = got;
        return;
    }
    /* Cancellable (`Reader.close()`, #100): then it ends as at the end of the stream. (A cancel
     * in the moment between registering and `ReadFile` starting is missed.) */
    Cancel *c = cancel_register(j);
    DWORD k = 0;
    BOOL ok = c ? ReadFile(handle_of(j->a), b, n > 0x40000000 ? 0x40000000 : (DWORD)n, &k, NULL)
                : FALSE;
    DWORD e = ok ? 0 : c ? GetLastError() : ERROR_OPERATION_ABORTED;
    int cancelled = c ? cancel_unregister(c) : 1;
    if (!ok) {
        if (e != ERROR_BROKEN_PIPE && e != ERROR_HANDLE_EOF && !cancelled) {
            free(b);
            win_errno(e, NULL);
            fail(j);
            return;
        }
        k = 0;
    }
    j->r = (int64_t)(uintptr_t)b;
    j->r2 = k;
}

/* Writes to pipe or stream `a` (job 21). A reader that's gone is EPIPE, as on POSIX. */
static void pipe_write(Job *j) {
    if (is_socket(j->a)) {
        if (sock_write(j->a, j->t, j->tlen) < 0) {
            fail(j);
            return;
        }
        j->r = (int64_t)j->tlen;
        return;
    }
    if (write_handle(handle_of(j->a), j->t, j->tlen) < 0) {
        DWORD e = GetLastError();
        if (e == ERROR_NO_DATA || e == ERROR_BROKEN_PIPE) fail_with(j, EPIPE);
        else win_fail(j, NULL);
        return;
    }
    j->r = (int64_t)j->tlen;
}

/* Reads pipe `a` to its end as text (job 22). */
static void pipe_read_all(Job *j) {
    size_t cap = 4096, n = 0;
    char *buf = malloc(cap);
    if (!buf) abort();
    HANDLE h = handle_of(j->a);
    int sock = is_socket(j->a);
    for (;;) {
        if (n == cap) {
            cap *= 2;
            buf = realloc(buf, cap);
            if (!buf) abort();
        }
        DWORD k = 0;
        DWORD want = cap - n > 0x40000000 ? 0x40000000 : (DWORD)(cap - n);
        if (sock) {
            ptrdiff_t got = sock_read(j->a, buf + n, want);
            if (got < 0) {
                free(buf);
                fail(j);
                return;
            }
            if (got == 0) break;
            n += (size_t)got;
            continue;
        }
        if (!ReadFile(h, buf + n, want, &k, NULL)) {
            DWORD e = GetLastError();
            if (e == ERROR_BROKEN_PIPE || e == ERROR_HANDLE_EOF) break;
            free(buf);
            win_errno(e, NULL);
            fail(j);
            return;
        }
        if (k == 0) break;
        n += k;
    }
    if (!valid_utf8((unsigned char *)buf, n)) {
        free(buf);
        j->r = -K_INVALID_DATA;
        return;
    }
    j->text = buf;
    j->text_len = n;
}

/* The control operations of `std/process` (110–119). */
static int64_t process_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PicoStr *s,
                          const PicoStr *t) {
    switch (op) {
    case 110: {
        Job *j = result_job(q);
        size_t tlen;
        char *prog = cstr(s, NULL), *fields = cstr(t, &tlen);
        spawn_child(j, a, b, c, prog, fields, tlen);
        free(prog);
        free(fields);
        return j->id;
    }
    case 111: { /* kill child a with signal b */
        pthread_mutex_lock(&child_lock);
        int i = live_find((DWORD)a);
        if (i >= 0 && !live[i].signal && TerminateProcess(live[i].h, (UINT)(128 + b)))
            live[i].signal = (int)b;
        pthread_mutex_unlock(&child_lock);
        return 0;
    }
    case 112: /* close pipe end a */
        return CloseHandle((HANDLE)(intptr_t)a) ? 0 : -1;
    case 113: { /* the arguments, each followed by a NUL */
        Job *j = result_job(q);
        text_add(j, "", 0);
        int n = 0;
        wchar_t **argv = CommandLineToArgvW(GetCommandLineW(), &n);
        for (int i = 1; argv && i < n; i++) {
            char *u = narrow(argv[i]);
            text_add(j, u, strlen(u) + 1);
            free(u);
        }
        if (argv) LocalFree(argv);
        return j->id;
    }
    case 114: { /* getenv(s): r = 1 when set, text = its value */
        Job *j = result_job(q);
        char *name = cstr(s, NULL);
        wchar_t *w = wide(name);
        free(name);
        SetLastError(0);
        DWORD n = GetEnvironmentVariableW(w, NULL, 0);
        if (n == 0) {
            j->r = GetLastError() != ERROR_ENVVAR_NOT_FOUND;
        } else {
            wchar_t *v = malloc((size_t)n * sizeof *v);
            if (!v) abort();
            DWORD got = GetEnvironmentVariableW(w, v, n);
            if (got > 0 && got < n) {
                char *u = narrow(v);
                text_add(j, u, strlen(u));
                free(u);
            }
            free(v);
            j->r = 1;
        }
        free(w);
        return j->id;
    }
    case 115:   /* setenv(s, t) */
    case 116: { /* unsetenv(s) */
        char *name = cstr(s, NULL), *value = op == 115 ? cstr(t, NULL) : NULL;
        wchar_t *wn = wide(name), *wv = value ? wide(value) : NULL;
        BOOL ok = SetEnvironmentVariableW(wn, wv);
        free(name);
        free(value);
        free(wn);
        free(wv);
        return ok || op == 116 ? 0 : -1;
    }
    case 117: { /* every variable: "NAME=value\0" each */
        Job *j = result_job(q);
        text_add(j, "", 0);
        wchar_t *env = GetEnvironmentStringsW();
        for (const wchar_t *e = env; e && *e; e += wcslen(e) + 1) {
            if (*e == L'=') continue; /* (cmd's per-drive directories) */
            char *u = narrow(e);
            text_add(j, u, strlen(u) + 1);
            free(u);
        }
        if (env) FreeEnvironmentStringsW(env);
        return j->id;
    }
    case 118: /* exit(a) */
        fflush(stdout);
        fflush(stderr);
        exit((int)a);
    case 119: return (int64_t)GetCurrentProcessId();
    default: return -1;
    }
}

static void run(Job *j) {
    j->r = 0;
    switch (j->op) {
    case 1: { /* readText */
        size_t n;
        char *b = read_all(j->s, &n);
        if (!b) {
            fail(j);
            return;
        }
        if (!valid_utf8((unsigned char *)b, n)) {
            free(b);
            j->r = -K_INVALID_DATA;
            return;
        }
        j->text = b;
        j->text_len = n;
        return;
    }
    case 2: { /* readBytes: a buffer handle */
        size_t n;
        char *b = read_all(j->s, &n);
        if (!b) {
            fail(j);
            return;
        }
        j->r = (int64_t)(uintptr_t)b;
        j->r2 = (int64_t)n;
        return;
    }
    case 3:   /* writeText (t), a: append */
    case 4: { /* writeBytes (t holds the bytes) */
        DWORD a = attributes(j->s);
        if (a != INVALID_FILE_ATTRIBUTES && (a & FILE_ATTRIBUTE_DIRECTORY)) {
            fail_with(j, EISDIR);
            return;
        }
        wchar_t *w = wide(j->s);
        HANDLE h = CreateFileW(w, j->a ? FILE_APPEND_DATA : GENERIC_WRITE,
                               FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL,
                               j->a ? OPEN_ALWAYS : CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
        free(w);
        if (h == INVALID_HANDLE_VALUE) {
            win_fail(j, j->s);
            return;
        }
        if (write_handle(h, j->t, j->tlen) < 0) win_fail(j, j->s);
        CloseHandle(h);
        return;
    }
    case 5: { /* stat, a: don't follow links */
        Info in;
        if (fs_info(j->s, !j->a, &in) < 0) {
            fail(j);
            return;
        }
        char line[96];
        int n = snprintf(line, sizeof line, "%c %lld %lld", in.kind, (long long)in.size,
                         (long long)(in.mtime_ns / 1000000));
        text_add(j, line, (size_t)n);
        return;
    }
    case 6: /* mkdir, a: recursive */
        if ((j->a ? mkdir_all(j->s) : make_dir(j->s)) < 0) fail(j);
        return;
    case 7: { /* remove, a: recursive */
        Info in;
        if (fs_info(j->s, 0, &in) < 0 || (j->a ? remove_all(j->s) : remove_one(j->s, &in)) < 0)
            fail(j);
        return;
    }
    case 8: { /* rename */
        wchar_t *from = wide(j->s), *to = wide(j->t);
        BOOL ok = MoveFileExW(from, to, MOVEFILE_REPLACE_EXISTING);
        free(from);
        free(to);
        if (!ok) win_fail(j, j->s);
        return;
    }
    case 37: { /* copy file s to t (replacing it) (#64) */
        wchar_t *from = wide(j->s), *to = wide(j->t);
        BOOL ok = CopyFileW(from, to, FALSE);
        free(from);
        free(to);
        if (!ok) win_fail(j, j->s);
        return;
    }
    case 38: { /* symlink at s pointing to t (#64): needs Developer Mode or the privilege */
        wchar_t *link = wide(j->s), *target = wide(j->t);
        /* A directory link when the target (relative to the link's directory) is one. */
        DWORD flags = 0x2; /* SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE */
        DWORD attrs = GetFileAttributesW(target);
        if (attrs != INVALID_FILE_ATTRIBUTES && (attrs & FILE_ATTRIBUTE_DIRECTORY)) flags |= 0x1;
        BOOL ok = CreateSymbolicLinkW(link, target, flags);
        free(link);
        free(target);
        if (!ok) win_fail(j, j->s);
        return;
    }
    case 39: /* readlink: not on Windows yet */
        errno = ENOSYS;
        fail(j);
        return;
    case 40: { /* chmod(s, a): Windows has only the read-only attribute (no owner write bit) */
        wchar_t *path = wide(j->s);
        DWORD attrs = GetFileAttributesW(path);
        BOOL ok = attrs != INVALID_FILE_ATTRIBUTES;
        if (ok) {
            attrs = (j->a & 0200) ? (attrs & ~FILE_ATTRIBUTE_READONLY) : (attrs | FILE_ATTRIBUTE_READONLY);
            ok = SetFileAttributesW(path, attrs);
        }
        free(path);
        if (!ok) win_fail(j, j->s);
        return;
    }
    case 9: { /* readDir: "<kind> <name>\0" per entry, in name order */
        DirIter *d = dir_open(j->s);
        if (!d) {
            fail(j);
            return;
        }
        char **names = NULL;
        size_t count = 0, cap = 0;
        const char *name;
        while ((name = dir_next(d))) {
            DWORD a = d->data.dwFileAttributes;
            char k = (a & FILE_ATTRIBUTE_REPARSE_POINT) ? 'l'
                     : (a & FILE_ATTRIBUTE_DIRECTORY)   ? 'd'
                                                        : 'f';
            size_t len = strlen(name) + 3;
            char *entry = malloc(len);
            if (!entry) abort();
            snprintf(entry, len, "%c %s", k, name);
            if (count == cap) {
                cap = cap ? cap * 2 : 16;
                names = realloc(names, cap * sizeof *names);
                if (!names) abort();
            }
            names[count++] = entry;
        }
        dir_close(d);
        /* Sorted by name (byte order), as on the other platforms. */
        for (size_t i = 1; i < count; i++) {
            for (size_t k = i; k > 0 && strcmp(names[k - 1] + 2, names[k] + 2) > 0; k--) {
                char *x = names[k];
                names[k] = names[k - 1];
                names[k - 1] = x;
            }
        }
        text_add(j, "", 0);
        for (size_t i = 0; i < count; i++) {
            text_add(j, names[i], strlen(names[i]) + 1);
            free(names[i]);
        }
        free(names);
        return;
    }
    case 10: { /* open: a = flags (1 read, 2 write, 4 append, 8 create, 16 truncate, 32 new);
                * r = the handle */
        int64_t f = j->a;
        DWORD a = attributes(j->s);
        if (a != INVALID_FILE_ATTRIBUTES && (a & FILE_ATTRIBUTE_DIRECTORY)) {
            fail_with(j, EISDIR);
            return;
        }
        DWORD access = (f & 1) ? GENERIC_READ : 0;
        if (f & 4) access |= FILE_APPEND_DATA;
        else if (f & 2) access |= GENERIC_WRITE;
        DWORD disposition = (f & 32)              ? CREATE_NEW
                            : (f & 8) && (f & 16) ? CREATE_ALWAYS
                            : (f & 8)             ? OPEN_ALWAYS
                            : (f & 16)            ? TRUNCATE_EXISTING
                                                  : OPEN_EXISTING;
        wchar_t *w = wide(j->s);
        HANDLE h = CreateFileW(w, access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                               NULL, disposition, FILE_ATTRIBUTE_NORMAL, NULL);
        free(w);
        if (h == INVALID_HANDLE_VALUE) {
            win_fail(j, j->s);
            return;
        }
        j->r = (int64_t)(uintptr_t)h;
        return;
    }
    case 11: { /* read: a = handle, b = at most this many bytes; r = buffer, r2 = count */
        size_t n = j->b > 0 ? (size_t)j->b : 1;
        char *b = calloc(n, 1);
        if (!b) abort();
        DWORD k = 0;
        if (!ReadFile((HANDLE)(intptr_t)j->a, b, n > 0x40000000 ? 0x40000000 : (DWORD)n, &k,
                      NULL)) {
            free(b);
            win_fail(j, NULL);
            return;
        }
        j->r = (int64_t)(uintptr_t)b;
        j->r2 = k;
        return;
    }
    case 12: /* write: a = handle, t = bytes */
        if (write_handle((HANDLE)(intptr_t)j->a, j->t, j->tlen) < 0) {
            win_fail(j, NULL);
            return;
        }
        j->r = (int64_t)j->tlen;
        return;
    case 13: /* close */
        if (!CloseHandle((HANDLE)(intptr_t)j->a)) win_fail(j, NULL);
        return;
    case 14: { /* seek: a = handle, b = position from the start */
        LARGE_INTEGER to, at;
        to.QuadPart = j->b;
        if (!SetFilePointerEx((HANDLE)(intptr_t)j->a, to, &at, FILE_BEGIN)) {
            win_fail(j, NULL);
            return;
        }
        j->r = (int64_t)at.QuadPart;
        return;
    }
    case 15: /* watch: s = path, a = recursive; r = the watcher's id */
        watch_open(j);
        return;
    case 20: pipe_read(j); return;
    case 21: pipe_write(j); return;
    case 22: pipe_read_all(j); return;
    case 23: child_wait(j); return;
    case 30:
    case 31:
    case 32:
    case 33:
    case 34:
    case 35: net_run(j); return;
    case 36: { /* TLS handshake on socket a: b = mode (pico_tls_start), s = host, t = PEM
                * text, then NUL and a second PEM text (a server's key); text = the error */
        size_t n1 = j->t ? strlen(j->t) : 0;
        const char *pem2 = j->t && n1 < j->tlen ? j->t + n1 + 1 : "";
        size_t n2 = j->t && n1 < j->tlen ? j->tlen - n1 - 1 : 0;
        char err[512];
        if (pico_tls_start((int)j->a, (int)j->b, j->s ? j->s : "", j->t ? j->t : "", n1, pem2,
                          n2, err, sizeof err) < 0) {
            j->r = -15;
            text_add(j, err, strlen(err));
        }
        return;
    }
    default:
        fail_with(j, ENOSYS);
        return;
    }
}

static void os_submit(Job *j) {
    if (j->op == 16) {
        watch_next(j);
        return;
    }
    if ((j->op >= 20 && j->op <= 23) || (j->op >= 30 && j->op <= 36)) {
        run_solo(j);
        return;
    }
    start_pool();
    pthread_mutex_lock(&pool_lock);
    if (work_tail) work_tail->next = j;
    else work_head = j;
    work_tail = j;
    pthread_cond_signal(&pool_cond);
    pthread_mutex_unlock(&pool_lock);
}

static int64_t os_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PicoStr *s,
                     const PicoStr *t) {
    switch (op) {
    case 107: /* cancel read job a (job 20; `Reader.close()`, #100) */
        cancel_read(q, a);
        return 0;
    case 106: /* close watcher a */
        watch_close(a);
        return 0;
    case 105: { /* the working directory, as a polled job's text */
        Job *j = result_job(q);
        DWORD n = GetCurrentDirectoryW(0, NULL);
        wchar_t *w = malloc((size_t)(n ? n : 1) * sizeof *w);
        if (!w) abort();
        if (n && GetCurrentDirectoryW(n, w)) {
            char *u = narrow(w);
            text_add(j, u, strlen(u));
            free(u);
        }
        free(w);
        return j->id;
    }
    case 134: { /* read file s whole, now: r = a buffer handle (or -1), r2 = its size */
        Job *j = result_job(q);
        char *path = cstr(s, NULL);
        size_t n = 0;
        char *m = read_all(path, &n);
        free(path);
        j->r = m ? (int64_t)(uintptr_t)m : -1;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 135: /* readlink(s): no links yet ("") */
        return result_job(q)->id;
    case 120: { /* shutdown socket a: b = 0 reading, 1 writing, 2 both (TLS: close_notify first).
                 * A blocked read doesn't notice a shutdown on Windows: it's cancelled too. */
        if (b != 0) pico_tls_close_notify((int)a);
        int r = shutdown((SOCKET)a, b == 0 ? SD_RECEIVE : b == 1 ? SD_SEND : SD_BOTH);
        if (b != 1) CancelIoEx((HANDLE)(intptr_t)a, NULL);
        return r == 0 ? 0 : -1;
    }
    case 121: /* close socket a (and its TLS session) */
        pico_tls_free((int)a);
        return closesocket((SOCKET)a) == 0 ? 0 : -1;
    case 110:
    case 111:
    case 112:
    case 113:
    case 114:
    case 115:
    case 116:
    case 117:
    case 118:
    case 119: return process_op(q, op, a, b, c, s, t);
    default: return -1;
    }
}
#else
#error "pico_io.c: no I/O for this platform"
#endif

#if !defined(__wasi__) /* (sockets on WASI: P8.5c) */
/* ---- `std/net` (spec §13.4) ----
 * Every operation that may block (accept, connect, receiving, sending, resolving) is a job on
 * a thread of its own. Reading and writing a TCP stream are the pipe jobs (20–22). Closing
 * while a job waits is arranged by the library: it shuts the socket down (or wakes the job
 * with a connection or a datagram to itself) and closes the descriptor once the job is done,
 * so a descriptor is never reused under a waiting job. The platform parts are the shims
 * (`net_init`, `sock_failed`, `sock_close`, `sock_private`, `sock_udp`, `sock_t`). */

/* "host port" of an address. */
static void addr_text(Job *j, const struct sockaddr *sa) {
    char host[INET6_ADDRSTRLEN] = "?";
    int port = 0;
    if (sa->sa_family == AF_INET) {
        const struct sockaddr_in *in = (const struct sockaddr_in *)sa;
        inet_ntop(AF_INET, &in->sin_addr, host, sizeof host);
        port = ntohs(in->sin_port);
    } else if (sa->sa_family == AF_INET6) {
        const struct sockaddr_in6 *in = (const struct sockaddr_in6 *)sa;
        inet_ntop(AF_INET6, &in->sin6_addr, host, sizeof host);
        port = ntohs(in->sin6_port);
    }
    char line[INET6_ADDRSTRLEN + 16];
    int n = snprintf(line, sizeof line, "%s %d", host, port);
    text_add(j, line, (size_t)n);
}

/* "local-host local-port remote-host remote-port" of a connected socket. */
static void ends_text(Job *j, int64_t fd) {
    struct sockaddr_storage a;
    socklen_t n = sizeof a;
    memset(&a, 0, sizeof a);
    getsockname((sock_t)fd, (struct sockaddr *)&a, &n);
    addr_text(j, (struct sockaddr *)&a);
    text_add(j, " ", 1);
    n = sizeof a;
    memset(&a, 0, sizeof a);
    getpeername((sock_t)fd, (struct sockaddr *)&a, &n);
    addr_text(j, (struct sockaddr *)&a);
}

static struct addrinfo *lookup(Job *j, const char *host, int port, int udp, int passive) {
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = udp ? SOCK_DGRAM : SOCK_STREAM;
    hints.ai_flags = passive ? AI_PASSIVE : 0;
    char serv[16];
    snprintf(serv, sizeof serv, "%d", port);
    int e = getaddrinfo(host, serv, &hints, &res);
    if (e != 0) {
        j->r = -K_HOST_NOT_FOUND;
        j->r2 = e;
        return NULL;
    }
    return res;
}

static void net_run(Job *j) {
    net_init();
    switch (j->op) {
    case 30: { /* listen (b = 0) or bind a UDP socket (b = 1): s = host, a = port */
        struct addrinfo *res = lookup(j, j->s, (int)j->a, (int)j->b, 1);
        if (!res) return;
        int64_t fd = -1;
        int e = 0;
        for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
            fd = (int64_t)socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
            if (fd == SOCK_NONE) {
                sock_failed();
                e = errno;
                fd = -1;
                continue;
            }
#if !defined(_WIN32)
            /* (On Windows that would let others take the port; it isn't needed there.) */
            int one = 1;
            if (!j->b) setsockopt((int)fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
#endif
            if (bind((sock_t)fd, ai->ai_addr, (socklen_t)ai->ai_addrlen) == 0 &&
                (j->b || listen((sock_t)fd, 128) == 0))
                break;
            sock_failed();
            e = errno;
            sock_close(fd);
            fd = -1;
        }
        freeaddrinfo(res);
        if (fd < 0) {
            errno = e;
            fail(j);
            return;
        }
        sock_private(fd);
        if (j->b) sock_udp(fd);
        struct sockaddr_storage a;
        socklen_t n = sizeof a;
        getsockname((sock_t)fd, (struct sockaddr *)&a, &n);
        addr_text(j, (struct sockaddr *)&a);
        j->r = fd;
        return;
    }
    case 31: { /* accept on listener a */
        int64_t fd;
        do {
            fd = (int64_t)accept((sock_t)j->a, NULL, NULL);
            if (fd == SOCK_NONE) sock_failed();
        } while (fd == SOCK_NONE && errno == EINTR);
        if (fd == SOCK_NONE) {
            fail(j);
            return;
        }
        sock_private(fd);
        ends_text(j, fd);
        j->r = fd;
        return;
    }
    case 32: { /* connect to s, port a */
        struct addrinfo *res = lookup(j, j->s, (int)j->a, 0, 0);
        if (!res) return;
        int64_t fd = -1;
        int e = 0;
        for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
            fd = (int64_t)socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
            if (fd == SOCK_NONE) {
                sock_failed();
                e = errno;
                fd = -1;
                continue;
            }
            int r;
            do {
                r = connect((sock_t)fd, ai->ai_addr, (socklen_t)ai->ai_addrlen);
                if (r < 0) sock_failed();
            } while (r < 0 && errno == EINTR);
            if (r == 0) break;
            e = errno;
            sock_close(fd);
            fd = -1;
        }
        freeaddrinfo(res);
        if (fd < 0) {
            errno = e;
            fail(j);
            return;
        }
        sock_private(fd);
        ends_text(j, fd);
        j->r = fd;
        return;
    }
    case 33: { /* receive a datagram on a, at most b bytes: r = handle, r2 = count */
        size_t n = j->b > 0 ? (size_t)j->b : 1;
        char *b = calloc(n, 1);
        if (!b) abort();
        struct sockaddr_storage from;
        memset(&from, 0, sizeof from);
        socklen_t fl = sizeof from;
        ptrdiff_t k;
        do {
            k = recvfrom((sock_t)j->a, b, (int)n, 0, (struct sockaddr *)&from, &fl);
            if (k < 0) sock_failed();
        } while (k < 0 && errno == EINTR);
        if (k < 0) {
            free(b);
            fail(j);
            return;
        }
        addr_text(j, (struct sockaddr *)&from);
        j->r = (int64_t)(uintptr_t)b;
        j->r2 = k;
        return;
    }
    case 34: { /* send t's bytes from socket a to s = "port host" */
        char *sp = strchr(j->s, ' ');
        int port = atoi(j->s);
        struct addrinfo *res = lookup(j, sp ? sp + 1 : "", port, 1, 0);
        if (!res) return;
        /* An address of the socket's family. */
        struct sockaddr_storage mine;
        socklen_t ml = sizeof mine;
        memset(&mine, 0, sizeof mine);
        getsockname((sock_t)j->a, (struct sockaddr *)&mine, &ml);
        struct addrinfo *to = res;
        for (struct addrinfo *ai = res; ai; ai = ai->ai_next)
            if (ai->ai_family == mine.ss_family) {
                to = ai;
                break;
            }
        ptrdiff_t k;
        do {
            k = sendto((sock_t)j->a, j->t, (int)j->tlen, 0, to->ai_addr, (socklen_t)to->ai_addrlen);
            if (k < 0) sock_failed();
        } while (k < 0 && errno == EINTR);
        freeaddrinfo(res);
        if (k < 0) {
            fail(j);
            return;
        }
        j->r = k;
        return;
    }
    case 35: { /* resolve s: "address\0" each, without repeats */
        struct addrinfo *res = lookup(j, j->s, 0, 0, 0);
        if (!res) return;
        for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
            Job one;
            memset(&one, 0, sizeof one);
            addr_text(&one, ai->ai_addr);
            char *sp = one.text ? strrchr(one.text, ' ') : NULL;
            if (sp) *sp = 0;
            size_t len = one.text ? strlen(one.text) : 0;
            int seen = 0;
            for (size_t i = 0; j->text && i < j->text_len; i += strlen(j->text + i) + 1)
                if (strcmp(j->text + i, one.text) == 0) seen = 1;
            if (!seen && len) text_add(j, one.text, len + 1);
            free(one.text);
        }
        freeaddrinfo(res);
        if (!j->text) text_add(j, "", 0);
        return;
    }
    }
}

#endif

/* Fills `p` with `n` bytes from the system's randomness; 0 if there's none. */
static int random_bytes(char *p, size_t n) {
#if defined(_WIN32)
    for (size_t i = 0; i < n; i += 4) {
        unsigned v;
        if (rand_s(&v) != 0) return 0;
        memcpy(p + i, &v, n - i < 4 ? n - i : 4);
    }
    return 1;
#elif defined(__wasi__)
    for (size_t i = 0; i < n; i += 256) {
        if (getentropy(p + i, n - i < 256 ? n - i : 256) != 0) return 0;
    }
    return 1;
#else
    int fd = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
    size_t got = 0;
    while (fd >= 0 && got < n) {
        ssize_t k = read(fd, p + got, n - got);
        if (k <= 0) {
            if (k < 0 && errno == EINTR) continue;
            break;
        }
        got += (size_t)k;
    }
    if (fd >= 0) close(fd);
    return got == n;
#endif
}

#if defined(PICO_BROWSER)
/* What the event loop waits for once it has handed control back to JavaScript (ms; -1:
 * nothing). Read by `pico_browser_pending` (pico_rt.c). */
int64_t pico_browser_wait = -1;
#endif

/* ---------------------------------------------------------------- std/js (P8.8)
 * JavaScript values held by pico code are handles into a table the glue keeps (0 `undefined`,
 * 1 `null`, 2 `globalThis`); these are its imports. Operations that may throw answer -1 and
 * keep the exception for `error`. Outside the browser every op answers -3. Decision 259. */
#if defined(PICO_BROWSER)
#define JS_IMPORT(n) __attribute__((import_module("pico_js"), import_name(#n)))
JS_IMPORT(get) int32_t js_get(int32_t h, const char *s, int32_t n);
JS_IMPORT(set) int32_t js_set(int32_t h, const char *s, int32_t n, int32_t v);
JS_IMPORT(call) int32_t js_call(int32_t h, const char *s, int32_t n, int32_t args);
JS_IMPORT(invoke) int32_t js_invoke(int32_t f, int32_t self, int32_t args);
JS_IMPORT(construct) int32_t js_construct(int32_t f, int32_t args);
JS_IMPORT(string) int32_t js_string(const char *s, int32_t n);
JS_IMPORT(number) int32_t js_number(double d);
JS_IMPORT(boolean) int32_t js_boolean(int32_t b);
JS_IMPORT(array) int32_t js_array(void);
JS_IMPORT(push) void js_push(int32_t a, int32_t v);
JS_IMPORT(type) int32_t js_type(int32_t h);
JS_IMPORT(to_number) double js_to_number(int32_t h);
JS_IMPORT(to_bool) int32_t js_to_bool(int32_t h);
JS_IMPORT(to_string) int32_t js_to_string(int32_t h);
JS_IMPORT(take_string) void js_take_string(char *dst);
JS_IMPORT(release) void js_release(int32_t h);
JS_IMPORT(error) int32_t js_error(void);
JS_IMPORT(func) int32_t js_func(int32_t id);
JS_IMPORT(module) int32_t js_module(const char *s, int32_t n);
JS_IMPORT(equals) int32_t js_equals(int32_t a, int32_t b);

static int64_t js_text_job(Queue *q, int32_t h);

static int64_t js_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PicoStr *s) {
    const char *sp = s ? s->data : "";
    int32_t sn = s ? (int32_t)s->byte_len : 0;
    /* What pico printed comes before what JavaScript code prints. */
    if (op <= 154) fflush(stdout);
    switch (op) {
    case 150: return js_get((int32_t)a, sp, sn);
    case 151: return js_set((int32_t)a, sp, sn, (int32_t)b);
    case 152: return js_call((int32_t)a, sp, sn, (int32_t)b);
    case 153: return js_invoke((int32_t)a, (int32_t)b, (int32_t)c);
    case 154: return js_construct((int32_t)a, (int32_t)b);
    case 155: return js_string(sp, sn);
    case 156: { /* a number from its bits: a the high half, b the low one */
        uint64_t bits = ((uint64_t)(uint32_t)a << 32) | (uint64_t)(uint32_t)b;
        double d;
        memcpy(&d, &bits, sizeof d);
        return js_number(d);
    }
    case 157: return js_boolean((int32_t)a);
    case 158: return js_array();
    case 159: js_push((int32_t)a, (int32_t)b); return 0;
    case 160: return js_type((int32_t)a);
    case 161: { /* a number's bits */
        double d = js_to_number((int32_t)a);
        int64_t bits;
        memcpy(&bits, &d, sizeof bits);
        return bits;
    }
    case 162: return js_to_bool((int32_t)a);
    case 163: return js_text_job(q, (int32_t)a);
    case 164: js_release((int32_t)a); return 0;
    case 165: return js_error();
    case 166: return js_func((int32_t)a);
    case 168: return js_module(sp, sn);
    default: return js_equals((int32_t)a, (int32_t)b); /* 167 */
    }
}
#endif

int64_t pico_io(int64_t op, int64_t a, int64_t b, int64_t c, const PicoStr *s, const PicoStr *t) {
    Queue *q = queue();
    if (op < 100) {
        Job *j = calloc(1, sizeof *j);
        if (!j) abort();
        j->id = q->next_id++;
        j->op = op;
        j->a = a;
        j->b = b;
        j->c = c;
        j->s = cstr(s, NULL);
        if (op == 4 || op == 12 || ((op == 21 || op == 25 || op == 34) && c >= 0)) {
            /* The bytes of buffer `b` (`c` of them), copied now: the buffer may be gone by
             * the time the job runs. */
            j->tlen = c > 0 ? (size_t)c : 0;
            j->t = malloc(j->tlen + 1);
            if (!j->t) abort();
            if (j->tlen) memcpy(j->t, (const char *)(intptr_t)b, j->tlen);
        } else {
            j->t = cstr(t, &j->tlen);
        }
        j->owner = q;
        q->pending++;
        if (op == 25) {
            /* Our own stdout/stderr: written now, in order with `print`, and flushed, so a
             * settled `write` means the OS has the bytes: stdout is a fully buffered FILE
             * when it's a pipe, and a parent waiting for a reply would wait forever (#82).
             * `print` itself stays buffered. */
            FILE *f = j->a == 2 ? stderr : stdout;
            if (j->a == 2) fflush(stdout);
            if (fwrite(j->t, 1, j->tlen, f) < j->tlen || fflush(f) != 0) fail(j);
            else j->r = (int64_t)j->tlen;
            finish(j);
        } else {
            os_submit(j);
        }
        return j->id;
    }
    switch (op) {
    case 100: { /* poll: a finished job's id, or -1 */
        pthread_mutex_lock(&q->lock);
        Job *j = q->head;
        if (j) {
            q->head = j->next;
            if (!q->head) q->tail = NULL;
        }
        pthread_mutex_unlock(&q->lock);
        if (!j) return -1;
        q->pending--;
        j->next = q->taken;
        q->taken = j;
        return j->id;
    }
    case 101: return q->pending;
    case 138: /* the event loop is about to wait a ms: 1 = the host waits instead (the browser:
               * the program returns to JavaScript, which resumes the loop when it's due) */
#if defined(PICO_BROWSER)
        pico_browser_wait = a;
        return 1;
#else
        return 0;
#endif
    case 130: { /* TextEncoder: s's UTF-8 bytes; r = a buffer handle, r2 = their count */
        Job *j = result_job(q);
        size_t n = s ? (size_t)s->byte_len : 0;
        char *m = malloc(n ? n : 1);
        if (!m) abort();
        if (n) memcpy(m, s->data, n);
        j->r = (int64_t)(uintptr_t)m;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 132: { /* a random bytes (from the system): r = a buffer handle, r2 = a */
        Job *j = result_job(q);
        size_t n = a > 0 ? (size_t)a : 0;
        char *m = calloc(n ? n : 1, 1);
        if (!m) abort();
        if (!random_bytes(m, n)) abort(); /* no randomness: nothing safe to fall back on */
        j->r = (int64_t)(uintptr_t)m;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 133: { /* a clock in nanoseconds: a = 0 the wall clock (since 1970), 1 monotonic
                 * (since the program started) */
        extern int64_t pico_start_ns;
        struct timespec ts;
        clock_gettime(a == 1 ? CLOCK_MONOTONIC : CLOCK_REALTIME, &ts);
        int64_t ns = (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
        return a == 1 ? ns - pico_start_ns : ns;
    }
    case 140: { /* crypto operation a on c bytes at b (pico_crypto.h): r = a buffer handle (or
                 * -1), r2 = its length (or the error) */
        Job *j = result_job(q);
        uint8_t *o = NULL;
        size_t len = 0;
        int64_t e = pico_crypto_op(a, (const uint8_t *)(intptr_t)b, c > 0 ? (size_t)c : 0, &o, &len);
        j->r = e ? -1 : (int64_t)(uintptr_t)o;
        j->r2 = e ? e : (int64_t)len;
        return j->id;
    }
    case 136: { /* the bundled tz database's zone s: r = a buffer handle (or -1), r2 = its size */
        extern char *pico_tz_bundled(const char *name, size_t *len);
        Job *j = result_job(q);
        char *name = cstr(s, NULL);
        size_t n = 0;
        char *m = pico_tz_bundled(name, &n);
        free(name);
        j->r = m ? (int64_t)(uintptr_t)m : -1;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 137: { /* the system's local zone as an IANA id, where it isn't TZ or /etc/localtime
                 * (Windows): text ("" when unknown) */
        extern const char *pico_tz_local_id(void);
        Job *j = result_job(q);
        const char *id = pico_tz_local_id();
        text_add(j, id ? id : "", id ? strlen(id) : 0);
        return j->id;
    }
    case 150: case 151: case 152: case 153: case 154: case 155: case 156: case 157:
    case 158: case 159: case 160: case 161: case 162: case 163: case 164: case 165:
    case 166: case 167: case 168:
#if defined(PICO_BROWSER)
        return js_op(q, op, a, b, c, s);
#else
        return -3;
#endif
    case 131: /* TextDecoder: c bytes at b as text; a = 1 rejects bad bytes (r = -InvalidData) */
        return decode_utf8(q, (const unsigned char *)(intptr_t)b, c > 0 ? (size_t)c : 0, a == 1);
    case 102: {
        Job *j = taken(a);
        return j ? j->r : 0;
    }
    case 103: {
        Job *j = taken(a);
        return j ? j->r2 : 0;
    }
    case 104: { /* free */
        Job **p = &q->taken;
        while (*p && (*p)->id != a) p = &(*p)->next;
        Job *j = *p;
        if (!j) return 0;
        *p = j->next;
        free(j->s);
        free(j->t);
        free(j->text);
        free(j);
        return 0;
    }
    default: return os_op(q, op, a, b, c, s, t);
    }
}

#if defined(PICO_BROWSER)
/* `String(value)` as a job's text. */
static int64_t js_text_job(Queue *q, int32_t h) {
    Job *j = result_job(q);
    int32_t n = js_to_string(h);
    char *buf = malloc(n > 0 ? (size_t)n : 1);
    if (!buf) abort();
    js_take_string(buf);
    text_add(j, buf, (size_t)n);
    free(buf);
    return j->id;
}
#endif

PicoStr *pico_io_text(int64_t id) {
    Job *j = taken(id);
    if (!j || !j->text) return pico_str_new("", 0);
    return pico_str_new(j->text, (int64_t)j->text_len);
}
