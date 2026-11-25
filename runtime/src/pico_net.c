/* UDP multicast for `std/net` (#214): socket options of a bound UDP socket, a UDP socket that
 * shares its address and port, and the machine's network interfaces. Used by compiled programs
 * (`pico_io` ops 170, 171 and `bind`'s `reuseAddress`) and, linked into the compiler like
 * pico_tls.c, by the interpreter. Errors are -(kind | errno << 8), the kinds of pico_io.c.
 * Nothing here on WASI or in the browser: no multicast there. */
#if defined(_WIN32) || defined(__APPLE__) || defined(__linux__)
#if defined(__linux__)
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(_WIN32)
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#if __has_include(<iphlpapi.h>)
#include <iphlpapi.h>
#else
/* The bundled MinGW headers (zig-libc) have no iphlpapi.h: the stable start of its structures,
 * as far as they're read here (the system fills in the rest; `Next` links them). */
typedef struct _IP_ADAPTER_UNICAST_ADDRESS {
    union {
        ULONGLONG Alignment;
        struct {
            ULONG Length;
            DWORD Flags;
        };
    };
    struct _IP_ADAPTER_UNICAST_ADDRESS *Next;
    SOCKET_ADDRESS Address;
} IP_ADAPTER_UNICAST_ADDRESS;
typedef struct _IP_ADAPTER_ADDRESSES {
    union {
        ULONGLONG Alignment;
        struct {
            ULONG Length;
            DWORD IfIndex;
        };
    };
    struct _IP_ADAPTER_ADDRESSES *Next;
    PCHAR AdapterName;
    IP_ADAPTER_UNICAST_ADDRESS *FirstUnicastAddress;
    void *FirstAnycastAddress;
    void *FirstMulticastAddress;
    void *FirstDnsServerAddress;
    PWCHAR DnsSuffix;
    PWCHAR Description;
    PWCHAR FriendlyName;
    BYTE PhysicalAddress[8];
    DWORD PhysicalAddressLength;
    DWORD Flags;
    DWORD Mtu;
    DWORD IfType;
    int OperStatus;
} IP_ADAPTER_ADDRESSES;
#define GAA_FLAG_SKIP_ANYCAST 0x0002
#define GAA_FLAG_SKIP_MULTICAST 0x0004
#define GAA_FLAG_SKIP_DNS_SERVER 0x0008
#define IP_ADAPTER_NO_MULTICAST 0x0010
#define IF_TYPE_SOFTWARE_LOOPBACK 24
#define IfOperStatusUp 1
ULONG WINAPI GetAdaptersAddresses(ULONG Family, ULONG Flags, PVOID Reserved, IP_ADAPTER_ADDRESSES *Addresses,
                                  PULONG SizePointer);
ULONG WINAPI if_nametoindex(PCSTR InterfaceName);
#endif
typedef SOCKET sock_t;
#define SOCK_NONE ((int64_t)INVALID_SOCKET)
#else
#include <arpa/inet.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#if __has_include(<netinet/tcp.h>)
#include <netinet/tcp.h>
#endif
#include <sys/socket.h>
#include <unistd.h>
typedef int sock_t;
#define SOCK_NONE ((int64_t)-1)
#if __has_include(<ifaddrs.h>)
#include <ifaddrs.h>
#else
/* The bundled macOS headers (zig-libc) have no ifaddrs.h; its structure has been the same since
 * 4.4BSD. */
struct ifaddrs {
    struct ifaddrs *ifa_next;
    char *ifa_name;
    unsigned int ifa_flags;
    struct sockaddr *ifa_addr;
    struct sockaddr *ifa_netmask;
    struct sockaddr *ifa_dstaddr;
    void *ifa_data;
};
int getifaddrs(struct ifaddrs **);
void freeifaddrs(struct ifaddrs *);
#endif
#if __has_include(<net/if.h>)
#include <net/if.h>
#else
#define IFF_UP 0x1
#define IFF_LOOPBACK 0x8
#define IFF_MULTICAST 0x8000
unsigned int if_nametoindex(const char *);
#endif
#endif

/* pico_io.c's kinds. */
enum { K_PERMISSION_DENIED = 3, K_OTHER = 8, K_ADDR_IN_USE = 11, K_ADDR_NOT_AVAIL = 12 };

