/* test_zerocopy.c - Tests for Linux-only zero-copy operations: tee, splice, copy_file_range */
#include "test_utils.h"

#if defined(__linux__)

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_op_called = 0;
static ssize_t g_op_result = -999;

static void op_callback(ssize_t result) {
    g_op_result = result;
    g_op_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_tee_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create two pipes */
    int pipe1[2], pipe2[2];
    ASSERT_EQ(pipe(pipe1), 0, "pipe1 should succeed");
    ASSERT_EQ(pipe(pipe2), 0, "pipe2 should succeed");

    /* Write some data to first pipe */
    const char *data = "Hello, tee!";
    write(pipe1[1], data, strlen(data));

    g_op_called = 0;
    g_op_result = -999;

    /* Tee from pipe1 read end to pipe2 write end */
    lio_tee(lio, pipe1[0], pipe2[1], strlen(data), op_callback);
    tick_until_flag(lio, &g_op_called, 2000);

    ASSERT(g_op_called, "tee callback should be called");
    ASSERT_EQ(g_op_result, (ssize_t)strlen(data), "tee should copy all bytes");

    /* Read from pipe2 to verify */
    char buf[64] = {0};
    ssize_t n = read(pipe2[0], buf, sizeof(buf));
    ASSERT_EQ(n, (ssize_t)strlen(data), "should read tee'd data");
    ASSERT(memcmp(buf, data, strlen(data)) == 0, "data should match");

    /* Original data should still be in pipe1 (tee doesn't consume) */
    memset(buf, 0, sizeof(buf));
    n = read(pipe1[0], buf, sizeof(buf));
    ASSERT_EQ(n, (ssize_t)strlen(data), "original data should still be there");

    close(pipe1[0]); close(pipe1[1]);
    close(pipe2[0]); close(pipe2[1]);
    lio_destroy(lio);
    TEST_PASS("test_tee_basic");
}

static void test_splice_pipe_to_file(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a pipe and temp file */
    int pipefd[2];
    ASSERT_EQ(pipe(pipefd), 0, "pipe should succeed");

    char path[256];
    int file_fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(file_fd, 0, "temp file should succeed");

    /* Write data to pipe */
    const char *data = "Splice this data!";
    write(pipefd[1], data, strlen(data));

    g_op_called = 0;
    g_op_result = -999;

    /* Splice from pipe to file */
    lio_splice(lio, pipefd[0], -1, file_fd, 0, strlen(data), 0, op_callback);
    tick_until_flag(lio, &g_op_called, 2000);

    ASSERT(g_op_called, "splice callback should be called");
    ASSERT_EQ(g_op_result, (ssize_t)strlen(data), "splice should transfer all bytes");

    /* Read file to verify */
    lseek(file_fd, 0, SEEK_SET);
    char buf[64] = {0};
    read(file_fd, buf, sizeof(buf));
    ASSERT(memcmp(buf, data, strlen(data)) == 0, "file data should match");

    close(pipefd[0]); close(pipefd[1]);
    close(file_fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_splice_pipe_to_file");
}

static void test_splice_file_to_pipe(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create temp file with data */
    char path[256];
    int file_fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(file_fd, 0, "temp file should succeed");

    const char *data = "File to pipe splice!";
    write(file_fd, data, strlen(data));
    lseek(file_fd, 0, SEEK_SET);

    /* Create pipe */
    int pipefd[2];
    ASSERT_EQ(pipe(pipefd), 0, "pipe should succeed");

    g_op_called = 0;
    g_op_result = -999;

    /* Splice from file to pipe */
    lio_splice(lio, file_fd, 0, pipefd[1], -1, strlen(data), 0, op_callback);
    tick_until_flag(lio, &g_op_called, 2000);

    ASSERT(g_op_called, "splice callback should be called");
    ASSERT_EQ(g_op_result, (ssize_t)strlen(data), "splice should transfer all bytes");

    /* Read pipe to verify */
    char buf[64] = {0};
    read(pipefd[0], buf, sizeof(buf));
    ASSERT(memcmp(buf, data, strlen(data)) == 0, "pipe data should match");

    close(pipefd[0]); close(pipefd[1]);
    close(file_fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_splice_file_to_pipe");
}

static void test_copy_file_range_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create source file with data */
    char src_path[256];
    int src_fd = create_temp_file(src_path, sizeof(src_path));
    ASSERT_GE(src_fd, 0, "source file should succeed");

    const char *data = "Copy file range test data!";
    write(src_fd, data, strlen(data));

    /* Create destination file */
    char dst_path[256];
    int dst_fd = create_temp_file(dst_path, sizeof(dst_path));
    ASSERT_GE(dst_fd, 0, "dest file should succeed");

    g_op_called = 0;
    g_op_result = -999;

    /* Copy from source to dest */
    lio_copy_file_range(lio, src_fd, 0, dst_fd, 0, strlen(data), op_callback);
    tick_until_flag(lio, &g_op_called, 2000);

    ASSERT(g_op_called, "copy_file_range callback should be called");
    ASSERT_EQ(g_op_result, (ssize_t)strlen(data), "should copy all bytes");

    /* Read dest to verify */
    lseek(dst_fd, 0, SEEK_SET);
    char buf[64] = {0};
    read(dst_fd, buf, sizeof(buf));
    ASSERT(memcmp(buf, data, strlen(data)) == 0, "dest data should match");

    close(src_fd);
    close(dst_fd);
    unlink(src_path);
    unlink(dst_path);
    lio_destroy(lio);
    TEST_PASS("test_copy_file_range_basic");
}

static void test_copy_file_range_with_offset(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create source file */
    char src_path[256];
    int src_fd = create_temp_file(src_path, sizeof(src_path));
    ASSERT_GE(src_fd, 0, "source file should succeed");

    const char *data = "HEADER:PAYLOAD:TRAILER";
    write(src_fd, data, strlen(data));

    /* Create destination file */
    char dst_path[256];
    int dst_fd = create_temp_file(dst_path, sizeof(dst_path));
    ASSERT_GE(dst_fd, 0, "dest file should succeed");

    g_op_called = 0;
    g_op_result = -999;

    /* Copy just "PAYLOAD" (offset 7, len 7) */
    lio_copy_file_range(lio, src_fd, 7, dst_fd, 0, 7, op_callback);
    tick_until_flag(lio, &g_op_called, 2000);

    ASSERT(g_op_called, "copy_file_range callback should be called");
    ASSERT_EQ(g_op_result, 7, "should copy 7 bytes");

    /* Verify dest contains "PAYLOAD" */
    lseek(dst_fd, 0, SEEK_SET);
    char buf[16] = {0};
    read(dst_fd, buf, sizeof(buf));
    ASSERT(memcmp(buf, "PAYLOAD", 7) == 0, "should have copied PAYLOAD");

    close(src_fd);
    close(dst_fd);
    unlink(src_path);
    unlink(dst_path);
    lio_destroy(lio);
    TEST_PASS("test_copy_file_range_with_offset");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Zero-Copy Tests (Linux only) ===\n");

    test_tee_basic();
    test_splice_pipe_to_file();
    test_splice_file_to_pipe();
    test_copy_file_range_basic();
    test_copy_file_range_with_offset();

    printf(GREEN "All zero-copy tests passed\n" RESET);
    return 0;
}

#else /* !__linux__ */

int main(void) {
    printf("=== Zero-Copy Tests (Linux only) ===\n");
    printf(YELLOW "SKIPPED" RESET " - not on Linux\n");
    return 0;
}

#endif /* __linux__ */
