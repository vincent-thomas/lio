/* test_timeout.c - Tests for lio_timeout */
#include "test_utils.h"
#include <sys/time.h>

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_timeout_called = 0;
static int g_timeout_result = -999;
static volatile int g_timeout_count = 0;

static void timeout_callback(int result) {
    g_timeout_result = result;
    g_timeout_called = 1;
}

static void timeout_count_callback(int result) {
    (void)result;
    g_timeout_count++;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_timeout_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_timeout_called = 0;
    g_timeout_result = -999;

    lio_timeout(lio, 10, timeout_callback); /* 10ms timeout */
    tick_until_flag(lio, &g_timeout_called, 1000);

    ASSERT(g_timeout_called, "timeout callback should be called");
    ASSERT_EQ(g_timeout_result, 0, "timeout should return 0 on success");

    lio_destroy(lio);
    TEST_PASS("test_timeout_basic");
}

static void test_timeout_zero(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_timeout_called = 0;
    g_timeout_result = -999;

    lio_timeout(lio, 0, timeout_callback); /* 0ms timeout - immediate */
    tick_until_flag(lio, &g_timeout_called, 1000);

    ASSERT(g_timeout_called, "zero timeout callback should be called");
    ASSERT_EQ(g_timeout_result, 0, "zero timeout should return 0");

    lio_destroy(lio);
    TEST_PASS("test_timeout_zero");
}

static void test_timeout_multiple(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_timeout_count = 0;

    /* Submit multiple timeouts */
    lio_timeout(lio, 5, timeout_count_callback);
    lio_timeout(lio, 10, timeout_count_callback);
    lio_timeout(lio, 15, timeout_count_callback);

    /* Wait for all to complete */
    for (int i = 0; i < 2000 && g_timeout_count < 3; i++) {
        lio_tick(lio);
        usleep(1000);
    }

    ASSERT_EQ(g_timeout_count, 3, "all three timeouts should complete");

    lio_destroy(lio);
    TEST_PASS("test_timeout_multiple");
}

static void test_timeout_timing(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_timeout_called = 0;
    g_timeout_result = -999;

    struct timeval start, end;
    gettimeofday(&start, NULL);

    lio_timeout(lio, 50, timeout_callback); /* 50ms */
    tick_until_flag(lio, &g_timeout_called, 2000);

    gettimeofday(&end, NULL);

    long elapsed_ms = (end.tv_sec - start.tv_sec) * 1000 +
                      (end.tv_usec - start.tv_usec) / 1000;

    ASSERT(g_timeout_called, "timeout callback should be called");
    /* Allow some slack (25ms to 200ms) - timing isn't precise */
    ASSERT_GE(elapsed_ms, 25, "timeout should wait at least 25ms");
    ASSERT_LT(elapsed_ms, 200, "timeout should complete within 200ms");

    lio_destroy(lio);
    TEST_PASS("test_timeout_timing");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Timeout Tests ===\n");

    test_timeout_basic();
    test_timeout_zero();
    test_timeout_multiple();
    test_timeout_timing();

    printf(GREEN "All timeout tests passed\n" RESET);
    return 0;
}
