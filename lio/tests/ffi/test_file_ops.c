/* test_file_ops.c - Tests for file operations: close, read_at, write_at, fsync, truncate */
#include "test_utils.h"
#include <sys/stat.h>

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_close_called = 0;
static int g_close_result = -999;

static volatile int g_rw_called = 0;
static int g_rw_result = -999;
static uint8_t *g_rw_buf = NULL;
static size_t g_rw_len = 0;

static volatile int g_fsync_called = 0;
static int g_fsync_result = -999;

static volatile int g_truncate_called = 0;
static int g_truncate_result = -999;

static void close_callback(int result) {
    g_close_result = result;
    g_close_called = 1;
}

static void rw_callback(int result, uint8_t *buf, size_t len) {
    g_rw_result = result;
    g_rw_buf = buf;
    g_rw_len = len;
    g_rw_called = 1;
}

static void fsync_callback(int result) {
    g_fsync_result = result;
    g_fsync_called = 1;
}

static void truncate_callback(int result) {
    g_truncate_result = result;
    g_truncate_called = 1;
}

/* ─── Close Tests ────────────────────────────────────────────────────────── */

static void test_close_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_close_called = 0;
    g_close_result = -999;

    /* fd 999999 doesn't exist — should get EBADF */
    lio_close(lio, 999999, close_callback);
    tick_until_flag(lio, &g_close_called, 1000);

    ASSERT(g_close_called, "close callback should be called");
    ASSERT_LT(g_close_result, 0, "close on invalid fd should return error");

    lio_destroy(lio);
    TEST_PASS("test_close_invalid_fd");
}

static void test_close_valid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file to close */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");

    g_close_called = 0;
    g_close_result = -999;

    lio_close(lio, fd, close_callback);
    tick_until_flag(lio, &g_close_called, 1000);

    ASSERT(g_close_called, "close callback should be called");
    ASSERT_EQ(g_close_result, 0, "close on valid fd should return 0");

    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_close_valid_fd");
}

/* ─── Write Tests ────────────────────────────────────────────────────────── */

static void test_write_at_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");

    g_rw_called = 0;
    g_rw_result = -999;
    g_rw_buf = NULL;

    const char *data = "Hello, FFI!";
    size_t data_len = strlen(data);
    uint8_t *buf = alloc_test_buffer(data_len, 0);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");
    memcpy(buf, data, data_len);

    lio_write_at(lio, fd, buf, data_len, 0, rw_callback);
    tick_until_flag(lio, &g_rw_called, 1000);

    ASSERT(g_rw_called, "write callback should be called");
    ASSERT_EQ(g_rw_result, (int)data_len, "write should return bytes written");
    ASSERT_NOT_NULL(g_rw_buf, "buffer should be returned");
    free(g_rw_buf);

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_write_at_basic");
}

static void test_write_at_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_rw_called = 0;
    g_rw_result = -999;
    g_rw_buf = NULL;

    uint8_t *buf = alloc_test_buffer(16, 'X');
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    lio_write_at(lio, 999999, buf, 16, 0, rw_callback);
    tick_until_flag(lio, &g_rw_called, 1000);

    ASSERT(g_rw_called, "write callback should be called");
    ASSERT_LT(g_rw_result, 0, "write to invalid fd should return error");
    ASSERT_NOT_NULL(g_rw_buf, "buffer should still be returned on error");
    free(g_rw_buf);

    lio_destroy(lio);
    TEST_PASS("test_write_at_invalid_fd");
}

/* ─── Read Tests ─────────────────────────────────────────────────────────── */

static void test_read_at_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create file with known content */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");

    const char *data = "Test data for reading";
    ssize_t written = write(fd, data, strlen(data));
    ASSERT_EQ(written, (ssize_t)strlen(data), "write should succeed");

    /* Reopen for reading */
    close(fd);
    fd = open(path, O_RDONLY);
    ASSERT_GE(fd, 0, "reopen should succeed");

    g_rw_called = 0;
    g_rw_result = -999;
    g_rw_buf = NULL;

    uint8_t *buf = alloc_test_buffer(64, 0);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    lio_read_at(lio, fd, buf, 64, 0, rw_callback);
    tick_until_flag(lio, &g_rw_called, 1000);

    ASSERT(g_rw_called, "read callback should be called");
    ASSERT_EQ(g_rw_result, (int)strlen(data), "read should return bytes read");
    ASSERT_NOT_NULL(g_rw_buf, "buffer should be returned");
    ASSERT(memcmp(g_rw_buf, data, strlen(data)) == 0, "data should match");
    free(g_rw_buf);

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_read_at_basic");
}

static void test_read_at_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_rw_called = 0;
    g_rw_result = -999;
    g_rw_buf = NULL;

    uint8_t *buf = alloc_test_buffer(16, 0);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    lio_read_at(lio, 999999, buf, 16, 0, rw_callback);
    tick_until_flag(lio, &g_rw_called, 1000);

    ASSERT(g_rw_called, "read callback should be called");
    ASSERT_LT(g_rw_result, 0, "read from invalid fd should return error");
    ASSERT_NOT_NULL(g_rw_buf, "buffer should still be returned on error");
    free(g_rw_buf);

    lio_destroy(lio);
    TEST_PASS("test_read_at_invalid_fd");
}

