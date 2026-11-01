/* The event loop's system interface (spec §10.1): a monotonic clock and waiting for events.
 * The loop itself (timers, microtasks, promises) is ppl code in the core library; it calls
 * `ppl_loop_wait` when nothing is ready. Only timeouts exist so far; I/O sources will be
 * registered with the same kqueue / epoll instance. */
#define _POSIX_C_SOURCE 200809L
#include "ppl_rt.h"

#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#if defined(__APPLE__)
#include <sys/event.h>
#include <unistd.h>
#elif defined(__linux__)
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>
#elif defined(_WIN32)
#include <windows.h>
#endif

int64_t ppl_clock_ms(void) {
#if defined(_WIN32)
    return (int64_t)GetTickCount64();
#else
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
#endif
}

#if defined(__APPLE__) || defined(__linux__)
/* Per thread: the poller its event loop waits on, and how other threads wake it. */
typedef struct {
    int poller;
#if defined(__linux__)
    int efd;
#endif
} Waker;
static _Thread_local Waker *waker;

static Waker *get_waker(void) {
    if (!waker) {
        waker = calloc(1, sizeof *waker);
        if (!waker) abort();
#if defined(__APPLE__)
        waker->poller = kqueue();
        struct kevent ev;
        EV_SET(&ev, 1, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0, NULL);
        if (waker->poller < 0 || kevent(waker->poller, &ev, 1, NULL, 0, NULL) < 0) {
            fflush(stdout);
            perror("fatal: event loop");
            abort();
        }
#else
        waker->poller = epoll_create1(0);
        waker->efd = eventfd(0, EFD_NONBLOCK);
        struct epoll_event ev = {.events = EPOLLIN, .data.fd = waker->efd};
        if (waker->poller < 0 || waker->efd < 0 ||
            epoll_ctl(waker->poller, EPOLL_CTL_ADD, waker->efd, &ev) < 0) {
            fflush(stdout);
            perror("fatal: event loop");
            abort();
        }
#endif
    }
    return waker;
}

static int get_poller(void) { return get_waker()->poller; }

void *ppl_loop_waker(void) { return get_waker(); }

/* Wakes the thread whose loop waits on `w` (from any thread). */
void ppl_loop_wake(void *w) {
    Waker *k = w;
    if (!k) return;
#if defined(__APPLE__)
    struct kevent ev;
    EV_SET(&ev, 1, EVFILT_USER, 0, NOTE_TRIGGER, 0, NULL);
    (void)kevent(k->poller, &ev, 1, NULL, 0, NULL);
#else
    uint64_t one = 1;
    (void)!write(k->efd, &one, sizeof one);
#endif
}
#else
void *ppl_loop_waker(void) { return NULL; }
void ppl_loop_wake(void *w) { (void)w; }
#endif

/* `ms` < 0: until woken. */
void ppl_loop_wait(int64_t ms) {
    if (ms == 0) return;
#if defined(__APPLE__)
    struct kevent ev;
    struct timespec timeout = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000};
    (void)kevent(get_poller(), NULL, 0, &ev, 1, ms < 0 ? NULL : &timeout);
#elif defined(__linux__)
    struct epoll_event ev;
    if (epoll_wait(get_poller(), &ev, 1, ms < 0 ? -1 : ms > 0x7fffffff ? 0x7fffffff : (int)ms) > 0) {
        uint64_t n;
        (void)!read(get_waker()->efd, &n, sizeof n);
    }
#elif defined(_WIN32)
    Sleep(ms < 0 ? INFINITE : (DWORD)ms);
#else
    struct timespec t = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000};
    if (ms > 0) nanosleep(&t, NULL);
#endif
    ppl_worker_check();
}
