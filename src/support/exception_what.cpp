#include "exception_what.h"

#include <cxxabi.h>
#include <exception>
#include <typeinfo>
#include <stdio.h>
#include <stdlib.h>

extern "C" const char* pg_exception_get_last_query(void);
extern "C" const char* pg_exception_get_last_column(void);

static std::terminate_handler g_prev_terminate_handler = nullptr;

static void pg_terminate_logger() {
    const char *type_name = "unknown";
    char what_buf[257];
    what_buf[0] = '\0';

    std::exception_ptr eptr = std::current_exception();
    if (eptr) {
        try {
            std::rethrow_exception(eptr);
        } catch (const std::exception &e) {
            type_name = typeid(e).name();
            snprintf(what_buf, sizeof(what_buf), "%s", e.what() ? e.what() : "");
        } catch (...) {
            type_name = "non-std::exception";
        }
    }

    int demangle_status = 0;
    char *demangled = __cxxabiv1::__cxa_demangle(type_name, nullptr, nullptr, &demangle_status);
    const char *readable = (demangle_status == 0 && demangled) ? demangled : type_name;

    const char *last_query = pg_exception_get_last_query();
    const char *last_column = pg_exception_get_last_column();
    fprintf(stderr, "[EXC_TERMINATE] type=%s what=%s\n", readable, what_buf);
    if (last_query && last_query[0]) {
        fprintf(stderr, "[EXC_TERMINATE] last_query=%.220s\n", last_query);
    }
    if (last_column && last_column[0]) {
        fprintf(stderr, "[EXC_TERMINATE] last_column=%.220s\n", last_column);
    }
    fflush(stderr);

    if (demangled) {
        free(demangled);
    }

    if (g_prev_terminate_handler) {
        g_prev_terminate_handler();
    }
    abort();
}

void pg_exception_install_terminate_logger(void) {
    g_prev_terminate_handler = std::set_terminate(pg_terminate_logger);
}

extern "C" void* __dynamic_cast(const void *sub,
                                 const std::type_info *src,
                                 const std::type_info *dst,
                                 ptrdiff_t src2dst_offset);

int pg_exception_extract_what(void *thrown_exception,
                              void *tinfo,
                              char *out_buf,
                              size_t out_buf_len) {
    if (!out_buf || out_buf_len == 0) {
        return 0;
    }
    out_buf[0] = '\0';

    if (!thrown_exception || !tinfo) {
        return 0;
    }

    void *exception_obj = __cxxabiv1::__cxa_get_exception_ptr(thrown_exception);
    if (!exception_obj) {
        exception_obj = thrown_exception;
    }

    // Use ABI-level dynamic cast from the concrete thrown type to std::exception.
    const std::type_info *src_type =
        reinterpret_cast<const std::type_info *>(tinfo);
    const std::type_info *dst_type = &typeid(std::exception);

    void *as_std_exception =
        __dynamic_cast(exception_obj, src_type, dst_type, -1);
    if (!as_std_exception) {
        return 0;
    }

    const std::exception *ex =
        reinterpret_cast<const std::exception *>(as_std_exception);

    const char *msg = NULL;
    try {
        msg = ex->what();
    } catch (...) {
        return 0;
    }

    if (!msg || msg[0] == '\0') {
        return 0;
    }

    snprintf(out_buf, out_buf_len, "%s", msg);
    return out_buf[0] != '\0';
}
