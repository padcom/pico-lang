/* Cryptography and TLS for pico over Mbed TLS (P7.8, P7.5b). Byte-oriented, with no pico
 * runtime types, so native programs (through `pico_io`) and the interpreter (through FFI)
 * call the same code. */
#ifndef PICO_CRYPTO_H
#define PICO_CRYPTO_H

#include <stddef.h>
#include <stdint.h>

/* Runs crypto operation `op` (kind * 100 + algorithm; see pico_crypto.c) on `in`: `n` bytes of
 * fields, each a 4-byte little-endian length then its bytes. On success returns 0 and sets
 * `*out` (malloc'd, freed with pico_crypto_free) and `*out_len`; else a negative error:
 * -1 bad arguments, -2 authentication failed (AEAD), -3 the library failed. */
int64_t pico_crypto_op(int64_t op, const uint8_t *in, size_t n, uint8_t **out, size_t *out_len);
void pico_crypto_free(void *p);

/* TLS sessions on socket descriptors (pico_tls.c). `pico_tls_start` runs the handshake
 * (blocking): mode 0 client verifying the server against the system roots plus `pem1`
 * (extra CA certificates, PEM), 1 client without verification, 2 server with certificate
 * chain `pem1` and private key `pem2`. `host` is the server name to send and check. On
 * failure returns -1 with a message in `err`. Afterwards, `pico_tls_read` (0 at the end)
 * and `pico_tls_write` (all bytes) replace the socket's reads and writes; both return -2 for
 * a descriptor without a session, and -1 with errno set on errors. */
int pico_tls_has(int fd);
int pico_tls_start(int fd, int mode, const char *host, const char *pem1, size_t n1,
                  const char *pem2, size_t n2, char *err, size_t errcap);
/* The handshake in steps, for callers that can't block (WASI): `pico_tls_begin` sets the
 * session up (-1 with `err`), then each `pico_tls_step` returns 0 when it's done, 1 when it waits
 * to read, 2 to write, -1 on failure (the session is gone). `pico_tls_try_read`/`_write` are the
 * non-blocking reads and writes: -3 waiting to read, -4 to write, else as above. */
int pico_tls_begin(int fd, int mode, const char *host, const char *pem1, size_t n1,
                  const char *pem2, size_t n2, char *err, size_t errcap);
int pico_tls_step(int fd, char *err, size_t errcap);
ptrdiff_t pico_tls_try_read(int fd, void *buf, size_t n);
ptrdiff_t pico_tls_try_write(int fd, const void *buf, size_t n);
ptrdiff_t pico_tls_read(int fd, void *buf, size_t n);
ptrdiff_t pico_tls_write(int fd, const void *buf, size_t n);
void pico_tls_close_notify(int fd);
void pico_tls_free(int fd);

#endif
