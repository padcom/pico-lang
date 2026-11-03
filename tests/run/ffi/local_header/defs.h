/* A local header for `c:./defs.h` (tests/run/ffi/local_header). */
#include <stddef.h>

#define ANSWER 42
#define RATIO 0.5
#define FLAGS 0x10u
#define GREETING "hi"

enum Color { RED, GREEN = 5, BLUE };

struct Pair {
    int a;
    double b;
    unsigned char tag[4];
};

size_t strlen(const char *s);
void *memset(void *p, int c, size_t n);
