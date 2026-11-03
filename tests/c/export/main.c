#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include "libhash.h"

static void *worker(void *arg) {
    (void)arg;
    const char *s = "from a thread";
    printf("thread hash %llu calls %d\n", (unsigned long long)ppl_hash((uint8_t *)s, strlen(s)), ppl_calls());
    return NULL;
}

int main(void) {
    const char *s = "hello";
    printf("hash %llu\n", (unsigned long long)ppl_hash((uint8_t *)s, strlen(s)));
    int32_t xs[] = {1, 2, 3, 4};
    Stats st;
    bool ok = ppl_stats(xs, 4, &st);
    printf("stats %d %llu %llu calls %d\n", ok, (unsigned long long)st.count, (unsigned long long)st.sum, ppl_calls());
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    pthread_join(t, NULL);
    printf("main calls %d\n", ppl_calls());
    return 0;
}
