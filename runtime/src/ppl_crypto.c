/* Cryptography over Mbed TLS (spec §13.2 `std/crypto`): hashes, HMAC, HKDF, PBKDF2, AEAD
 * (AES-GCM, ChaCha20-Poly1305), elliptic-curve keys, ECDSA and ECDH. See ppl_crypto.h for
 * the calling convention. Operations (`op` = kind * 100 + algorithm):
 *
 *   1xx digest [data]                         hash algorithms: 1 SHA-1, 2 SHA-256,
 *   2xx hmac [key, data]                        3 SHA-384, 4 SHA-512, 5 SHA3-256,
 *   3xx hkdf [ikm, salt, info, length:u32]      6 SHA3-384, 7 SHA3-512, 8 SHA-224
 *   4xx pbkdf2 [password, salt, iterations:u32, length:u32]
 *   5xx AEAD seal [key, nonce, aad, plaintext] → ciphertext ‖ 16-byte tag
 *   6xx AEAD open [key, nonce, aad, ciphertext ‖ tag]   AEADs: 1 AES-GCM, 2 ChaCha20-Poly1305
 *   7xx generate a key pair → [private, public]     curves: 1 P-256, 2 P-384, 3 X25519
 *   8xx public key of [private] → public            (public keys: uncompressed points for
 *   9xx ECDSA sign [hash:u32, private, message] → r ‖ s    P-256/P-384, 32 bytes for X25519)
 *  10xx ECDSA verify [hash:u32, public, message, r ‖ s] → 1 byte (1 valid, 0 not)
 *  11xx ECDH [private, peer public] → shared secret
 */
#define _POSIX_C_SOURCE 200809L /* clock_gettime (WASI) */
#include "ppl_crypto.h"

#if defined(__wasi__)
#include <sys/random.h> /* getentropy */
#include <time.h>

#include "mbedtls/platform_time.h"

