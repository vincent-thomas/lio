/* test_watch.c - Tests for lio_watch (file system watching) */
#include "test_utils.h"
#include <sys/stat.h>

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_watch_called = 0;
static int g_watch_result = -999;

static void watch_callback(int result) {
    g_watch_result = result;
    g_watch_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_watch_modify(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file should succeed");
    close(fd);

    g_watch_called = 0;
    g_watch_result = -999;

    /* Start watching for modifications */
    lio_watch(lio, path, WATCH_MODIFY, watch_callback);

    /* Modify the file */
    fd = open(path, O_WRONLY | O_APPEND);
    write(fd, "test", 4);
    close(fd);

    /* Wait for watch event */
    tick_until_flag(lio, &g_watch_called, 2000);

    ASSERT(g_watch_called, "watch callback should be called");
    ASSERT_GE(g_watch_result, 0, "watch should succeed");
    ASSERT(g_watch_result & WATCH_MODIFY, "should detect MODIFY event");

    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_watch_modify");
}

static void test_watch_delete(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file should succeed");
    close(fd);

    g_watch_called = 0;
    g_watch_result = -999;

    /* Start watching for deletion */
    lio_watch(lio, path, WATCH_DELETE, watch_callback);

    /* Delete the file */
    unlink(path);

    /* Wait for watch event */
    tick_until_flag(lio, &g_watch_called, 2000);

    ASSERT(g_watch_called, "watch callback should be called");
    ASSERT_GE(g_watch_result, 0, "watch should succeed");
    ASSERT(g_watch_result & WATCH_DELETE, "should detect DELETE event");

    lio_destroy(lio);
    TEST_PASS("test_watch_delete");
}

static void test_watch_nonexistent(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_watch_called = 0;
    g_watch_result = 0;

    lio_watch(lio, "/nonexistent/path/to/file", WATCH_MODIFY, watch_callback);
    tick_until_flag(lio, &g_watch_called, 1000);

    ASSERT(g_watch_called, "watch callback should be called");
    ASSERT_LT(g_watch_result, 0, "watch on nonexistent should fail");

    lio_destroy(lio);
    TEST_PASS("test_watch_nonexistent");
}

static void test_watch_attrib(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file should succeed");
    close(fd);

    g_watch_called = 0;
    g_watch_result = -999;

    /* Start watching for attribute changes */
    lio_watch(lio, path, WATCH_ATTRIB, watch_callback);

    /* Change file permissions */
    chmod(path, 0600);

    /* Wait for watch event */
    tick_until_flag(lio, &g_watch_called, 2000);

    ASSERT(g_watch_called, "watch callback should be called");
    ASSERT_GE(g_watch_result, 0, "watch should succeed");
    ASSERT(g_watch_result & WATCH_ATTRIB, "should detect ATTRIB event");

    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_watch_attrib");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Watch Tests ===\n");

    test_watch_modify();
    test_watch_delete();
    test_watch_nonexistent();
    test_watch_attrib();

    printf(GREEN "All watch tests passed\n" RESET);
    return 0;
}
