#ifndef EXCEPTION_WHAT_H
#define EXCEPTION_WHAT_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// Try to extract std::exception::what() from a thrown C++ exception object.
// Returns 1 when a message was extracted into out_buf, 0 otherwise.
int pg_exception_extract_what(void *thrown_exception,
                              void *tinfo,
                              char *out_buf,
                              size_t out_buf_len);

// Install a terminate() logger that prints exception type + what().
void pg_exception_install_terminate_logger(void);

#ifdef __cplusplus
}
#endif

#endif
