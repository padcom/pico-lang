/* ppl's changes to Mbed TLS's default configuration (third_party/mbedtls), applied through
 * MBEDTLS_USER_CONFIG_FILE. Workers run on threads, so the library's global state (PSA, the
 * entropy sources) needs its locks. */
#ifndef PPL_MBEDTLS_CONFIG_H
#define PPL_MBEDTLS_CONFIG_H

/* (On Windows through winpthreads.) */
#define MBEDTLS_THREADING_C
#define MBEDTLS_THREADING_PTHREAD

#endif
