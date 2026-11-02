/* Cryptography and TLS for ppl over Mbed TLS (P7.8, P7.5b). Byte-oriented, with no ppl
 * runtime types, so native programs (through `ppl_io`) and the interpreter (through FFI)
 * call the same code. */
#ifndef PPL_CRYPTO_H
#define PPL_CRYPTO_H

#include <stddef.h>
#include <stdint.h>

/* Runs crypto operation `op` (kind * 100 + algorithm; see ppl_crypto.c) on `in`: `n` bytes of
 * fields, each a 4-byte little-endian length then its bytes. On success returns 0 and sets
 * `*out` (malloc'd, freed with ppl_crypto_free) and `*out_len`; else a negative error:
 * -1 bad arguments, -2 authentication failed (AEAD), -3 the library failed. */
int64_t ppl_crypto_op(int64_t op, const uint8_t *in, size_t n, uint8_t **out, size_t *out_len);
void ppl_crypto_free(void *p);

/* TLS sessions on socket descriptors (ppl_tls.c). `ppl_tls_start` runs the handshake
 * (blocking): mode 0 client verifying the server against the system roots plus `pem1`
 * (extra CA certificates, PEM), 1 client without verification, 2 server with certificate
 * chain `pem1` and private key `pem2`. `host` is the server name to send and check. On
 * failure returns -1 with a message in `err`. Afterwards, `ppl_tls_read` (0 at the end)
 * and `ppl_tls_write` (all bytes) replace the socket's reads and writes; both return -2 for
 * a descriptor without a session, and -1 with errno set on errors. */
int ppl_tls_has(int fd);
int ppl_tls_start(int fd, int mode, const char *host, const char *pem1, size_t n1,
                  const char *pem2, size_t n2, char *err, size_t errcap);
ptrdiff_t ppl_tls_read(int fd, void *buf, size_t n);
ptrdiff_t ppl_tls_write(int fd, const void *buf, size_t n);
void ppl_tls_close_notify(int fd);
void ppl_tls_free(int fd);

#endif
