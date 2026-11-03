/* TLS over Mbed TLS for `std/net` connections (https:, wss:, `serve` with a certificate),
 * shared by native programs (ppl_io.c) and the interpreter (FFI). A session belongs to a
 * socket descriptor: once `ppl_tls_start` succeeds, reads and writes on that descriptor go
 * through `ppl_tls_read`/`ppl_tls_write` (callers check `ppl_tls_has`). The socket is made
 * non-blocking; each call holds the session's lock only while Mbed TLS runs and waits with
 * poll() outside it, so a read waiting for data never blocks a write. Shutting the socket
 * down wakes a waiting read (it then sees the end). On Windows the socket is a SOCKET kept in
 * an int (handle values fit), waits are 100 ms `WSAPoll` slices (a shutdown doesn't wake
 * one), and the roots are the system's ROOT store. */
#if defined(_WIN32)
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <wincrypt.h>
#endif
#include "ppl_crypto.h"

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if !defined(_WIN32) && !defined(__wasi__)
#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#endif
#if !defined(_WIN32)
#include <unistd.h> /* access */
#endif

#include "mbedtls/ctr_drbg.h"
#include "mbedtls/entropy.h"
#include "mbedtls/error.h"
#include "mbedtls/net_sockets.h"
#include "mbedtls/pk.h"
#include "mbedtls/ssl.h"
#include "mbedtls/x509_crt.h"
#include "psa/crypto.h"

typedef struct Tls {
    int fd;
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config conf;
    mbedtls_x509_crt ca, own;
    mbedtls_pk_context key;
    mbedtls_entropy_context entropy;
    mbedtls_ctr_drbg_context drbg;
    pthread_mutex_t lock;
    struct Tls *next;
} Tls;

static pthread_mutex_t table_lock = PTHREAD_MUTEX_INITIALIZER;
static Tls *sessions;
static pthread_once_t psa_once = PTHREAD_ONCE_INIT;
static int psa_ok;

static void psa_start(void) { psa_ok = psa_crypto_init() == PSA_SUCCESS; }

static Tls *find(int fd) {
    pthread_mutex_lock(&table_lock);
    Tls *t = sessions;
    while (t && t->fd != fd) t = t->next;
    pthread_mutex_unlock(&table_lock);
    return t;
}

int ppl_tls_has(int fd) { return find(fd) != NULL; }

/* ---- the socket calls, per platform ---- */
#if defined(__wasi__)
/* WASI: ppl's sockets (`ppl_io.c`: preview 2 streams behind ids), non-blocking. */
ptrdiff_t ppl_wasi_send(int fd, const unsigned char *buf, size_t len);
ptrdiff_t ppl_wasi_recv(int fd, unsigned char *buf, size_t len);
void ppl_wasi_wait(int fd, int write);

enum { E_INTR = EINTR, E_AGAIN = EAGAIN, E_RESET = ECONNRESET, E_PIPE = EPIPE };

static int sock_error(void) { return errno; }
static ptrdiff_t sock_send(int fd, const unsigned char *buf, size_t len) {
    return ppl_wasi_send(fd, buf, len);
}
static ptrdiff_t sock_recv(int fd, unsigned char *buf, size_t len) {
    return ppl_wasi_recv(fd, buf, len);
}
static int set_nonblocking(int fd, int on) {
    (void)fd, (void)on;
    return 0;
}
static void wait_for(int fd, int r) { ppl_wasi_wait(fd, r == MBEDTLS_ERR_SSL_WANT_WRITE); }
#elif defined(_WIN32)
enum { E_INTR = WSAEINTR, E_AGAIN = WSAEWOULDBLOCK, E_RESET = WSAECONNRESET, E_PIPE = WSAESHUTDOWN };

static int sock_error(void) { return WSAGetLastError(); }

static ptrdiff_t sock_send(int fd, const unsigned char *buf, size_t len) {
    return send((SOCKET)fd, (const char *)buf, len > 0x40000000 ? 0x40000000 : (int)len, 0);
}

static ptrdiff_t sock_recv(int fd, unsigned char *buf, size_t len) {
    return recv((SOCKET)fd, (char *)buf, len > 0x40000000 ? 0x40000000 : (int)len, 0);
}

