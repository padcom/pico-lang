/* ppl's changes to Mbed TLS's default configuration (third_party/mbedtls), applied through
 * MBEDTLS_USER_CONFIG_FILE. Workers run on threads, so the library's global state (PSA, the
 * entropy sources) needs its locks. */
#ifndef PPL_MBEDTLS_CONFIG_H
#define PPL_MBEDTLS_CONFIG_H

/* (On Windows through winpthreads.) */
#define MBEDTLS_THREADING_C
#define MBEDTLS_THREADING_PTHREAD

/* WASI (P8.5a): randomness through `mbedtls_hardware_poll` (getentropy, in ppl_crypto.c); no
 * BSD sockets or timers (ppl uses neither part). */
#if defined(__wasi__)
#define MBEDTLS_NO_PLATFORM_ENTROPY
#define MBEDTLS_ENTROPY_HARDWARE_ALT
#define MBEDTLS_PLATFORM_MS_TIME_ALT /* (also in ppl_crypto.c) */
#undef MBEDTLS_NET_C
#undef MBEDTLS_TIMING_C
#endif

#endif
