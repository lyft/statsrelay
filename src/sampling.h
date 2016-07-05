#ifndef STATSRELAY_SAMPLING_H
#define STATSRELAY_SAMPLING_H

#include <stdbool.h>
#include "protocol.h"
#include "validate.h"

typedef struct sampler sampler_t;

typedef enum {
	SAMPLER_NOT_SAMPLING = 0,
	SAMPLER_SAMPLING = 1
} sampling_result;

typedef void(sampler_flush_cb)(void* data, const char* key, const char* line, int len);


int sampler_init(sampler_t** sampler, int threshold, int window);

/**
 * Consider a statsd metric for sampling - based on its name and validation
 * parsed result which includes its data object.
 */
sampling_result sampler_consider_metric(sampler_t* sampler, const char* name, validate_parsed_result_t*);

/**
 * Walk through all keys in the sampler and update the sampling or not sampling flag,
 * but do not flush any data.
 */
void sampler_update_flags(sampler_t* sampler);

/**
 * Walk through all keys in the sampler and invoke cb() on them, passing that callback
 * the data pointer.
 *
 * This will also internally call sampler_update_flags on each item.
 */
void sampler_flush(sampler_t* sampler, sampler_flush_cb cb, void* data);

/*
 * Introspect if a key (of any type) is in sampling mode or not.
 * This is mainly used for unit tests
 */
sampling_result sampler_is_sampling(sampler_t* sampler, const char* name);

/*
 * Get the sampling window
 */
int sampler_window(sampler_t* sampler);

/**
 * Destroy the sampler
 */
void sampler_destroy(sampler_t* sampler);

#endif //STATSRELAY_SAMPLING_H