/* Non-blocking on (1) or off (0); Windows can't tell what it was, and our sockets block. */
static int set_nonblocking(int fd, int on) {
    u_long v = (u_long)on;
    return ioctlsocket((SOCKET)fd, FIONBIO, &v) == 0 ? 0 : -1;
}

/* Waits (up to 100 ms: a shutdown doesn't end a WSAPoll) for what Mbed TLS asked for. */
static void wait_for(int fd, int r) {
    WSAPOLLFD p = {(SOCKET)fd, (short)(r == MBEDTLS_ERR_SSL_WANT_WRITE ? POLLOUT : POLLIN), 0};
    WSAPoll(&p, 1, 100);
}
#else
enum { E_INTR = EINTR, E_AGAIN = EAGAIN, E_RESET = ECONNRESET, E_PIPE = EPIPE };

static int sock_error(void) { return errno == EWOULDBLOCK ? EAGAIN : errno; }

static ptrdiff_t sock_send(int fd, const unsigned char *buf, size_t len) {
#if defined(MSG_NOSIGNAL)
    return send(fd, buf, len, MSG_NOSIGNAL);
#else
    return send(fd, buf, len, 0);
#endif
}

static ptrdiff_t sock_recv(int fd, unsigned char *buf, size_t len) { return recv(fd, buf, len, 0); }

static int set_nonblocking(int fd, int on) {
    int fl = fcntl(fd, F_GETFL);
    if (fl < 0) return -1;
    return fcntl(fd, F_SETFL, on ? fl | O_NONBLOCK : fl & ~O_NONBLOCK);
}

/* Waits until the socket is ready for what Mbed TLS asked for. */
static void wait_for(int fd, int r) {
    struct pollfd p = {fd, (short)(r == MBEDTLS_ERR_SSL_WANT_WRITE ? POLLOUT : POLLIN), 0};
    while (poll(&p, 1, -1) < 0 && errno == EINTR) {
    }
}
#endif

static int bio_send(void *ctx, const unsigned char *buf, size_t len) {
    int fd = *(int *)ctx;
    for (;;) {
        ptrdiff_t k = sock_send(fd, buf, len);
        if (k >= 0) return (int)k;
        int e = sock_error();
        if (e == E_INTR) continue;
        if (e == E_AGAIN) return MBEDTLS_ERR_SSL_WANT_WRITE;
        if (e == E_PIPE || e == E_RESET) return MBEDTLS_ERR_NET_CONN_RESET;
        return MBEDTLS_ERR_NET_SEND_FAILED;
    }
}

static int bio_recv(void *ctx, unsigned char *buf, size_t len) {
    int fd = *(int *)ctx;
    for (;;) {
        ptrdiff_t k = sock_recv(fd, buf, len);
        if (k >= 0) return (int)k;
        int e = sock_error();
        if (e == E_INTR) continue;
        if (e == E_AGAIN) return MBEDTLS_ERR_SSL_WANT_READ;
        if (e == E_RESET) return MBEDTLS_ERR_NET_CONN_RESET;
        return MBEDTLS_ERR_NET_RECV_FAILED;
    }
}

static void tls_free(Tls *t) {
    mbedtls_ssl_free(&t->ssl);
    mbedtls_ssl_config_free(&t->conf);
    mbedtls_x509_crt_free(&t->ca);
    mbedtls_x509_crt_free(&t->own);
    mbedtls_pk_free(&t->key);
    mbedtls_ctr_drbg_free(&t->drbg);
    mbedtls_entropy_free(&t->entropy);
    pthread_mutex_destroy(&t->lock);
    free(t);
}

/* Parses PEM text (not NUL-terminated) into `crt`: Mbed TLS wants the terminator counted. */
static int parse_pem_crt(mbedtls_x509_crt *crt, const char *pem, size_t n) {
    char *z = malloc(n + 1);
    if (!z) abort();
    memcpy(z, pem, n);
    z[n] = 0;
    int r = mbedtls_x509_crt_parse(crt, (const unsigned char *)z, n + 1);
    free(z);
    return r;
}

/* The system's trusted roots: `SSL_CERT_FILE`, `SSL_CERT_DIR`, else the usual bundle
 * locations (Windows: the ROOT store). Whether it found any. */
