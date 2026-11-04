/* The bundled IANA tz database (third_party/tzdata, decision 253) for targets without a system
 * one (Windows, WASI), and the system's local zone where it isn't a TZ variable or
 * /etc/localtime. `std/time` asks the system's database first (`__io` 134), then these
 * (`__io` 136, 137 in pico_io.c). Elsewhere both find nothing, so programs don't carry the data.
 *
 * The data is embedded with `#embed` of the packed archive (`PICO_TZDATA_BIN`, its absolute
 * path, defined by the driver); its format is in third_party/tzdata/README.pico.md. */
#if defined(_WIN32)
#include <windows.h>
#endif
#include "pico_rt.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

char *pico_tz_bundled(const char *name, size_t *len);
const char *pico_tz_local_id(void);

#if (defined(_WIN32) || defined(__wasi__)) && defined(PICO_TZDATA_BIN)
static const unsigned char pico_tzdata[] = {
#embed PICO_TZDATA_BIN
};
#define pico_tzdata_end (pico_tzdata + sizeof pico_tzdata)

static uint32_t u32(const unsigned char *p) {
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

/* Zone `name`'s TZif bytes (a malloc'd copy), or NULL. */
char *pico_tz_bundled(const char *name, size_t *len) {
    const unsigned char *p = pico_tzdata, *end = pico_tzdata_end;
    size_t size = (size_t)(end - p), n = strlen(name);
    if (size < 12 || memcmp(p, "PPLTZ\0\1\0", 8) != 0) return NULL;
    uint32_t count = u32(p + 8);
    const unsigned char *e = p + 12;
    for (uint32_t i = 0; i < count; i++) {
        if ((size_t)(end - e) < 2) return NULL;
        size_t nl = (size_t)e[0] | (size_t)e[1] << 8;
        if ((size_t)(end - e) < 2 + nl + 8) return NULL;
        if (nl == n && memcmp(e + 2, name, n) == 0) {
            uint32_t off = u32(e + 2 + nl), l = u32(e + 6 + nl);
            if (off > size || l > size - off) return NULL;
            char *m = malloc(l ? l : 1);
            if (!m) abort();
            memcpy(m, p + off, l);
            *len = l;
            return m;
        }
        e += 2 + nl + 8;
    }
    return NULL;
}

#endif

#if defined(_WIN32) && defined(PICO_TZDATA_BIN)
/* Windows zone names → IANA ids (CLDR windowsZones, sorted by the Windows name). */
static const struct {
    const wchar_t *windows;
    const char *iana;
} windows_zones[] = {
#include "windows_zones.inc"
};

/* The IANA id of Windows' current time zone, or NULL. */
const char *pico_tz_local_id(void) {
    DYNAMIC_TIME_ZONE_INFORMATION d;
    if (GetDynamicTimeZoneInformation(&d) == TIME_ZONE_ID_INVALID) return NULL;
    for (size_t i = 0; i < sizeof windows_zones / sizeof *windows_zones; i++)
        if (wcscmp(windows_zones[i].windows, d.TimeZoneKeyName) == 0) return windows_zones[i].iana;
    return NULL;
}
#else
const char *pico_tz_local_id(void) { return NULL; }
#endif

#if !((defined(_WIN32) || defined(__wasi__)) && defined(PICO_TZDATA_BIN))
char *pico_tz_bundled(const char *name, size_t *len) {
    (void)name, (void)len;
    return NULL;
}
#endif