/* ─── Fsync Tests ────────────────────────────────────────────────────────── */

static void test_fsync_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");

    g_fsync_called = 0;
    g_fsync_result = -999;

    lio_fsync(lio, fd, fsync_callback);
    tick_until_flag(lio, &g_fsync_called, 1000);

    ASSERT(g_fsync_called, "fsync callback should be called");
    ASSERT_EQ(g_fsync_result, 0, "fsync should return 0 on success");

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_fsync_basic");
}

static void test_fsync_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_fsync_called = 0;
    g_fsync_result = -999;

    lio_fsync(lio, 999999, fsync_callback);
    tick_until_flag(lio, &g_fsync_called, 1000);

    ASSERT(g_fsync_called, "fsync callback should be called");
    ASSERT_LT(g_fsync_result, 0, "fsync on invalid fd should return error");

    lio_destroy(lio);
    TEST_PASS("test_fsync_invalid_fd");
}

/* ─── Truncate Tests ─────────────────────────────────────────────────────── */

static void test_truncate_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");

    /* Write some data */
    const char *data = "This is test data that will be truncated";
    write(fd, data, strlen(data));

    g_truncate_called = 0;
    g_truncate_result = -999;

    lio_truncate(lio, fd, 10, truncate_callback);
    tick_until_flag(lio, &g_truncate_called, 1000);

    ASSERT(g_truncate_called, "truncate callback should be called");
    ASSERT_EQ(g_truncate_result, 0, "truncate should return 0 on success");

    /* Verify size */
    struct stat st;
    memset(&st, 0, sizeof(st));
    int stat_ret = fstat(fd, &st);
    ASSERT_EQ(stat_ret, 0, "fstat should succeed");
    ASSERT_EQ(st.st_size, 10, "file should be truncated to 10 bytes");

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_truncate_basic");
}

static void test_truncate_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_truncate_called = 0;
    g_truncate_result = -999;

    lio_truncate(lio, 999999, 0, truncate_callback);
    tick_until_flag(lio, &g_truncate_called, 1000);

    ASSERT(g_truncate_called, "truncate callback should be called");
    ASSERT_LT(g_truncate_result, 0, "truncate on invalid fd should return error");

    lio_destroy(lio);
    TEST_PASS("test_truncate_invalid_fd");
}

/* ─── Buffer Ownership Tests ─────────────────────────────────────────────── */

static void test_buffer_ownership_write(void) {
    /* Verify that buffer is returned after write, even on error */
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_rw_called = 0;
    g_rw_buf = NULL;

    /* Allocate buffer with specific pattern */
    uint8_t *buf = alloc_test_buffer(32, 0xAB);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    /* Write to invalid fd - should still return buffer */
    lio_write_at(lio, 999999, buf, 32, 0, rw_callback);
    tick_until_flag(lio, &g_rw_called, 1000);

    ASSERT(g_rw_called, "callback should be called");
    ASSERT_NOT_NULL(g_rw_buf, "buffer must be returned");
    ASSERT_EQ(g_rw_len, 32, "buffer length should be preserved");
    /* Verify buffer contents weren't corrupted */
    ASSERT(verify_buffer_pattern(g_rw_buf, 32, 0xAB), "buffer contents should be preserved");
    free(g_rw_buf);

    lio_destroy(lio);
    TEST_PASS("test_buffer_ownership_write");
}

static void test_buffer_ownership_read(void) {
    /* Verify that buffer is returned after read, even on error */
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_rw_called = 0;
    g_rw_buf = NULL;

    uint8_t *buf = alloc_test_buffer(32, 0xCD);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    /* Read from invalid fd - should still return buffer */
    lio_read_at(lio, 999999, buf, 32, 0, rw_callback);
    tick_until_flag(lio, &g_rw_called, 1000);

    ASSERT(g_rw_called, "callback should be called");
    ASSERT_NOT_NULL(g_rw_buf, "buffer must be returned");
    ASSERT_EQ(g_rw_len, 32, "buffer length should be preserved");
    free(g_rw_buf);

    lio_destroy(lio);
    TEST_PASS("test_buffer_ownership_read");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== File Operation Tests ===\n");

    /* Close tests */
    test_close_invalid_fd();
    test_close_valid_fd();

    /* Write tests */
    test_write_at_basic();
    test_write_at_invalid_fd();

    /* Read tests */
    test_read_at_basic();
    test_read_at_invalid_fd();

    /* Fsync tests */
    test_fsync_basic();
    test_fsync_invalid_fd();

    /* Truncate tests */
    test_truncate_basic();
    test_truncate_invalid_fd();

    /* Buffer ownership tests */
    test_buffer_ownership_write();
    test_buffer_ownership_read();

    printf(GREEN "All file operation tests passed\n" RESET);
    return 0;
}
