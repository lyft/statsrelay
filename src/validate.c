#include "validate.h"

#include "log.h"

#include <string.h>

static char *valid_stat_types[6] = {
    "c",
    "ms",
    "kv",
    "g",
    "h",
    "s"
};
static size_t valid_stat_types_len = 6;

// For some reason this is not in string.h, probably because it was introduced
// there by a GNU extention:
// "The memrchr() function  is  a  GNU  extension,  available  since  glibc 2.1.91."
// (http://manpages.ubuntu.com/manpages/xenial/man3/memchr.3.html)
/* HACK: for OSX debugging */
#ifndef HAS_MEMRCHR
/*
 * Reverse memchr()
 * Find the last occurrence of 'c' in the buffer 's' of size 'n'.
 */
void *
memrchr(s, c, n)
    const void *s;
    int c;
    size_t n;
{
    const unsigned char *cp;

    if (n != 0) {
	cp = (unsigned char *)s + n;
	do {
	    if (*(--cp) == (unsigned char)c)
		return (void *)cp;
	} while (--n != 0);
    }
    return (void *)0;
}
#else
extern void *memrchr(const void*, int, size_t);
#endif

int validate_statsd(const char *line, size_t len, validate_parsed_result_t* result) {
    size_t plen;
    char c;
    int i, valid;

    // FIXME: this is dumb, don't do a memory copy
    char *line_copy = strndup(line, len);
    char *start, *end;
    char *err;

    if (line_copy == NULL) {
        stats_log("validate: allocation failure. marking invalid line");
        goto statsd_err;
    }

    start = line_copy;
    plen = len;
    // Search backwards, otherwise might eat up irrelevant data.
    // Example: keyname.__tagname=tag:value:42.0|ms
    //                                      ^^^^--- actual value
    end = memrchr(start, ':', plen);
    if (end == NULL) {
        stats_log("validate: Invalid line \"%.*s\" missing ':'", len, line);
        goto statsd_err;
    }

    if ((end - start) < 1) {
        stats_log("validate: Invalid line \"%.*s\" zero length key", len, line);
        goto statsd_err;
    }

    start = end + 1;
    plen = len - (start - line_copy);

    c = end[0];
    end[0] = '\0';
    result->presampling_value = 1.0; /* Default pre-sampling to 1.0 */
    result->value = strtod(start, &err);
    if ((result->value == 0.0) && (err == start)) {
        stats_log("validate: Invalid line \"%.*s\" unable to parse value as double", len, line);
        goto statsd_err;
    }
    end[0] = c;

    end = memchr(start, '|', plen);
    if (end == NULL) {
        stats_log("validate: Invalid line \"%.*s\" missing '|'", len, line);
        goto statsd_err;
    }

    start = end + 1;
    plen = len - (start - line_copy);

    end = memchr(start, '|', plen);
    if (end != NULL) {
        c = end[0];
        end[0] = '\0';
        plen = end - start;
    }

    valid = 0;
    for (i = 0; i < valid_stat_types_len; i++) {
        if (strlen(valid_stat_types[i]) != plen) {
            continue;
        }
        if (strncmp(start, valid_stat_types[i], plen) == 0) {
            result->type = (metric_type)i; /* The indexes match the enum values in the comparison list */
            valid = 1;
            break;
        }
    }

    if (valid == 0) {
        stats_log("validate: Invalid line \"%.*s\" unknown stat type \"%.*s\"", len, line, plen, start);
        goto statsd_err;
    }

    if (end != NULL) {
        end[0] = c;
        // end[0] is currently the second | char
        // test if we have at least 1 char following it (@)
        if ((len - (end - line_copy) > 1) && (end[1] == '@')) {
            start = end + 2;
            plen = len - (start - line_copy);
            if (plen == 0) {
                stats_log("validate: Invalid line \"%.*s\" @ sample with no rate", len, line);
                goto statsd_err;
            }
            result->presampling_value = strtod(start, &err);
            if ((result->presampling_value == 0.0) && err == start) {
                stats_log("validate: Invalid line \"%.*s\" invalid sample rate", len, line);
                goto statsd_err;
            }
        } else {
            stats_log("validate: Invalid line \"%.*s\" no @ sample rate specifier", len, line);
            goto statsd_err;
        }
    }

    free(line_copy);
    return 0;

statsd_err:
    free(line_copy);
    return 1;
}