static void net_init(void) {
#if defined(_WIN32)
    static volatile LONG done;
    if (InterlockedCompareExchange(&done, 1, 0) == 0) {
        WSADATA w;
        WSAStartup(MAKEWORD(2, 2), &w);
    }
#endif
}

/* The last socket error: -(kind | errno << 8). */
static int64_t net_err(void) {
#if defined(_WIN32)
    int w = WSAGetLastError();
    int k = w == WSAEADDRINUSE ? K_ADDR_IN_USE
          : w == WSAEADDRNOTAVAIL ? K_ADDR_NOT_AVAIL
          : w == WSAEACCES ? K_PERMISSION_DENIED
                           : K_OTHER;
    return -(int64_t)(k | ((int64_t)w << 8));
#else
    int e = errno;
    int k = e == EADDRINUSE ? K_ADDR_IN_USE
          : e == EADDRNOTAVAIL ? K_ADDR_NOT_AVAIL
          : (e == EACCES || e == EPERM) ? K_PERMISSION_DENIED
                                         : K_OTHER;
    return -(int64_t)(k | ((int64_t)e << 8));
#endif
}

static int64_t bad_address(void) { return -(int64_t)(K_ADDR_NOT_AVAIL | ((int64_t)EINVAL << 8)); }

static void sock_close(int64_t fd) {
#if defined(_WIN32)
    closesocket((SOCKET)fd);
#else
    close((int)fd);
#endif
}

/* Not inherited by children. */
static void sock_private(int64_t fd) {
#if defined(_WIN32)
    SetHandleInformation((HANDLE)(intptr_t)fd, HANDLE_FLAG_INHERIT, 0);
#else
    fcntl((int)fd, F_SETFD, FD_CLOEXEC);
#endif
}

static void sock_reuse(int64_t fd) {
    int one = 1;
    setsockopt((sock_t)fd, SOL_SOCKET, SO_REUSEADDR, (const char *)&one, sizeof one);
#if defined(SO_REUSEPORT) && !defined(_WIN32)
    setsockopt((sock_t)fd, SOL_SOCKET, SO_REUSEPORT, (const char *)&one, sizeof one);
#endif
}

/* The address family the socket is bound to (AF_INET or AF_INET6). */
static int sock_family(int64_t fd) {
    struct sockaddr_storage a;
    socklen_t n = sizeof a;
    if (getsockname((sock_t)fd, (struct sockaddr *)&a, &n) != 0) return AF_INET;
    return a.ss_family;
}

/* An interface given by address (IPv4) or name (IPv6): its IPv4 address / index. */
static unsigned iface_index(const char *iface) {
    if (!iface || !*iface) return 0;
    unsigned i = if_nametoindex(iface);
    return i ? i : (unsigned)atoi(iface);
}

/* Option `what` of UDP socket `fd`: 1 join group `group` (on interface `iface`, "" any),
 * 2 leave it, 3 multicast TTL / hops `n`, 4 multicast loopback `n` (0/1), 5 the interface
 * multicasts go out on (`iface`). */
