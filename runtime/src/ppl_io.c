/* File-system I/O for `std/fs` (spec §13.4): jobs run on a small thread pool; their results
 * go back to the thread that submitted them (its event loop polls them, and is woken when one
 * is done). The ppl side is `__io(op, a, b, c, s, t)` and `__io_text(job)` (see
 * `stdlib/fs.ppl` and the core event loop).
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
#include "ppl_rt.h"
#include "ppl_crypto.h"

#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#if defined(__APPLE__) || defined(__linux__)
#define PPL_IO_POSIX 1
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
#include <sys/random.h> /* getentropy */
#endif

void *ppl_loop_waker(void);
void ppl_loop_wake(void *waker);

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
        my_queue->waker = ppl_loop_waker();
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
    ppl_loop_wake(w);
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

static char *cstr(const PplStr *s, size_t *len) {
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

/* ---- the platform's part (below) ---- */

#if defined(_WIN32)
#define PPL_SEP '\\'
#else
#define PPL_SEP '/'
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
    if (*rel) snprintf(m, n, "%s%c%s", root, PPL_SEP, rel);
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

#if PPL_IO_POSIX
/* ---- the POSIX operations (files, processes, sockets) ---- */


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
    ptrdiff_t t = ppl_tls_write(fd, p, n);
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
static void child_wait(Job *j);
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
        j->r = (int64_t)(intptr_t)b;
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
    case 20: { /* (the same from a pipe) */
        size_t n = j->b > 0 ? (size_t)j->b : 1;
        char *b = calloc(n, 1);
        if (!b) abort();
        ssize_t k = ppl_tls_read((int)j->a, b, n);
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
        j->r = (int64_t)(intptr_t)b;
        j->r2 = k;
        return;
    }
    case 12: /* write: a = fd, t = bytes */
    case 21: /* (the same to a pipe) */
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
        watch_open(j);
        return;
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
            ssize_t k = ppl_tls_read((int)j->a, buf + n, cap - n);
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
    case 36: { /* TLS handshake on socket a: b = mode (ppl_tls_start), s = host, t = PEM
                * text, then NUL and a second PEM text (a server's key); text = the error */
        size_t n1 = j->t ? strlen(j->t) : 0;
        const char *pem2 = j->t && n1 < j->tlen ? j->t + n1 + 1 : "";
        size_t n2 = j->t && n1 < j->tlen ? j->tlen - n1 - 1 : 0;
        char err[512];
        if (ppl_tls_start((int)j->a, (int)j->b, j->s ? j->s : "", j->t ? j->t : "", n1, pem2,
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

/* ---- `std/process` (spec §13.3) ---- */

static char *cstr(const PplStr *s, size_t *len);

extern int ppl_argc;
extern char **ppl_argv;
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
    if (!err) {
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

static int64_t process_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PplStr *s,
                          const PplStr *t) {
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
        for (int i = 1; i < ppl_argc; i++) text_add(j, ppl_argv[i], strlen(ppl_argv[i]) + 1);
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

/* Starts job `j` (op < 100), which reports to its queue when it's done. */
static void os_submit(Job *j) {
    int64_t op = j->op;
    if (op == 16) {
        watch_next(j);
        return;
    }
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
static int64_t os_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PplStr *s,
                     const PplStr *t) {
    switch (op) {
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
        j->r = (int64_t)(intptr_t)m;
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
    case 120: /* shutdown socket a: b = 0 reading, 1 writing, 2 both (TLS: close_notify first) */
        if (b != 0) ppl_tls_close_notify((int)a);
        return shutdown((int)a, b == 0 ? SHUT_RD : b == 1 ? SHUT_WR : SHUT_RDWR) < 0 ? -1 : 0;
    case 121: /* close socket a (and its TLS session) */
        ppl_tls_free((int)a);
        return close((int)a) < 0 ? -1 : 0;
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
/* ---- Windows: files and `watch` (P8.3b) over the W functions, paths UTF-8 on the ppl
 * side; processes and sockets come with P8.3c. Errors become the POSIX errno values the
 * kinds are mapped from (`win_errno`), so messages match the other platforms. */
#include <windows.h>
#include <shellapi.h>

static char *cstr(const PplStr *s, size_t *len);

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
    ptrdiff_t k = ppl_tls_read((int)a, b, n);
    if (k != -2) return k;
    int got = recv((SOCKET)a, b, n > 0x40000000 ? 0x40000000 : (int)n, 0);
    if (got >= 0) return got;
    int e = WSAGetLastError();
    if (read_ended(e)) return 0;
    errno = wsa_errno(e);
    return -1;
}

static int sock_write(int64_t a, const char *p, size_t n) {
    ptrdiff_t t = ppl_tls_write((int)a, p, n);
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
 * Pipe ends are handles (passed to ppl as integers in place of POSIX fds; 0, 1 and 2 still
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
        j->r = (int64_t)(intptr_t)b;
        j->r2 = got;
        return;
    }
    DWORD k = 0;
    if (!ReadFile(handle_of(j->a), b, n > 0x40000000 ? 0x40000000 : (DWORD)n, &k, NULL)) {
        DWORD e = GetLastError();
        if (e != ERROR_BROKEN_PIPE && e != ERROR_HANDLE_EOF) {
            free(b);
            win_errno(e, NULL);
            fail(j);
            return;
        }
        k = 0;
    }
    j->r = (int64_t)(intptr_t)b;
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
static int64_t process_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PplStr *s,
                          const PplStr *t) {
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
        j->r = (int64_t)(intptr_t)b;
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
        j->r = (int64_t)(intptr_t)h;
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
        j->r = (int64_t)(intptr_t)b;
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
    case 36: { /* TLS handshake on socket a: b = mode (ppl_tls_start), s = host, t = PEM
                * text, then NUL and a second PEM text (a server's key); text = the error */
        size_t n1 = j->t ? strlen(j->t) : 0;
        const char *pem2 = j->t && n1 < j->tlen ? j->t + n1 + 1 : "";
        size_t n2 = j->t && n1 < j->tlen ? j->tlen - n1 - 1 : 0;
        char err[512];
        if (ppl_tls_start((int)j->a, (int)j->b, j->s ? j->s : "", j->t ? j->t : "", n1, pem2,
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

static int64_t os_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PplStr *s,
                     const PplStr *t) {
    switch (op) {
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
        j->r = m ? (int64_t)(intptr_t)m : -1;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 135: /* readlink(s): no links yet ("") */
        return result_job(q)->id;
    case 120: { /* shutdown socket a: b = 0 reading, 1 writing, 2 both (TLS: close_notify first).
                 * A blocked read doesn't notice a shutdown on Windows: it's cancelled too. */
        if (b != 0) ppl_tls_close_notify((int)a);
        int r = shutdown((SOCKET)a, b == 0 ? SD_RECEIVE : b == 1 ? SD_SEND : SD_BOTH);
        if (b != 1) CancelIoEx((HANDLE)(intptr_t)a, NULL);
        return r == 0 ? 0 : -1;
    }
    case 121: /* close socket a (and its TLS session) */
        ppl_tls_free((int)a);
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
#elif defined(__wasi__)
/* ---- wasm32-wasi: no I/O yet (P8.5b: files, clocks and the loop; P8.5c: sockets). Jobs end
 * at once with ENOSYS. ---- */

static void fail_with(Job *j, int en) {
    errno = en;
    fail(j);
}

static int fs_info(const char *path, int follow, Info *out) {
    (void)path, (void)follow, (void)out;
    errno = ENOSYS;
    return -1;
}

struct DirIter {
    int unused;
};

static DirIter *dir_open(const char *path) {
    (void)path;
    errno = ENOSYS;
    return NULL;
}

static const char *dir_next(DirIter *d) {
    (void)d;
    return NULL;
}

static void dir_close(DirIter *d) { (void)d; }

static void wake_open(Watcher *w) { (void)w; }
static void wake_signal(Watcher *w) { (void)w; }
static void wake_close(Watcher *w) { (void)w; }
static void wait_os(Watcher *w, int ms) { (void)w, (void)ms; }

static void run(Job *j) { fail_with(j, ENOSYS); }

static void net_run(Job *j) { fail_with(j, ENOSYS); }

static void os_submit(Job *j) {
    fail_with(j, ENOSYS);
    finish(j);
}

extern int ppl_argc;
extern char **ppl_argv;
extern char **environ;

/* This process (spec §13.3's availability table: args, env and exit work on WASI). */
static int64_t os_op(Queue *q, int64_t op, int64_t a, int64_t b, int64_t c, const PplStr *s,
                     const PplStr *t) {
    (void)b, (void)c;
    switch (op) {
    case 113: { /* the arguments, each followed by a NUL */
        Job *j = result_job(q);
        text_add(j, "", 0);
        for (int i = 1; i < ppl_argc; i++) text_add(j, ppl_argv[i], strlen(ppl_argv[i]) + 1);
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
        fflush(stdout);
        fflush(stderr);
        exit((int)a);
    case 119: return 1; /* (WASI has no process ids) */
    default: return -1;
    }
}
#else
#error "ppl_io.c: no I/O for this platform"
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
        j->r = (int64_t)(intptr_t)b;
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

int64_t ppl_io(int64_t op, int64_t a, int64_t b, int64_t c, const PplStr *s, const PplStr *t) {
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
            /* Our own stdout/stderr: written now, in order with `print`. */
            FILE *f = j->a == 2 ? stderr : stdout;
            if (j->a == 2) fflush(stdout);
            if (fwrite(j->t, 1, j->tlen, f) < j->tlen) fail(j);
            else j->r = (int64_t)j->tlen;
            if (j->a == 2) fflush(stderr);
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
    case 130: { /* TextEncoder: s's UTF-8 bytes; r = a buffer handle, r2 = their count */
        Job *j = result_job(q);
        size_t n = s ? (size_t)s->byte_len : 0;
        char *m = malloc(n ? n : 1);
        if (!m) abort();
        if (n) memcpy(m, s->data, n);
        j->r = (int64_t)(intptr_t)m;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 132: { /* a random bytes (from the system): r = a buffer handle, r2 = a */
        Job *j = result_job(q);
        size_t n = a > 0 ? (size_t)a : 0;
        char *m = calloc(n ? n : 1, 1);
        if (!m) abort();
        if (!random_bytes(m, n)) abort(); /* no randomness: nothing safe to fall back on */
        j->r = (int64_t)(intptr_t)m;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 133: { /* a clock in nanoseconds: a = 0 the wall clock (since 1970), 1 monotonic
                 * (since the program started) */
        extern int64_t ppl_start_ns;
        struct timespec ts;
        clock_gettime(a == 1 ? CLOCK_MONOTONIC : CLOCK_REALTIME, &ts);
        int64_t ns = (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
        return a == 1 ? ns - ppl_start_ns : ns;
    }
    case 140: { /* crypto operation a on c bytes at b (ppl_crypto.h): r = a buffer handle (or
                 * -1), r2 = its length (or the error) */
        Job *j = result_job(q);
        uint8_t *o = NULL;
        size_t len = 0;
        int64_t e = ppl_crypto_op(a, (const uint8_t *)(intptr_t)b, c > 0 ? (size_t)c : 0, &o, &len);
        j->r = e ? -1 : (int64_t)(intptr_t)o;
        j->r2 = e ? e : (int64_t)len;
        return j->id;
    }
    case 136: { /* the bundled tz database's zone s: r = a buffer handle (or -1), r2 = its size */
        extern char *ppl_tz_bundled(const char *name, size_t *len);
        Job *j = result_job(q);
        char *name = cstr(s, NULL);
        size_t n = 0;
        char *m = ppl_tz_bundled(name, &n);
        free(name);
        j->r = m ? (int64_t)(intptr_t)m : -1;
        j->r2 = (int64_t)n;
        return j->id;
    }
    case 137: { /* the system's local zone as an IANA id, where it isn't TZ or /etc/localtime
                 * (Windows): text ("" when unknown) */
        extern const char *ppl_tz_local_id(void);
        Job *j = result_job(q);
        const char *id = ppl_tz_local_id();
        text_add(j, id ? id : "", id ? strlen(id) : 0);
        return j->id;
    }
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

PplStr *ppl_io_text(int64_t id) {
    Job *j = taken(id);
    if (!j || !j->text) return ppl_str_new("", 0);
    return ppl_str_new(j->text, (int64_t)j->text_len);
}