/* Mbed TLS's millisecond clock on WASI (MBEDTLS_PLATFORM_MS_TIME_ALT). */
mbedtls_ms_time_t mbedtls_ms_time(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (mbedtls_ms_time_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

/* Mbed TLS's entropy source on WASI (MBEDTLS_ENTROPY_HARDWARE_ALT). */
int mbedtls_hardware_poll(void *data, unsigned char *output, size_t len, size_t *olen) {
    (void)data;
    size_t n = len < 256 ? len : 256;
    if (getentropy(output, n) != 0) return -1;
    *olen = n;
    return 0;
}
#endif

#include <stdlib.h>
#include <string.h>

#include "mbedtls/chachapoly.h"
#include "mbedtls/ctr_drbg.h"
#include "mbedtls/ecdh.h"
#include "mbedtls/ecdsa.h"
#include "mbedtls/ecp.h"
#include "mbedtls/entropy.h"
#include "mbedtls/gcm.h"
#include "mbedtls/hkdf.h"
#include "mbedtls/md.h"
#include "mbedtls/pkcs5.h"

#define MAX_FIELDS 6

typedef struct {
    const uint8_t *p;
    size_t n;
} Field;

/* Splits `in` into `want` fields; 0 when it doesn't hold exactly that many. */
static int fields(const uint8_t *in, size_t n, Field *f, int want) {
    size_t at = 0;
    for (int i = 0; i < want; i++) {
        if (n - at < 4) return 0;
        size_t len = (size_t)in[at] | (size_t)in[at + 1] << 8 | (size_t)in[at + 2] << 16 |
                     (size_t)in[at + 3] << 24;
        at += 4;
        if (n - at < len) return 0;
        f[i].p = in + at;
        f[i].n = len;
        at += len;
    }
    return at == n;
}

static uint32_t u32_of(Field f) {
    if (f.n != 4) return 0;
    return (uint32_t)f.p[0] | (uint32_t)f.p[1] << 8 | (uint32_t)f.p[2] << 16 |
           (uint32_t)f.p[3] << 24;
}

static mbedtls_md_type_t md_of(int64_t alg) {
    switch (alg) {
    case 1: return MBEDTLS_MD_SHA1;
    case 2: return MBEDTLS_MD_SHA256;
    case 3: return MBEDTLS_MD_SHA384;
    case 4: return MBEDTLS_MD_SHA512;
    case 5: return MBEDTLS_MD_SHA3_256;
    case 6: return MBEDTLS_MD_SHA3_384;
    case 7: return MBEDTLS_MD_SHA3_512;
    case 8: return MBEDTLS_MD_SHA224;
    default: return MBEDTLS_MD_NONE;
    }
}

static mbedtls_ecp_group_id curve_of(int64_t alg) {
    switch (alg) {
    case 1: return MBEDTLS_ECP_DP_SECP256R1;
    case 2: return MBEDTLS_ECP_DP_SECP384R1;
    case 3: return MBEDTLS_ECP_DP_CURVE25519;
    default: return MBEDTLS_ECP_DP_NONE;
    }
}

/* A random generator per thread (workers run on threads), seeded from the system. */
static _Thread_local mbedtls_ctr_drbg_context drbg;
static _Thread_local mbedtls_entropy_context entropy;
static _Thread_local int drbg_ready;

static mbedtls_ctr_drbg_context *rng(void) {
    if (!drbg_ready) {
        mbedtls_entropy_init(&entropy);
        mbedtls_ctr_drbg_init(&drbg);
        static const unsigned char pers[] = "ppl";
        if (mbedtls_ctr_drbg_seed(&drbg, mbedtls_entropy_func, &entropy, pers, sizeof pers - 1)) {
            abort(); /* no randomness: nothing safe to fall back on */
        }
        drbg_ready = 1;
    }
    return &drbg;
}

/* A new output buffer of `n` bytes (at least one byte is allocated). */
static uint8_t *alloc(size_t n) {
    uint8_t *p = malloc(n ? n : 1);
    if (!p) abort();
    return p;
}

static void put_field(uint8_t *at, const uint8_t *p, size_t n) {
    at[0] = (uint8_t)n;
    at[1] = (uint8_t)(n >> 8);
    at[2] = (uint8_t)(n >> 16);
    at[3] = (uint8_t)(n >> 24);
    memcpy(at + 4, p, n);
}

void ppl_crypto_free(void *p) { free(p); }

/* The public key of keypair `kp` as bytes. */
static int write_public(mbedtls_ecp_keypair *kp, uint8_t *buf, size_t cap, size_t *len) {
    return mbedtls_ecp_write_public_key(kp, MBEDTLS_ECP_PF_UNCOMPRESSED, len, buf, cap);
}

static int read_public(mbedtls_ecp_group_id id, mbedtls_ecp_keypair *kp, Field f) {
    if (mbedtls_ecp_group_load(&kp->MBEDTLS_PRIVATE(grp), id)) return -1;
    if (mbedtls_ecp_point_read_binary(&kp->MBEDTLS_PRIVATE(grp), &kp->MBEDTLS_PRIVATE(Q), f.p,
                                      f.n)) {
        return -1;
    }
    return mbedtls_ecp_check_pubkey(&kp->MBEDTLS_PRIVATE(grp), &kp->MBEDTLS_PRIVATE(Q)) ? -1 : 0;
}

int64_t ppl_crypto_op(int64_t op, const uint8_t *in, size_t n, uint8_t **out, size_t *out_len) {
    Field f[MAX_FIELDS];
    int64_t kind = op / 100, alg = op % 100;
    *out = NULL;
    *out_len = 0;
    switch (kind) {
    case 1:
    case 2: {
        const mbedtls_md_info_t *md = mbedtls_md_info_from_type(md_of(alg));
        if (!md || !fields(in, n, f, kind == 1 ? 1 : 2)) return -1;
        size_t len = mbedtls_md_get_size(md);
        uint8_t *o = alloc(len);
        int r = kind == 1 ? mbedtls_md(md, f[0].p, f[0].n, o)
                          : mbedtls_md_hmac(md, f[0].p, f[0].n, f[1].p, f[1].n, o);
        if (r) {
            free(o);
            return -3;
        }
        *out = o;
        *out_len = len;
        return 0;
    }
    case 3: {
        const mbedtls_md_info_t *md = mbedtls_md_info_from_type(md_of(alg));
        if (!md || !fields(in, n, f, 4)) return -1;
        size_t len = u32_of(f[3]);
        if (len == 0 || len > 255 * mbedtls_md_get_size(md)) return -1;
        uint8_t *o = alloc(len);
        if (mbedtls_hkdf(md, f[1].p, f[1].n, f[0].p, f[0].n, f[2].p, f[2].n, o, len)) {
            free(o);
            return -3;
        }
        *out = o;
        *out_len = len;
        return 0;
    }
    case 4: {
        mbedtls_md_type_t md = md_of(alg);
        if (md == MBEDTLS_MD_NONE || !fields(in, n, f, 4)) return -1;
        uint32_t iters = u32_of(f[2]), len = u32_of(f[3]);
        if (iters == 0 || len == 0) return -1;
        uint8_t *o = alloc(len);
        if (mbedtls_pkcs5_pbkdf2_hmac_ext(md, f[0].p, f[0].n, f[1].p, f[1].n, iters, len, o)) {
            free(o);
            return -3;
        }
        *out = o;
        *out_len = len;
        return 0;
    }
    case 5:
    case 6: {
        if (!fields(in, n, f, 4)) return -1;
        Field key = f[0], nonce = f[1], aad = f[2], data = f[3];
        int seal = kind == 5;
        if (!seal && data.n < 16) return -2;
        size_t len = seal ? data.n + 16 : data.n - 16;
        uint8_t *o = alloc(len);
        int r;
        if (alg == 1) {
            if (key.n != 16 && key.n != 24 && key.n != 32) {
                free(o);
                return -1;
            }
            if (nonce.n == 0) {
                free(o);
                return -1;
            }
            mbedtls_gcm_context g;
            mbedtls_gcm_init(&g);
            r = mbedtls_gcm_setkey(&g, MBEDTLS_CIPHER_ID_AES, key.p, (unsigned)(key.n * 8));
            if (!r) {
                r = seal ? mbedtls_gcm_crypt_and_tag(&g, MBEDTLS_GCM_ENCRYPT, data.n, nonce.p,
                                                     nonce.n, aad.p, aad.n, data.p, o, 16,
                                                     o + data.n)
                         : mbedtls_gcm_auth_decrypt(&g, len, nonce.p, nonce.n, aad.p, aad.n,
                                                    data.p + len, 16, data.p, o);
                if (r == MBEDTLS_ERR_GCM_AUTH_FAILED) r = -2;
            }
            mbedtls_gcm_free(&g);
        } else if (alg == 2) {
            if (key.n != 32 || nonce.n != 12) {
                free(o);
                return -1;
            }
            mbedtls_chachapoly_context c;
            mbedtls_chachapoly_init(&c);
            r = mbedtls_chachapoly_setkey(&c, key.p);
            if (!r) {
                r = seal ? mbedtls_chachapoly_encrypt_and_tag(&c, data.n, nonce.p, aad.p, aad.n,
                                                              data.p, o, o + data.n)
                         : mbedtls_chachapoly_auth_decrypt(&c, len, nonce.p, aad.p, aad.n,
                                                           data.p + len, data.p, o);
                if (r == MBEDTLS_ERR_CHACHAPOLY_AUTH_FAILED) r = -2;
            }
            mbedtls_chachapoly_free(&c);
        } else {
            free(o);
            return -1;
        }
        if (r) {
            free(o);
            return r == -2 ? -2 : -3;
        }
        *out = o;
        *out_len = len;
        return 0;
    }
    case 7:
    case 8: {
        mbedtls_ecp_group_id id = curve_of(alg);
        if (id == MBEDTLS_ECP_DP_NONE) return -1;
        mbedtls_ecp_keypair kp;
        mbedtls_ecp_keypair_init(&kp);
        int r;
        if (kind == 7) {
            r = mbedtls_ecp_gen_key(id, &kp, mbedtls_ctr_drbg_random, rng());
        } else {
            if (!fields(in, n, f, 1)) {
                mbedtls_ecp_keypair_free(&kp);
                return -1;
            }
            r = mbedtls_ecp_read_key(id, &kp, f[0].p, f[0].n);
            if (!r) {
                r = mbedtls_ecp_keypair_calc_public(&kp, mbedtls_ctr_drbg_random, rng());
            }
        }
        uint8_t priv[MBEDTLS_ECP_MAX_BYTES], pub[2 * MBEDTLS_ECP_MAX_BYTES + 1];
        size_t plen = 0, qlen = 0;
        if (!r) r = mbedtls_ecp_write_key_ext(&kp, &plen, priv, sizeof priv);
        if (!r) r = write_public(&kp, pub, sizeof pub, &qlen);
        mbedtls_ecp_keypair_free(&kp);
        if (r) return kind == 8 ? -1 : -3;
        if (kind == 8) {
            uint8_t *o = alloc(qlen);
            memcpy(o, pub, qlen);
            *out = o;
            *out_len = qlen;
            return 0;
        }
        uint8_t *o = alloc(8 + plen + qlen);
        put_field(o, priv, plen);
        put_field(o + 4 + plen, pub, qlen);
        *out = o;
        *out_len = 8 + plen + qlen;
        return 0;
    }
    case 9:
    case 10: {
        mbedtls_ecp_group_id id = curve_of(alg);
        int sign = kind == 9;
        if ((id != MBEDTLS_ECP_DP_SECP256R1 && id != MBEDTLS_ECP_DP_SECP384R1) ||
            !fields(in, n, f, sign ? 3 : 4)) {
            return -1;
        }
        const mbedtls_md_info_t *md = mbedtls_md_info_from_type(md_of(u32_of(f[0])));
        if (!md) return -1;
        uint8_t hash[MBEDTLS_MD_MAX_SIZE];
        if (mbedtls_md(md, f[2].p, f[2].n, hash)) return -3;
        size_t hlen = mbedtls_md_get_size(md);
        mbedtls_ecp_keypair kp;
        mbedtls_ecp_keypair_init(&kp);
        mbedtls_mpi r, s;
        mbedtls_mpi_init(&r);
        mbedtls_mpi_init(&s);
        int64_t result = 0;
        size_t clen = id == MBEDTLS_ECP_DP_SECP256R1 ? 32 : 48;
        if (sign) {
            if (mbedtls_ecp_read_key(id, &kp, f[1].p, f[1].n) ||
                mbedtls_ecdsa_sign_det_ext(&kp.MBEDTLS_PRIVATE(grp), &r, &s,
                                           &kp.MBEDTLS_PRIVATE(d), hash, hlen,
                                           mbedtls_md_get_type(md), mbedtls_ctr_drbg_random,
                                           rng())) {
                result = -1;
            } else {
                uint8_t *o = alloc(2 * clen);
                if (mbedtls_mpi_write_binary(&r, o, clen) ||
                    mbedtls_mpi_write_binary(&s, o + clen, clen)) {
                    free(o);
                    result = -3;
                } else {
                    *out = o;
                    *out_len = 2 * clen;
                }
            }
        } else {
            int valid = 0;
            if (read_public(id, &kp, f[1]) == 0 && f[3].n == 2 * clen &&
                mbedtls_mpi_read_binary(&r, f[3].p, clen) == 0 &&
                mbedtls_mpi_read_binary(&s, f[3].p + clen, clen) == 0) {
                valid = mbedtls_ecdsa_verify(&kp.MBEDTLS_PRIVATE(grp), hash, hlen,
                                             &kp.MBEDTLS_PRIVATE(Q), &r, &s) == 0;
            }
            uint8_t *o = alloc(1);
            o[0] = (uint8_t)valid;
            *out = o;
            *out_len = 1;
        }
        mbedtls_mpi_free(&r);
        mbedtls_mpi_free(&s);
        mbedtls_ecp_keypair_free(&kp);
        return result;
    }
    case 11: {
        mbedtls_ecp_group_id id = curve_of(alg);
        if (id == MBEDTLS_ECP_DP_NONE || !fields(in, n, f, 2)) return -1;
        mbedtls_ecp_keypair mine, peer;
        mbedtls_ecp_keypair_init(&mine);
        mbedtls_ecp_keypair_init(&peer);
        mbedtls_mpi z;
        mbedtls_mpi_init(&z);
        int64_t result = 0;
        if (mbedtls_ecp_read_key(id, &mine, f[0].p, f[0].n) || read_public(id, &peer, f[1]) ||
            mbedtls_ecdh_compute_shared(&mine.MBEDTLS_PRIVATE(grp), &z, &peer.MBEDTLS_PRIVATE(Q),
                                        &mine.MBEDTLS_PRIVATE(d), mbedtls_ctr_drbg_random,
                                        rng())) {
            result = -1;
        } else {
            size_t len = (mine.MBEDTLS_PRIVATE(grp).pbits + 7) / 8;
            uint8_t *o = alloc(len);
            int r = id == MBEDTLS_ECP_DP_CURVE25519 ? mbedtls_mpi_write_binary_le(&z, o, len)
                                                    : mbedtls_mpi_write_binary(&z, o, len);
            if (r) {
                free(o);
                result = -3;
            } else {
                *out = o;
                *out_len = len;
            }
        }
        mbedtls_mpi_free(&z);
        mbedtls_ecp_keypair_free(&mine);
        mbedtls_ecp_keypair_free(&peer);
        return result;
    }
    default: return -1;
    }
}
