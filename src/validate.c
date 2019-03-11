#include "validate.h"

#include "log.h"

#include <string.h>

// macOS does not provide memrchr
#if defined(__APPLE__) && defined(__MACH__)
    // macOS specific so only check for clang, for GCC check
    // that the version is greater than 4.5
    #ifdef __clang__
        #pragma GCC diagnostic push
    #endif
    #ifdef __GNUC__
        #pragma GCC diagnostic ignored "-Wcast-qual"
    #endif
    // helper function to cast away const
    static void* cast_away_const(const void* ptr) {
        return (void*)ptr;
    }
    #ifdef __clang__
        #pragma GCC diagnostic pop
    #endif

    static void * memrchr(const void *s, int c_in, size_t n) {
        const unsigned char c = (const unsigned char)c_in;
        const unsigned char *p = (const unsigned char *)s;
        p += n;
        while (n--) {
            if (*--p == c) {
                return (void *)cast_away_const(p);
            }
        }
        return NULL;
    }
#else
    // For some reason this is not in string.h, probably because it was
    // introduced there by a GNU extention:
    //
    // "The memrchr() function  is  a  GNU  extension,  available  since  glibc 2.1.91."
    // (http://manpages.ubuntu.com/manpages/xenial/man3/memchr.3.html)
    //
    extern void *memrchr(const void*, int, size_t);
#endif

static const char * const valid_stat_types[6] = {
    "c",
    "ms",
    "kv",
    "g",
    "h",
    "s"
};
static const int valid_stat_types_len = 6;

static metric_type parse_stat_type(const char *str, size_t len) {
    if (1 <= len && len <= 2) {
        for (int i = 0; i < valid_stat_types_len; i++) {
            const char *stat = valid_stat_types[i];
            // len is bounded, so help the the compiler optimize
            if ((len == 1 ? memcmp(stat, str, 1) : memcmp(stat, str, 2)) == 0) {
                return (metric_type)i;
            }
        }
    }
    return METRIC_UNKNOWN;
}

int validate_statsd(const char *line, size_t len, validate_parsed_result_t* result) {
    const char *start = line;
    size_t plen = len;

    // Search backwards, otherwise might eat up irrelevant data.
    // Example: keyname.__tagname=tag:value:42.0|ms
    //                                      ^^^^--- actual value
    const char *end = memrchr(start, ':', plen);
    if (end == NULL) {
        stats_log("validate: Invalid line \"%.*s\" missing ':'", len, line);
        return 1;
    }
    if ((end - start) < 1) {
        stats_log("validate: Invalid line \"%.*s\" zero length key", len, line);
        return 1;
    }
    start = end + 1;
    plen = len - (start - line);

    char *err;
    result->value = strtod(start, &err);
    if (result->value == 0 && err == start) {
        stats_log("validate: Invalid line \"%.*s\" unable to parse value as double", len, line);
        return 1;
    }

    end = memchr(start, '|', plen);
    if (end == NULL) {
        stats_log("validate: Invalid line \"%.*s\" missing '|'", len, line);
        return 1;
    }
    start = end + 1;
    plen = len - (start - line);

    end = memchr(start, '|', plen);
    if (end != NULL) {
        plen = end - start;
    }

    result->type = parse_stat_type(start, plen);
    if (result->type < 0) {
        stats_log("validate: Invalid line \"%.*s\" unknown stat type \"%.*s\"", len, line, plen, start);
        return 1;
    }

    result->presampling_value = 1.0; /* Default pre-sampling to 1.0 */

    if (end != NULL) {
        // end[0] is currently the second | char
        // test if we have at least 1 char following it (@)
        if ((len - (end - line) > 1) && (end[1] == '@')) {
            start = end + 2;
            plen = len - (start - line);
            if (plen == 0) {
                stats_log("validate: Invalid line \"%.*s\" @ sample with no rate", len, line);
                return 1;
            }
            result->presampling_value = strtod(start, &err);
            if ((result->presampling_value == 0.0) && err == start) {
                stats_log("validate: Invalid line \"%.*s\" invalid sample rate", len, line);
                return 1;
            }
        } else {
            stats_log("validate: Invalid line \"%.*s\" no @ sample rate specifier", len, line);
            return 1;
        }
    }

    return 0;
}
