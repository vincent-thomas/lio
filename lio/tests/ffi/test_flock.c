/* test_flock.c - Tests for lio_flock (advisory file locking) */
#include "test_utils.h"
#include <sys/file.h>

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_flock_called = 0;
static int g_flock_result = -999;

static void flock_callback(int result) {
    g_flock_result = result;
    g_flock_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_flock_exclusive(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create temp file */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");

    /* Acquire exclusive lock */
    g_flock_called = 0;
    g_flock_result = -999;

    lio_flock(lio, fd, LOCK_EX, flock_callback);
    tick_until_flag(lio, &g_flock_called, 1000);

    ASSERT(g_flock_called, "flock callback should be called");
    ASSERT_EQ(g_flock_result, 0, "exclusive lock should succeed");

    /* Release lock */
    g_flock_called = 0;
    g_flock_result = -999;

    lio_flock(lio, fd, LOCK_UN, flock_callback);
    tick_until_flag(lio, &g_flock_called, 1000);

    ASSERT(g_flock_called, "unlock callback should be called");
    ASSERT_EQ(g_flock_result, 0, "unlock should succeed");

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_flock_exclusive");
}

static void test_flock_shared(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create temp file */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");

    /* Acquire shared lock */
    g_flock_called = 0;
    g_flock_result = -999;

    lio_flock(lio, fd, LOCK_SH, flock_callback);
    tick_until_flag(lio, &g_flock_called, 1000);

    ASSERT(g_flock_called, "flock callback should be called");
    ASSERT_EQ(g_flock_result, 0, "shared lock should succeed");

    /* Release lock */
    g_flock_called = 0;
    lio_flock(lio, fd, LOCK_UN, flock_callback);
    tick_until_flag(lio, &g_flock_called, 1000);
    ASSERT_EQ(g_flock_result, 0, "unlock should succeed");

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_flock_shared");
}

static void test_flock_nonblocking(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create temp file */
    char path[256];
    int fd1 = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd1, 0, "temp file creation should succeed");

    /* Open the same file again */
    int fd2 = open(path, O_RDWR);
    ASSERT_GE(fd2, 0, "second open should succeed");

    /* Acquire exclusive lock on fd1 (blocking) */
    int ret = flock(fd1, LOCK_EX);
    ASSERT_EQ(ret, 0, "flock on fd1 should succeed");

    /* Try non-blocking exclusive lock on fd2 via FFI */
    g_flock_called = 0;
    g_flock_result = 0;

    lio_flock(lio, fd2, LOCK_EX | LOCK_NB, flock_callback);
    tick_until_flag(lio, &g_flock_called, 1000);

    ASSERT(g_flock_called, "flock callback should be called");
    /* Should fail with EWOULDBLOCK/EAGAIN */
    ASSERT_LT(g_flock_result, 0, "non-blocking lock should fail when already locked");

    /* Release first lock */
    flock(fd1, LOCK_UN);

    close(fd1);
    close(fd2);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_flock_nonblocking");
}

static void test_flock_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_flock_called = 0;
    g_flock_result = 0;

    lio_flock(lio, 999999, LOCK_EX, flock_callback);
    tick_until_flag(lio, &g_flock_called, 1000);

    ASSERT(g_flock_called, "flock callback should be called");
    ASSERT_LT(g_flock_result, 0, "flock on invalid fd should fail");

    lio_destroy(lio);
    TEST_PASS("test_flock_invalid_fd");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Flock Tests ===\n");

    test_flock_exclusive();
    test_flock_shared();
    test_flock_nonblocking();
    test_flock_invalid_fd();

    printf(GREEN "All flock tests passed\n" RESET);
    return 0;
}
