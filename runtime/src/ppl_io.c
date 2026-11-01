/* File-system I/O for `std/fs` (spec §13.4): jobs run on a small thread pool; their results
 * go back to the thread that submitted them (its event loop polls them, and is woken when one
 * is done). The ppl side is `__io(op, a, b, c, s, t)` and `__io_text(job)` (see
 * `stdlib/fs.ppl` and the core event loop).
 *
 * Errors are portable kinds (the job's `r` is -kind, `r2` the errno), so messages are the same
 * in the interpreter, which runs the same operations with Rust's std::fs. */
#define _GNU_SOURCE
#include "ppl_rt.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(__APPLE__) || defined(__linux__)
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

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

/* The pool. */
static pthread_mutex_t pool_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t pool_cond = PTHREAD_COND_INITIALIZER;
static Job *work_head, *work_tail;
static int pool_started;

static int kind_of(int e) {
    switch (e) {
    case ENOENT: return K_NOT_FOUND;
    case EEXIST: return K_ALREADY_EXISTS;
    case EACCES:
    case EPERM: return K_PERMISSION_DENIED;
    case ENOTDIR: return K_NOT_A_DIRECTORY;
    case EISDIR: return K_IS_A_DIRECTORY;
    case ENOTEMPTY: return K_DIRECTORY_NOT_EMPTY;
    default: return K_OTHER;
    }
}

static void fail(Job *j) {
    int e = errno;
    j->r = -kind_of(e);
    j->r2 = e;
}

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
    case 11: { /* read: a = fd, b = at most this many bytes; r = handle, r2 = count */
        size_t n = j->b > 0 ? (size_t)j->b : 1;
        char *b = calloc(n, 1);
        if (!b) abort();
        ssize_t k;
        do k = read((int)j->a, b, n);
        while (k < 0 && errno == EINTR);
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
    default: j->r = -K_OTHER; j->r2 = EINVAL; return;
    }
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

/* `watch` (spec §13.4): a watcher keeps a snapshot of the tree (path, kind, size, mtime,
 * inode per entry, sorted by path). While a "next" job (op 16) waits on it, its thread
 * compares a fresh scan with the snapshot and delivers the differences as events
 * ("c|m|r <relative path>\0" each, in path order). kqueue (macOS) or inotify (Linux) only
 * wake the thread up; a timeout catches whatever they miss. Between two `next` jobs changes
 * aren't looked at, so they arrive as one difference (the interpreter does the same). */
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
    int wake[2]; /* a pipe: written on close, to end the OS wait */
    struct Watcher *next;
} Watcher;

static pthread_mutex_t watch_lock = PTHREAD_MUTEX_INITIALIZER;
static Watcher *watchers;
static int64_t next_watcher = 1;

static void snap_free(Snap *s) {
    for (size_t i = 0; i < s->n; i++) free(s->e[i].rel);
    free(s->e);
    s->e = NULL;
    s->n = s->cap = 0;
}

static int64_t mtime_ns(const struct stat *st) {
#if defined(__APPLE__)
    return (int64_t)st->st_mtimespec.tv_sec * 1000000000 + st->st_mtimespec.tv_nsec;
#else
    return (int64_t)st->st_mtim.tv_sec * 1000000000 + st->st_mtim.tv_nsec;
#endif
}

static void snap_add(Snap *s, const char *rel, const struct stat *st) {
    if (s->n == s->cap) {
        s->cap = s->cap ? s->cap * 2 : 32;
        s->e = realloc(s->e, s->cap * sizeof *s->e);
        if (!s->e) abort();
    }
    Entry *e = &s->e[s->n++];
    e->rel = strdup(rel);
    if (!e->rel) abort();
    e->kind = kind_char(st->st_mode);
    e->size = (int64_t)st->st_size;
    e->mtime_ns = mtime_ns(st);
    e->ino = (uint64_t)st->st_ino;
}

/* `root/rel` (or `root` for ""), malloc'd. */
static char *join_rel(const char *root, const char *rel) {
    size_t n = strlen(root) + strlen(rel) + 2;
    char *m = malloc(n);
    if (!m) abort();
    if (*rel) snprintf(m, n, "%s/%s", root, rel);
    else snprintf(m, n, "%s", root);
    return m;
}

static void scan_dir(Snap *s, const char *root, const char *rel, int recursive) {
    char *dir = join_rel(root, rel);
    DIR *d = opendir(dir);
    free(dir);
    if (!d) return;
    struct dirent *de;
    while ((de = readdir(d))) {
        if (!strcmp(de->d_name, ".") || !strcmp(de->d_name, "..")) continue;
        char *child = join_rel(rel, de->d_name);
        if (!*rel) {
            free(child);
            child = strdup(de->d_name);
            if (!child) abort();
        }
        char *full = join_rel(root, child);
        struct stat st;
        if (lstat(full, &st) == 0) {
            snap_add(s, child, &st);
            if (recursive && S_ISDIR(st.st_mode)) scan_dir(s, root, child, recursive);
        }
        free(full);
        free(child);
    }
    closedir(d);
}

static int entry_cmp(const void *a, const void *b) {
    return strcmp(((const Entry *)a)->rel, ((const Entry *)b)->rel);
}

/* The tree at `root`: its entries, or the file itself (as ""). -1 (errno set) if it can't
 * be looked at. */
static int scan(const char *root, int recursive, Snap *out) {
    struct stat st;
    if (stat(root, &st) < 0) return -1;
    if (S_ISDIR(st.st_mode)) scan_dir(out, root, "", recursive);
    else snap_add(out, "", &st);
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
    EV_SET(&ch[n++], w->wake[0], EVFILT_READ, EV_ADD, 0, 0, NULL);
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
    struct pollfd p[2] = {{fd, POLLIN, 0}, {w->wake[0], POLLIN, 0}};
    int got = poll(p, 2, ms);
    close(fd);
    if (got > 0) usleep(20000); /* let a burst of changes finish */
}
#endif

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
    close(w->wake[0]);
    close(w->wake[1]);
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
    if (pipe(w->wake) < 0) abort();
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
        char c = 1;
        if (write(w->wake[1], &c, 1) < 0) { /* the thread still ends at its timeout */ }
        pthread_mutex_unlock(&w->lock);
    }
    pthread_mutex_unlock(&watch_lock);
    if (j) {
        j->r = 1;
        finish(j);
    }
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
        if (op == 4 || op == 12) {
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
        if (op == 16) {
            watch_next(j);
            return j->id;
        }
        start_pool();
        pthread_mutex_lock(&pool_lock);
        if (work_tail) work_tail->next = j;
        else work_head = j;
        work_tail = j;
        pthread_cond_signal(&pool_cond);
        pthread_mutex_unlock(&pool_lock);
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
    case 106: /* close watcher a */
        watch_close(a);
        return 0;
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
    default: return -1;
    }
}

PplStr *ppl_io_text(int64_t id) {
    Job *j = taken(id);
    if (!j || !j->text) return ppl_str_new("", 0);
    return ppl_str_new(j->text, (int64_t)j->text_len);
}

#else
/* No file system support on this platform yet (Windows: P8.3). */
int64_t ppl_io(int64_t op, int64_t a, int64_t b, int64_t c, const PplStr *s, const PplStr *t) {
    (void)a, (void)b, (void)c, (void)s, (void)t;
    return op < 100 ? -1 : op == 101 ? 0 : -1;
}
PplStr *ppl_io_text(int64_t id) {
    (void)id;
    return ppl_str_new("", 0);
}
#endif
