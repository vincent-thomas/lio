/* test_nop.c - Tests for lio_nop (no-op operation) */
#include "test_utils.h"

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_nop_called = 0;
static int g_nop_result = -999;
static volatile int g_nop_count = 0;

static void nop_callback(int result) {
    g_nop_result = result;
    g_nop_called = 1;
}

static void nop_count_callback(int result) {
    (void)result;
    g_nop_count++;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_nop_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_nop_called = 0;
    g_nop_result = -999;

    lio_nop(lio, nop_callback);
    tick_until_flag(lio, &g_nop_called, 1000);

    ASSERT(g_nop_called, "nop callback should be called");
    ASSERT_EQ(g_nop_result, 0, "nop should return 0");

    lio_destroy(lio);
    TEST_PASS("test_nop_basic");
}

static void test_nop_multiple(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_nop_count = 0;

    /* Submit multiple nops */
    lio_nop(lio, nop_count_callback);
    lio_nop(lio, nop_count_callback);
    lio_nop(lio, nop_count_callback);
    lio_nop(lio, nop_count_callback);
    lio_nop(lio, nop_count_callback);

    /* Wait for all to complete */
    for (int i = 0; i < 1000 && g_nop_count < 5; i++) {
        lio_tick(lio);
        usleep(1000);
    }

    ASSERT_EQ(g_nop_count, 5, "all five nops should complete");

    lio_destroy(lio);
    TEST_PASS("test_nop_multiple");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Nop Tests ===\n");

    test_nop_basic();
    test_nop_multiple();

    printf(GREEN "All nop tests passed\n" RESET);
    return 0;
}
