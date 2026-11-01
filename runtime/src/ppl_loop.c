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
static int poller = -1;

static int get_poller(void) {
    if (poller < 0) {
#if defined(__APPLE__)
        poller = kqueue();
#else
        poller = epoll_create1(0);
#endif
        if (poller < 0) {
            fflush(stdout);
            perror("fatal: event loop");
            abort();
        }
    }
    return poller;
}
#endif

void ppl_loop_wait(int64_t ms) {
    if (ms <= 0) return;
#if defined(__APPLE__)
    struct kevent ev;
    struct timespec timeout = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000};
    (void)kevent(get_poller(), NULL, 0, &ev, 1, &timeout);
#elif defined(__linux__)
    struct epoll_event ev;
    (void)epoll_wait(get_poller(), &ev, 1, ms > 0x7fffffff ? 0x7fffffff : (int)ms);
#elif defined(_WIN32)
    Sleep((DWORD)ms);
#else
    struct timespec t = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000};
    nanosleep(&t, NULL);
#endif
}