int64_t pico_net_udp_option(int64_t fd, int64_t what, int64_t n, const char *group, const char *iface) {
    net_init();
    int v6 = sock_family(fd) == AF_INET6;
    int r;
    if (what == 1 || what == 2) {
        struct in_addr g4;
        struct in6_addr g6;
        if (inet_pton(AF_INET, group, &g4) == 1) {
            struct ip_mreq m;
            memset(&m, 0, sizeof m);
            m.imr_multiaddr = g4;
            m.imr_interface.s_addr = htonl(INADDR_ANY);
            if (iface && *iface && inet_pton(AF_INET, iface, &m.imr_interface) != 1) {
                return bad_address();
            }
            r = setsockopt((sock_t)fd, IPPROTO_IP, what == 1 ? IP_ADD_MEMBERSHIP : IP_DROP_MEMBERSHIP,
                           (const char *)&m, sizeof m);
        } else if (inet_pton(AF_INET6, group, &g6) == 1) {
            struct ipv6_mreq m;
            memset(&m, 0, sizeof m);
            m.ipv6mr_multiaddr = g6;
            m.ipv6mr_interface = iface_index(iface);
            r = setsockopt((sock_t)fd, IPPROTO_IPV6, what == 1 ? IPV6_JOIN_GROUP : IPV6_LEAVE_GROUP,
                           (const char *)&m, sizeof m);
        } else {
            return bad_address();
        }
    } else if (what == 3 || what == 4) {
        int v = (int)n;
#if defined(_WIN32)
        DWORD dv = (DWORD)v;
        const char *p = (const char *)&dv;
        int len = sizeof dv;
#else
        /* (IP_MULTICAST_TTL/LOOP take a byte on some systems; an int works on Linux and
         * macOS, a byte on all of them for IPv4.) */
        unsigned char b = (unsigned char)v;
        const char *p = v6 ? (const char *)&v : (const char *)&b;
        socklen_t len = v6 ? sizeof v : sizeof b;
#endif
        if (v6) {
            r = setsockopt((sock_t)fd, IPPROTO_IPV6, what == 3 ? IPV6_MULTICAST_HOPS : IPV6_MULTICAST_LOOP,
                           p, len);
        } else {
            r = setsockopt((sock_t)fd, IPPROTO_IP, what == 3 ? IP_MULTICAST_TTL : IP_MULTICAST_LOOP, p, len);
        }
    } else if (what == 5) {
        if (v6) {
            unsigned i = iface_index(iface);
            r = setsockopt((sock_t)fd, IPPROTO_IPV6, IPV6_MULTICAST_IF, (const char *)&i, sizeof i);
        } else {
            struct in_addr a;
            if (inet_pton(AF_INET, iface, &a) != 1) {
                return bad_address();
            }
            r = setsockopt((sock_t)fd, IPPROTO_IP, IP_MULTICAST_IF, (const char *)&a, sizeof a);
        }
    } else if (what == 6) {
        /* TCP: Nagle's algorithm off (n != 0) or on (#65). */
#ifndef TCP_NODELAY
#define TCP_NODELAY 1
#endif
        int v = n != 0;
        r = setsockopt((sock_t)fd, IPPROTO_TCP, TCP_NODELAY, (const char *)&v, sizeof v);
    } else if (what == 7) {
        /* TCP keep-alive (#65): n < 0 off, else on, after n idle seconds (0: the system's). */
        int on = n >= 0;
        r = setsockopt((sock_t)fd, SOL_SOCKET, SO_KEEPALIVE, (const char *)&on, sizeof on);
        if (r == 0 && n > 0) {
            int idle = (int)n;
#if defined(__APPLE__)
#ifndef TCP_KEEPALIVE
#define TCP_KEEPALIVE 0x10
#endif
            r = setsockopt((sock_t)fd, IPPROTO_TCP, TCP_KEEPALIVE, (const char *)&idle, sizeof idle);
#elif defined(__linux__)
#ifndef TCP_KEEPIDLE
#define TCP_KEEPIDLE 4
#endif
            r = setsockopt((sock_t)fd, IPPROTO_TCP, TCP_KEEPIDLE, (const char *)&idle, sizeof idle);
#elif defined(TCP_KEEPIDLE)
            r = setsockopt((sock_t)fd, IPPROTO_TCP, TCP_KEEPIDLE, (const char *)&idle, sizeof idle);
#else
            (void)idle;
#endif
        }
    } else {
        return -(int64_t)(K_OTHER | ((int64_t)EINVAL << 8));
    }
    return r == 0 ? 0 : net_err();
}

/* The interpreter's `bind` with `reuseAddress`: a UDP socket on numeric address `ip`, `port`
 * (std's sockets can't set the option before binding). The descriptor, or an error. */
int64_t pico_net_udp_bind_reuse(const char *ip, int64_t port) {
    net_init();
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_DGRAM;
    hints.ai_flags = AI_PASSIVE | AI_NUMERICHOST;
    char serv[16];
    snprintf(serv, sizeof serv, "%d", (int)port);
    if (getaddrinfo(ip, serv, &hints, &res) != 0 || !res) {
        return bad_address();
    }
    int64_t fd = (int64_t)socket(res->ai_family, res->ai_socktype, res->ai_protocol);
    if (fd == SOCK_NONE) {
        freeaddrinfo(res);
        return net_err();
    }
    sock_reuse(fd);
    if (bind((sock_t)fd, res->ai_addr, (socklen_t)res->ai_addrlen) != 0) {
        int64_t e = net_err();
        sock_close(fd);
        freeaddrinfo(res);
        return e;
    }
    freeaddrinfo(res);
    sock_private(fd);
    return fd;
}