static int load_roots(mbedtls_x509_crt *ca) {
    int found = 0;
    const char *file = getenv("SSL_CERT_FILE");
    if (file && *file && mbedtls_x509_crt_parse_file(ca, file) >= 0) found = 1;
    const char *dir = getenv("SSL_CERT_DIR");
    if (dir && *dir && mbedtls_x509_crt_parse_path(ca, dir) >= 0) found = 1;
    if (found) return 1;
#if defined(_WIN32)
    HCERTSTORE store = CertOpenSystemStoreW(0, L"ROOT");
    if (!store) return 0;
    const CERT_CONTEXT *c = NULL;
    while ((c = CertEnumCertificatesInStore(store, c))) {
        if (c->dwCertEncodingType & X509_ASN_ENCODING &&
            mbedtls_x509_crt_parse_der(ca, c->pbCertEncoded, c->cbCertEncoded) == 0)
            found = 1;
    }
    CertCloseStore(store, 0);
    return found;
#else
    static const char *const bundles[] = {
        "/etc/ssl/cert.pem",                  /* macOS, Alpine, OpenBSD */
        "/etc/ssl/certs/ca-certificates.crt", /* Debian, Ubuntu, Arch */
        "/etc/pki/tls/certs/ca-bundle.crt",   /* Fedora, RHEL */
        "/etc/ssl/ca-bundle.pem",             /* openSUSE */
        "/etc/pki/tls/cacert.pem",
    };
    for (size_t i = 0; i < sizeof bundles / sizeof *bundles; i++) {
        /* (A bundle with a few unparsable entries still counts.) */
        if (access(bundles[i], R_OK) == 0 && mbedtls_x509_crt_parse_file(ca, bundles[i]) >= 0) {
            return 1;
        }
    }
    return 0;
#endif
}

static void set_err(char *err, size_t cap, const char *what, int r) {
    char buf[160];
    mbedtls_strerror(r, buf, sizeof buf);
    snprintf(err, cap, "%s: %s", what, buf);
}

