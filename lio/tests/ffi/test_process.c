/* test_process.c - Tests for lio_spawn and lio_waitid */
#include "test_utils.h"
#include <sys/wait.h>

/* Wait target types */
#define P_ALL  0
#define P_PID  1
#define P_PGID 2

/* Wait status types from callback */
#define STATUS_EXITED    1
#define STATUS_SIGNALED  2
#define STATUS_STOPPED   3
#define STATUS_CONTINUED 4

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_spawn_called = 0;
static int g_spawn_result = -999;

static volatile int g_waitid_called = 0;
static int g_waitid_result = -999;
static int g_waitid_pid = 0;
static int g_waitid_status = 0;
static int g_waitid_code = 0;

static void spawn_callback(int result) {
    g_spawn_result = result;
    g_spawn_called = 1;
}

static void waitid_callback(int result, int pid, int status, int code) {
    g_waitid_result = result;
    g_waitid_pid = pid;
    g_waitid_status = status;
    g_waitid_code = code;
    g_waitid_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_spawn_and_wait(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_spawn_called = 0;
    g_spawn_result = -999;

    /* Spawn /bin/true which exits immediately with code 0 */
    const char *path = "/usr/bin/true";
    const char *argv[] = {"true", NULL};
    const char *envp[] = {NULL};

    lio_spawn(lio, path, argv, envp, spawn_callback);
    tick_until_flag(lio, &g_spawn_called, 2000);

    ASSERT(g_spawn_called, "spawn callback should be called");
    ASSERT_GE(g_spawn_result, 0, "spawn should return valid PID");

    int child_pid = g_spawn_result;

    /* Wait for child to exit */
    g_waitid_called = 0;
    g_waitid_result = -999;

    lio_waitid(lio, P_PID, child_pid, WEXITED, waitid_callback);
    tick_until_flag(lio, &g_waitid_called, 3000);

    ASSERT(g_waitid_called, "waitid callback should be called");
    ASSERT_EQ(g_waitid_result, 0, "waitid should succeed");
    ASSERT_EQ(g_waitid_pid, child_pid, "waitid should return correct PID");
    ASSERT_EQ(g_waitid_status, STATUS_EXITED, "child should have exited");
    ASSERT_EQ(g_waitid_code, 0, "exit code should be 0");

    lio_destroy(lio);
    TEST_PASS("test_spawn_and_wait");
}

static void test_spawn_with_exit_code(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_spawn_called = 0;
    g_spawn_result = -999;

    /* Spawn /bin/false which exits with code 1 */
    const char *path = "/usr/bin/false";
    const char *argv[] = {"false", NULL};

    lio_spawn(lio, path, argv, NULL, spawn_callback);
    tick_until_flag(lio, &g_spawn_called, 2000);

    ASSERT(g_spawn_called, "spawn callback should be called");
    ASSERT_GE(g_spawn_result, 0, "spawn should return valid PID");

    int child_pid = g_spawn_result;

    /* Wait for child */
    g_waitid_called = 0;

    lio_waitid(lio, P_PID, child_pid, WEXITED, waitid_callback);
    tick_until_flag(lio, &g_waitid_called, 3000);

    ASSERT(g_waitid_called, "waitid callback should be called");
    ASSERT_EQ(g_waitid_result, 0, "waitid should succeed");
    ASSERT_EQ(g_waitid_status, STATUS_EXITED, "child should have exited");
    ASSERT_EQ(g_waitid_code, 1, "exit code should be 1");

    lio_destroy(lio);
    TEST_PASS("test_spawn_with_exit_code");
}

static void test_spawn_nonexistent(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_spawn_called = 0;
    g_spawn_result = 0;

    const char *path = "/nonexistent/path/to/binary";
    const char *argv[] = {"nonexistent", NULL};

    lio_spawn(lio, path, argv, NULL, spawn_callback);
    tick_until_flag(lio, &g_spawn_called, 1000);

    ASSERT(g_spawn_called, "spawn callback should be called");
    ASSERT_LT(g_spawn_result, 0, "spawn of nonexistent should fail");

    lio_destroy(lio);
    TEST_PASS("test_spawn_nonexistent");
}

static void test_waitid_nohang(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_waitid_called = 0;
    g_waitid_result = -999;
    g_waitid_pid = -1;

    /* Wait for any child with WNOHANG - should return immediately with pid=0 */
    lio_waitid(lio, P_ALL, 0, WEXITED | WNOHANG, waitid_callback);
    tick_until_flag(lio, &g_waitid_called, 1000);

    ASSERT(g_waitid_called, "waitid callback should be called");
    /* Either success with no child (pid=0) or ECHILD error */
    if (g_waitid_result == 0) {
        ASSERT_EQ(g_waitid_pid, 0, "WNOHANG with no children should return pid=0");
    }

    lio_destroy(lio);
    TEST_PASS("test_waitid_nohang");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Process Tests ===\n");

    test_spawn_and_wait();
    test_spawn_with_exit_code();
    test_spawn_nonexistent();
    test_waitid_nohang();

    printf(GREEN "All process tests passed\n" RESET);
    return 0;
}