/* The machine's interfaces, one line per address: name, family (4/6), address, and flags
 * (u up, l loopback, m multicast), tab-separated. A malloc'd text (the caller frees it). */
char *pico_net_interfaces(void) {
    net_init();
    size_t cap = 1024, len = 0;
    char *out = malloc(cap);
    if (!out) abort();
    out[0] = 0;
#define IFACE_LINE(NAME, FAM, ADDR, FLAGS) \
    do { \
        char line[512]; \
        int k = snprintf(line, sizeof line, "%s\t%d\t%s\t%s\n", NAME, FAM, ADDR, FLAGS); \
        if (k > 0 && (size_t)k < sizeof line) { \
            while (len + (size_t)k + 1 > cap) { \
                cap *= 2; \
                out = realloc(out, cap); \
                if (!out) abort(); \
            } \
            memcpy(out + len, line, (size_t)k + 1); \
            len += (size_t)k; \
        } \
    } while (0)
#if defined(_WIN32)
    ULONG size = 16384;
    IP_ADAPTER_ADDRESSES *list = NULL;
    ULONG e = ERROR_BUFFER_OVERFLOW;
    for (int tries = 0; tries < 3 && e == ERROR_BUFFER_OVERFLOW; tries++) {
        free(list);
        list = malloc(size);
        if (!list) abort();
        e = GetAdaptersAddresses(AF_UNSPEC, GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                                 NULL, list, &size);
    }
    if (e == NO_ERROR) {
        for (IP_ADAPTER_ADDRESSES *a = list; a; a = a->Next) {
            char name[256];
            if (!WideCharToMultiByte(CP_UTF8, 0, a->FriendlyName, -1, name, sizeof name, NULL, NULL)) {
                snprintf(name, sizeof name, "%s", a->AdapterName);
            }
            char flags[4];
            int f = 0;
            if (a->OperStatus == IfOperStatusUp) flags[f++] = 'u';
            if (a->IfType == IF_TYPE_SOFTWARE_LOOPBACK) flags[f++] = 'l';
            if (!(a->Flags & IP_ADAPTER_NO_MULTICAST)) flags[f++] = 'm';
            flags[f] = 0;
            for (IP_ADAPTER_UNICAST_ADDRESS *u = a->FirstUnicastAddress; u; u = u->Next) {
                struct sockaddr *sa = u->Address.lpSockaddr;
                char host[INET6_ADDRSTRLEN];
                if (sa->sa_family == AF_INET) {
                    inet_ntop(AF_INET, &((struct sockaddr_in *)sa)->sin_addr, host, sizeof host);
                    IFACE_LINE(name, 4, host, flags);
                } else if (sa->sa_family == AF_INET6) {
                    inet_ntop(AF_INET6, &((struct sockaddr_in6 *)sa)->sin6_addr, host, sizeof host);
                    IFACE_LINE(name, 6, host, flags);
                }
            }
        }
    }
    free(list);
#else
    struct ifaddrs *list = NULL;
    if (getifaddrs(&list) == 0) {
        for (struct ifaddrs *a = list; a; a = a->ifa_next) {
            if (!a->ifa_addr) continue;
            char flags[4];
            int f = 0;
            if (a->ifa_flags & IFF_UP) flags[f++] = 'u';
            if (a->ifa_flags & IFF_LOOPBACK) flags[f++] = 'l';
            if (a->ifa_flags & IFF_MULTICAST) flags[f++] = 'm';
            flags[f] = 0;
            char host[INET6_ADDRSTRLEN];
            if (a->ifa_addr->sa_family == AF_INET) {
                inet_ntop(AF_INET, &((struct sockaddr_in *)a->ifa_addr)->sin_addr, host, sizeof host);
                IFACE_LINE(a->ifa_name, 4, host, flags);
            } else if (a->ifa_addr->sa_family == AF_INET6) {
                inet_ntop(AF_INET6, &((struct sockaddr_in6 *)a->ifa_addr)->sin6_addr, host, sizeof host);
                IFACE_LINE(a->ifa_name, 6, host, flags);
            }
        }
        freeifaddrs(list);
    }
#endif
#undef IFACE_LINE
    return out;
}

#endif
