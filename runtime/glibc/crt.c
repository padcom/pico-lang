/* What glibc's Scrt1.o and libc_nonshared.a give a program, written for pico (decision 245):
 * linked into every linux-gnu executable, compiled against the target's glibc version. */
#define _GNU_SOURCE
#include <stddef.h>
#include <sys/stat.h>

/* glibc's stdio checks for this to tell its FILE layout from pre-2.1 ones. */
const int _IO_stdin_used = 0x20001;

/* Identifies the executable to `__cxa_atexit`. */
void *__dso_handle __attribute__((visibility("hidden"))) = &__dso_handle;

extern void (*__preinit_array_start[])(int, char **, char **) __attribute__((visibility("hidden")));
extern void (*__preinit_array_end[])(int, char **, char **) __attribute__((visibility("hidden")));
extern void (*__init_array_start[])(int, char **, char **) __attribute__((visibility("hidden")));
extern void (*__init_array_end[])(int, char **, char **) __attribute__((visibility("hidden")));

/* `__libc_start_main`'s init argument: the program's constructors. Before glibc 2.34 the
 * program ran them itself (`__libc_csu_init`); 2.34+ still calls an init it's given. */
__attribute__((visibility("hidden"))) void pico_glibc_init(int argc, char **argv, char **envp) {
    size_t n = (size_t)(__preinit_array_end - __preinit_array_start);
    for (size_t i = 0; i < n; i++) __preinit_array_start[i](argc, argv, envp);
    n = (size_t)(__init_array_end - __init_array_start);
    for (size_t i = 0; i < n; i++) __init_array_start[i](argc, argv, envp);
}

#if __GLIBC_MINOR__ < 33
/* Before glibc 2.33, the stat family wasn't exported: libc_nonshared.a wrapped the versioned
 * `__xstat` functions, so programs must bring the wrappers. */
#if defined(__x86_64__)
#define STAT_VER 1
#else
#define STAT_VER 0
#endif
int __xstat(int, const char *, struct stat *);
int __fxstat(int, int, struct stat *);
int __lxstat(int, const char *, struct stat *);
int __fxstatat(int, int, const char *, struct stat *, int);
int __xstat64(int, const char *, struct stat64 *);
int __fxstat64(int, int, struct stat64 *);
int __lxstat64(int, const char *, struct stat64 *);
int __fxstatat64(int, int, const char *, struct stat64 *, int);
int __xmknod(int, const char *, mode_t, dev_t *);
int __xmknodat(int, int, const char *, mode_t, dev_t *);

int stat(const char *p, struct stat *b) { return __xstat(STAT_VER, p, b); }
int fstat(int fd, struct stat *b) { return __fxstat(STAT_VER, fd, b); }
int lstat(const char *p, struct stat *b) { return __lxstat(STAT_VER, p, b); }
int fstatat(int dir, const char *p, struct stat *b, int flags) {
    return __fxstatat(STAT_VER, dir, p, b, flags);
}
int stat64(const char *p, struct stat64 *b) { return __xstat64(STAT_VER, p, b); }
int fstat64(int fd, struct stat64 *b) { return __fxstat64(STAT_VER, fd, b); }
int lstat64(const char *p, struct stat64 *b) { return __lxstat64(STAT_VER, p, b); }
int fstatat64(int dir, const char *p, struct stat64 *b, int flags) {
    return __fxstatat64(STAT_VER, dir, p, b, flags);
}
int mknod(const char *p, mode_t m, dev_t d) { return __xmknod(0, p, m, &d); }
int mknodat(int dir, const char *p, mode_t m, dev_t d) { return __xmknodat(0, dir, p, m, &d); }
#endif
