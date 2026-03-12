/* test_lifecycle.c - Tests for lio_create, lio_destroy, lio_tick */
#include "test_utils.h"

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_create_destroy(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    lio_destroy(lio);
    TEST_PASS("test_create_destroy");
}

static void test_create_zero_capacity(void) {
    /* Zero capacity might fail or succeed depending on impl */
    lio_handle_t *lio = lio_create(0);
    if (lio) {
        lio_destroy(lio);
    }
    TEST_PASS("test_create_zero_capacity");
}

static void test_destroy_null(void) {
    /* Should not crash */
    lio_destroy(NULL);
    TEST_PASS("test_destroy_null");
}

static void test_tick_empty(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Tick with no pending operations should return 0 */
    int result = lio_tick(lio);
    ASSERT_GE(result, 0, "tick on empty queue should return >= 0");

    lio_destroy(lio);
    TEST_PASS("test_tick_empty");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Lifecycle Tests ===\n");

    test_create_destroy();
    test_create_zero_capacity();
    test_destroy_null();
    test_tick_empty();

    printf(GREEN "All lifecycle tests passed\n" RESET);
    return 0;
}
