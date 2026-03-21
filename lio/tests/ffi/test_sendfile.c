/* test_sendfile.c - Tests for lio_sendfile */
#include "test_utils.h"
#include <sys/stat.h>

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_sendfile_called = 0;
static ssize_t g_sendfile_result = -999;

static void sendfile_callback(ssize_t result) {
    g_sendfile_result = result;
    g_sendfile_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_sendfile_to_socket(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file with some content */
    char path[256];
    int file_fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(file_fd, 0, "temp file creation should succeed");

    const char *test_data = "Hello, sendfile! This is test data for the sendfile operation.";
    size_t data_len = strlen(test_data);
    write(file_fd, test_data, data_len);
    lseek(file_fd, 0, SEEK_SET);

    /* Create a socket pair for testing */
    int sv[2];
    int ret = socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    ASSERT_EQ(ret, 0, "socketpair should succeed");

    g_sendfile_called = 0;
    g_sendfile_result = -999;

    /* Send file to socket */
    lio_sendfile(lio, sv[0], file_fd, 0, data_len, sendfile_callback);
    tick_until_flag(lio, &g_sendfile_called, 2000);

    ASSERT(g_sendfile_called, "sendfile callback should be called");
    ASSERT_EQ(g_sendfile_result, (ssize_t)data_len, "sendfile should send all bytes");

    /* Read from the other end to verify */
    char buf[256] = {0};
    ssize_t n = read(sv[1], buf, sizeof(buf));
    ASSERT_EQ(n, (ssize_t)data_len, "should read all sent bytes");
    ASSERT(memcmp(buf, test_data, data_len) == 0, "data should match");

    /* Clean up */
    close(sv[0]);
    close(sv[1]);
    close(file_fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_sendfile_to_socket");
}

static void test_sendfile_with_offset(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file with some content */
    char path[256];
    int file_fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(file_fd, 0, "temp file creation should succeed");

    const char *test_data = "HEADER:PAYLOAD:TRAILER";
    write(file_fd, test_data, strlen(test_data));

    /* Create a socket pair */
    int sv[2];
    int ret = socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    ASSERT_EQ(ret, 0, "socketpair should succeed");

    g_sendfile_called = 0;
    g_sendfile_result = -999;

    /* Send just "PAYLOAD" (offset 7, len 7) */
    lio_sendfile(lio, sv[0], file_fd, 7, 7, sendfile_callback);
    tick_until_flag(lio, &g_sendfile_called, 2000);

    ASSERT(g_sendfile_called, "sendfile callback should be called");
    ASSERT_EQ(g_sendfile_result, 7, "sendfile should send 7 bytes");

    /* Read and verify */
    char buf[16] = {0};
    read(sv[1], buf, sizeof(buf));
    ASSERT(memcmp(buf, "PAYLOAD", 7) == 0, "should receive PAYLOAD");

    /* Clean up */
    close(sv[0]);
    close(sv[1]);
    close(file_fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_sendfile_with_offset");
}

static void test_sendfile_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_sendfile_called = 0;
    g_sendfile_result = 0;

    lio_sendfile(lio, 999999, 999998, 0, 100, sendfile_callback);
    tick_until_flag(lio, &g_sendfile_called, 1000);

    ASSERT(g_sendfile_called, "sendfile callback should be called");
    ASSERT_LT(g_sendfile_result, 0, "sendfile on invalid fd should fail");

    lio_destroy(lio);
    TEST_PASS("test_sendfile_invalid_fd");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Sendfile Tests ===\n");

    test_sendfile_to_socket();
    test_sendfile_with_offset();
    test_sendfile_invalid_fd();

    printf(GREEN "All sendfile tests passed\n" RESET);
    return 0;
}
