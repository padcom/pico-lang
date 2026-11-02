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

#endif