int ppl_tls_begin(int fd, int mode, const char *host, const char *pem1, size_t n1,
                  const char *pem2, size_t n2, char *err, size_t errcap) {
    pthread_once(&psa_once, psa_start);
    if (!psa_ok) {
        snprintf(err, errcap, "TLS: the crypto library didn't start");
        return -1;
    }
    Tls *t = calloc(1, sizeof *t);
    if (!t) abort();
    t->fd = fd;
    pthread_mutex_init(&t->lock, NULL);
    mbedtls_ssl_init(&t->ssl);
    mbedtls_ssl_config_init(&t->conf);
    mbedtls_x509_crt_init(&t->ca);
    mbedtls_x509_crt_init(&t->own);
    mbedtls_pk_init(&t->key);
    mbedtls_entropy_init(&t->entropy);
    mbedtls_ctr_drbg_init(&t->drbg);
    int server = mode == 2;
    int r = mbedtls_ctr_drbg_seed(&t->drbg, mbedtls_entropy_func, &t->entropy,
                                  (const unsigned char *)"ppl-tls", 7);
    if (r) {
        set_err(err, errcap, "TLS: no randomness", r);
        tls_free(t);
        return -1;
    }
    r = mbedtls_ssl_config_defaults(&t->conf, server ? MBEDTLS_SSL_IS_SERVER : MBEDTLS_SSL_IS_CLIENT,
                                    MBEDTLS_SSL_TRANSPORT_STREAM, MBEDTLS_SSL_PRESET_DEFAULT);
    if (r) {
        set_err(err, errcap, "TLS: configuration failed", r);
        tls_free(t);
        return -1;
    }
    mbedtls_ssl_conf_rng(&t->conf, mbedtls_ctr_drbg_random, &t->drbg);
    if (server) {
        /* pem1: the certificate chain, pem2: the private key. */
        r = parse_pem_crt(&t->own, pem1, n1);
        if (r) {
            set_err(err, errcap, "TLS: invalid certificate", r);
            tls_free(t);
            return -1;
        }
        char *z = malloc(n2 + 1);
        if (!z) abort();
        memcpy(z, pem2, n2);
        z[n2] = 0;
        r = mbedtls_pk_parse_key(&t->key, (const unsigned char *)z, n2 + 1, NULL, 0,
                                 mbedtls_ctr_drbg_random, &t->drbg);
        free(z);
        if (!r) r = mbedtls_ssl_conf_own_cert(&t->conf, &t->own, &t->key);
        if (r) {
            set_err(err, errcap, "TLS: invalid private key", r);
            tls_free(t);
            return -1;
        }
        mbedtls_ssl_conf_authmode(&t->conf, MBEDTLS_SSL_VERIFY_NONE);
    } else {
        /* pem1: extra trusted certificates (besides the system's). Mode 1 doesn't verify. */
        int roots = load_roots(&t->ca);
        if (n1 > 0) {
            r = parse_pem_crt(&t->ca, pem1, n1);
            if (r < 0) {
                set_err(err, errcap, "TLS: invalid CA certificate", r);
                tls_free(t);
                return -1;
            }
            roots = 1;
        }
        /* (WASI has no system roots in reach: there, only `caCerts` are trusted, and other
         * servers fail verification as untrusted ones do.) */
#if defined(__wasi__)
        (void)roots;
        if (0) {
#else
        if (mode == 0 && !roots) {
#endif
            snprintf(err, errcap, "TLS: no trusted root certificates found (set SSL_CERT_FILE)");
            tls_free(t);
            return -1;
        }
        mbedtls_ssl_conf_ca_chain(&t->conf, &t->ca, NULL);
        mbedtls_ssl_conf_authmode(&t->conf,
                                  mode == 1 ? MBEDTLS_SSL_VERIFY_NONE : MBEDTLS_SSL_VERIFY_REQUIRED);
    }
    r = mbedtls_ssl_setup(&t->ssl, &t->conf);
    if (!r && !server && host && *host) r = mbedtls_ssl_set_hostname(&t->ssl, host);
    if (r) {
        set_err(err, errcap, "TLS: setup failed", r);
        tls_free(t);
        return -1;
    }
#if defined(SO_NOSIGPIPE)
    {
        int one = 1;
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
    }
#endif
    set_nonblocking(fd, 1);
    mbedtls_ssl_set_bio(&t->ssl, &t->fd, bio_send, bio_recv, NULL);
    pthread_mutex_lock(&table_lock);
    t->next = sessions;
    sessions = t;
    pthread_mutex_unlock(&table_lock);
    return 0;
}

/* Takes session `fd` out of the table and frees it. */
static void tls_drop(int fd) {
    pthread_mutex_lock(&table_lock);
    Tls **p = &sessions;
    while (*p && (*p)->fd != fd) p = &(*p)->next;
    Tls *t = *p;
    if (t) *p = t->next;
    pthread_mutex_unlock(&table_lock);
    if (t) tls_free(t);
}

int ppl_tls_step(int fd, char *err, size_t errcap) {
    Tls *t = find(fd);
    if (!t) {
        snprintf(err, errcap, "TLS: no session");
        return -1;
    }
    int r = mbedtls_ssl_handshake(&t->ssl);
    if (r == 0) return 0;
    if (r == MBEDTLS_ERR_SSL_WANT_READ) return 1;
    if (r == MBEDTLS_ERR_SSL_WANT_WRITE) return 2;
    {
        uint32_t flags = mbedtls_ssl_get_verify_result(&t->ssl);
        if (r == MBEDTLS_ERR_X509_CERT_VERIFY_FAILED && flags != 0 && flags != (uint32_t)-1) {
            char info[256];
            mbedtls_x509_crt_verify_info(info, sizeof info, "", flags);
            size_t k = strlen(info);
            while (k > 0 && (info[k - 1] == '\n' || info[k - 1] == ' ')) info[--k] = 0;
            for (char *p = info; *p; p++) {
                if (*p == '\n') *p = ';';
            }
            snprintf(err, errcap, "TLS: certificate verification failed: %s", info);
        } else {
            set_err(err, errcap, "TLS: handshake failed", r);
        }
        set_nonblocking(fd, 0);
        tls_drop(fd);
        return -1;
    }
}

int ppl_tls_start(int fd, int mode, const char *host, const char *pem1, size_t n1,
                  const char *pem2, size_t n2, char *err, size_t errcap) {
    if (ppl_tls_begin(fd, mode, host, pem1, n1, pem2, n2, err, errcap) < 0) return -1;
    for (;;) {
        int r = ppl_tls_step(fd, err, errcap);
        if (r <= 0) return r;
        wait_for(fd, r == 1 ? MBEDTLS_ERR_SSL_WANT_READ : MBEDTLS_ERR_SSL_WANT_WRITE);
    }
}

ptrdiff_t ppl_tls_try_read(int fd, void *buf, size_t n) {
    Tls *t = find(fd);
    if (!t) return -2;
    for (;;) {
        pthread_mutex_lock(&t->lock);
        int r = mbedtls_ssl_read(&t->ssl, buf, n);
        pthread_mutex_unlock(&t->lock);
        if (r > 0) return r;
        if (r == 0 || r == MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY || r == MBEDTLS_ERR_SSL_CONN_EOF) {
            return 0;
        }
        if (r == MBEDTLS_ERR_SSL_WANT_READ) return -3;
        if (r == MBEDTLS_ERR_SSL_WANT_WRITE) return -4;
        if (r == MBEDTLS_ERR_SSL_RECEIVED_NEW_SESSION_TICKET) continue;
        errno = r == MBEDTLS_ERR_NET_RECV_FAILED || r == MBEDTLS_ERR_NET_CONN_RESET ? ECONNRESET
                                                                                   : EPROTO;
        return -1;
    }
}

ptrdiff_t ppl_tls_try_write(int fd, const void *buf, size_t n) {
    Tls *t = find(fd);
    if (!t) return -2;
    pthread_mutex_lock(&t->lock);
    int r = mbedtls_ssl_write(&t->ssl, buf, n);
    pthread_mutex_unlock(&t->lock);
    if (r > 0) return r;
    if (r == MBEDTLS_ERR_SSL_WANT_READ) return -3;
    if (r == MBEDTLS_ERR_SSL_WANT_WRITE) return -4;
    errno = r == MBEDTLS_ERR_NET_CONN_RESET ? EPIPE : EPROTO;
    return -1;
}

ptrdiff_t ppl_tls_read(int fd, void *buf, size_t n) {
    Tls *t = find(fd);
    if (!t) return -2;
    for (;;) {
        pthread_mutex_lock(&t->lock);
        int r = mbedtls_ssl_read(&t->ssl, buf, n);
        pthread_mutex_unlock(&t->lock);
        if (r > 0) return r;
        if (r == 0 || r == MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY || r == MBEDTLS_ERR_SSL_CONN_EOF) {
            return 0;
        }
        if (r == MBEDTLS_ERR_SSL_WANT_READ || r == MBEDTLS_ERR_SSL_WANT_WRITE) {
            wait_for(fd, r);
            continue;
        }
        if (r == MBEDTLS_ERR_SSL_RECEIVED_NEW_SESSION_TICKET) continue;
        /* A shut-down socket reads as the end. */
        if (r == MBEDTLS_ERR_NET_RECV_FAILED || r == MBEDTLS_ERR_NET_CONN_RESET) {
            errno = ECONNRESET;
            return -1;
        }
        errno = EPROTO;
        return -1;
    }
}

ptrdiff_t ppl_tls_write(int fd, const void *buf, size_t n) {
    Tls *t = find(fd);
    if (!t) return -2;
    size_t done = 0;
    while (done < n) {
        pthread_mutex_lock(&t->lock);
        int r = mbedtls_ssl_write(&t->ssl, (const unsigned char *)buf + done, n - done);
        pthread_mutex_unlock(&t->lock);
        if (r > 0) {
            done += (size_t)r;
            continue;
        }
        if (r == MBEDTLS_ERR_SSL_WANT_READ || r == MBEDTLS_ERR_SSL_WANT_WRITE) {
            wait_for(fd, r);
            continue;
        }
        errno = r == MBEDTLS_ERR_NET_CONN_RESET ? EPIPE : EPROTO;
        return -1;
    }
    return (ptrdiff_t)done;
}

void ppl_tls_close_notify(int fd) {
    Tls *t = find(fd);
    if (!t) return;
    for (int tries = 0; tries < 100; tries++) {
        pthread_mutex_lock(&t->lock);
        int r = mbedtls_ssl_close_notify(&t->ssl);
        pthread_mutex_unlock(&t->lock);
        if (r != MBEDTLS_ERR_SSL_WANT_WRITE && r != MBEDTLS_ERR_SSL_WANT_READ) return;
        wait_for(fd, MBEDTLS_ERR_SSL_WANT_WRITE);
    }
}

void ppl_tls_free(int fd) { tls_drop(fd); }
