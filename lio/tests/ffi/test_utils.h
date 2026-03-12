/* test_utils.h - Shared FFI test utilities */
#ifndef TEST_UTILS_H
#define TEST_UTILS_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

#include <lio.h>

/* ─── Test framework ─────────────────────────────────────────────────────── */

#define TEST_CAPACITY 64

/* Color output */
#define RED     "\033[0;31m"
#define GREEN   "\033[0;32m"
#define YELLOW  "\033[0;33m"
#define RESET   "\033[0m"

/* Assertion macros */
#define ASSERT(cond, msg) do { \
    if (!(cond)) { \
        fprintf(stderr, RED "FAIL" RESET " [%s:%d] %s: %s\n", \
                __FILE__, __LINE__, __func__, msg); \
        exit(1); \
    } \
} while (0)

#define ASSERT_EQ(a, b, msg) do { \
    long _a = (long)(a), _b = (long)(b); \
    if (_a != _b) { \
        fprintf(stderr, RED "FAIL" RESET " [%s:%d] %s: %s (expected %ld, got %ld)\n", \
                __FILE__, __LINE__, __func__, msg, _b, _a); \
        exit(1); \
    } \
} while (0)

#define ASSERT_GE(a, b, msg) do { \
    long _a = (long)(a), _b = (long)(b); \
    if (_a < _b) { \
        fprintf(stderr, RED "FAIL" RESET " [%s:%d] %s: %s (expected >= %ld, got %ld)\n", \
                __FILE__, __LINE__, __func__, msg, _b, _a); \
        exit(1); \
    } \
} while (0)

#define ASSERT_LT(a, b, msg) do { \
    long _a = (long)(a), _b = (long)(b); \
    if (_a >= _b) { \
        fprintf(stderr, RED "FAIL" RESET " [%s:%d] %s: %s (expected < %ld, got %ld)\n", \
                __FILE__, __LINE__, __func__, msg, _b, _a); \
        exit(1); \
    } \
} while (0)

#define ASSERT_NOT_NULL(ptr, msg) ASSERT((ptr) != NULL, msg)
#define ASSERT_NULL(ptr, msg) ASSERT((ptr) == NULL, msg)

#define TEST_PASS(name) printf(GREEN "PASS" RESET " %s\n", name)

/* ─── Event loop helpers ─────────────────────────────────────────────────── */

/* Run tick until at least one completion, with timeout */
static inline int tick_until_complete(lio_handle_t *lio, int max_iterations) {
    for (int i = 0; i < max_iterations; i++) {
        int n = lio_tick(lio);
        if (n > 0) return n;
        if (n < 0) return n;
        usleep(1000); /* 1ms backoff */
    }
    return 0;
}

/* Run tick until a flag is set */
static inline void tick_until_flag(lio_handle_t *lio, volatile int *flag, int max_iterations) {
    for (int i = 0; i < max_iterations && !*flag; i++) {
        lio_tick(lio);
        if (!*flag) usleep(1000);
    }
}

/* ─── Socket helpers ───────────────────────────────────────────────────── */

/* Create a sockaddr_in for localhost with given port */
static inline struct sockaddr_in make_loopback_addr(uint16_t port) {
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return addr;
}

/* Find an available port (bind to 0 and get assigned port) */
static inline uint16_t find_available_port(void) {
    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) return 0;

    struct sockaddr_in addr = make_loopback_addr(0);
    if (bind(sock, (struct sockaddr*)&addr, sizeof(addr)) < 0) {
        close(sock);
        return 0;
    }

    socklen_t len = sizeof(addr);
    if (getsockname(sock, (struct sockaddr*)&addr, &len) < 0) {
        close(sock);
        return 0;
    }

    uint16_t port = ntohs(addr.sin_port);
    close(sock);
    return port;
}

/* ─── Buffer helpers ─────────────────────────────────────────────────────── */

/* Allocate a buffer with test pattern */
static inline uint8_t* alloc_test_buffer(size_t len, uint8_t pattern) {
    uint8_t *buf = (uint8_t*)malloc(len);
    if (buf) memset(buf, pattern, len);
    return buf;
}

/* Verify buffer contains expected pattern */
static inline int verify_buffer_pattern(const uint8_t *buf, size_t len, uint8_t pattern) {
    for (size_t i = 0; i < len; i++) {
        if (buf[i] != pattern) return 0;
    }
    return 1;
}

/* ─── Temp file helpers ───────────────────────────────────────────────────── */

static inline int create_temp_file(char *path_buf, size_t path_len) {
    snprintf(path_buf, path_len, "/tmp/lio_ffi_test_XXXXXX");
    return mkstemp(path_buf);
}

#endif /* TEST_UTILS_H */
