/* test_sleep.c - Tests for lio_sleep (sleep/delay operation) */
#include "test_utils.h"
#include <sys/time.h>

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_sleep_called = 0;
static int g_sleep_result = -999;
static volatile int g_sleep_count = 0;

static void sleep_callback(int result) {
    g_sleep_result = result;
    g_sleep_called = 1;
}

static void sleep_count_callback(int result) {
    (void)result;
    g_sleep_count++;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_sleep_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_sleep_called = 0;
    g_sleep_result = -999;

    lio_sleep(lio, 10, sleep_callback); /* 10ms sleep */
    tick_until_flag(lio, &g_sleep_called, 1000);

    ASSERT(g_sleep_called, "sleep callback should be called");
    ASSERT_EQ(g_sleep_result, 0, "sleep should return 0 on success");

    lio_destroy(lio);
    TEST_PASS("test_sleep_basic");
}

static void test_sleep_zero(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_sleep_called = 0;
    g_sleep_result = -999;

    lio_sleep(lio, 0, sleep_callback); /* 0ms sleep - immediate */
    tick_until_flag(lio, &g_sleep_called, 1000);

    ASSERT(g_sleep_called, "zero sleep callback should be called");
    ASSERT_EQ(g_sleep_result, 0, "zero sleep should return 0");

    lio_destroy(lio);
    TEST_PASS("test_sleep_zero");
}

static void test_sleep_multiple(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_sleep_count = 0;

    /* Submit multiple sleeps */
    lio_sleep(lio, 5, sleep_count_callback);
    lio_sleep(lio, 10, sleep_count_callback);
    lio_sleep(lio, 15, sleep_count_callback);

    /* Wait for all to complete */
    for (int i = 0; i < 2000 && g_sleep_count < 3; i++) {
        lio_tick(lio);
        usleep(1000);
    }

    ASSERT_EQ(g_sleep_count, 3, "all three sleeps should complete");

    lio_destroy(lio);
    TEST_PASS("test_sleep_multiple");
}

static void test_sleep_timing(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_sleep_called = 0;
    g_sleep_result = -999;

    struct timeval start, end;
    gettimeofday(&start, NULL);

    lio_sleep(lio, 50, sleep_callback); /* 50ms */
    tick_until_flag(lio, &g_sleep_called, 2000);

    gettimeofday(&end, NULL);

    long elapsed_ms = (end.tv_sec - start.tv_sec) * 1000 +
                      (end.tv_usec - start.tv_usec) / 1000;

    ASSERT(g_sleep_called, "sleep callback should be called");
    /* Allow some slack (25ms to 200ms) - timing isn't precise */
    ASSERT_GE(elapsed_ms, 25, "sleep should wait at least 25ms");
    ASSERT_LT(elapsed_ms, 200, "sleep should complete within 200ms");

    lio_destroy(lio);
    TEST_PASS("test_sleep_timing");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Sleep Tests ===\n");

    test_sleep_basic();
    test_sleep_zero();
    test_sleep_multiple();
    test_sleep_timing();

    printf(GREEN "All sleep tests passed\n" RESET);
    return 0;
}
