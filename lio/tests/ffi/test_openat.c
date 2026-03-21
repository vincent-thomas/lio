/* test_openat.c - Tests for lio_openat, lio_read, lio_write */
#include "test_utils.h"

#ifndef AT_FDCWD
#define AT_FDCWD -100
#endif

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_openat_called = 0;
static intptr_t g_openat_result = -999;

static volatile int g_read_called = 0;
static int g_read_result = -999;
static uint8_t *g_read_buf = NULL;
static size_t g_read_len = 0;

static volatile int g_write_called = 0;
static int g_write_result = -999;
static uint8_t *g_write_buf = NULL;
static size_t g_write_len = 0;

static void openat_callback(intptr_t result) {
    g_openat_result = result;
    g_openat_called = 1;
}

static void read_callback(int result, uint8_t *buf, size_t len) {
    g_read_result = result;
    g_read_buf = buf;
    g_read_len = len;
    g_read_called = 1;
}

static void write_callback(int result, uint8_t *buf, size_t len) {
    g_write_result = result;
    g_write_buf = buf;
    g_write_len = len;
    g_write_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_openat_read_file(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_openat_called = 0;
    g_openat_result = -999;

    /* Open /dev/null for reading */
    lio_openat(lio, AT_FDCWD, "/dev/null", O_RDONLY, 0, openat_callback);
    tick_until_flag(lio, &g_openat_called, 1000);

    ASSERT(g_openat_called, "openat callback should be called");
    ASSERT_GE(g_openat_result, 0, "openat /dev/null should succeed");

    close((int)g_openat_result);
    lio_destroy(lio);
    TEST_PASS("test_openat_read_file");
}

static void test_openat_nonexistent(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_openat_called = 0;
    g_openat_result = 0;

    lio_openat(lio, AT_FDCWD, "/nonexistent/path/to/file", O_RDONLY, 0, openat_callback);
    tick_until_flag(lio, &g_openat_called, 1000);

    ASSERT(g_openat_called, "openat callback should be called");
    ASSERT_LT(g_openat_result, 0, "openat nonexistent should fail");

    lio_destroy(lio);
    TEST_PASS("test_openat_nonexistent");
}

static void test_openat_create_write_read(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    char path[256];
    snprintf(path, sizeof(path), "/tmp/lio_ffi_openat_test_%d", getpid());

    /* Create and open file for writing */
    g_openat_called = 0;
    lio_openat(lio, AT_FDCWD, path, O_CREAT | O_WRONLY | O_TRUNC, 0644, openat_callback);
    tick_until_flag(lio, &g_openat_called, 1000);
    ASSERT(g_openat_called && g_openat_result >= 0, "create file should succeed");
    int write_fd = (int)g_openat_result;

    /* Write data */
    const char *test_data = "Hello from openat!";
    size_t data_len = strlen(test_data);
    uint8_t *wbuf = (uint8_t*)malloc(data_len);
    memcpy(wbuf, test_data, data_len);

    g_write_called = 0;
    g_write_result = -999;

    lio_write(lio, write_fd, wbuf, data_len, write_callback);
    tick_until_flag(lio, &g_write_called, 1000);

    ASSERT(g_write_called, "write callback should be called");
    ASSERT_EQ(g_write_result, (int)data_len, "write should write all bytes");
    free(g_write_buf);
    close(write_fd);

    /* Open for reading */
    g_openat_called = 0;
    lio_openat(lio, AT_FDCWD, path, O_RDONLY, 0, openat_callback);
    tick_until_flag(lio, &g_openat_called, 1000);
    ASSERT(g_openat_called && g_openat_result >= 0, "open for read should succeed");
    int read_fd = (int)g_openat_result;

    /* Read data */
    uint8_t *rbuf = (uint8_t*)malloc(data_len + 1);
    memset(rbuf, 0, data_len + 1);

    g_read_called = 0;
    g_read_result = -999;

    lio_read(lio, read_fd, rbuf, data_len, read_callback);
    tick_until_flag(lio, &g_read_called, 1000);

    ASSERT(g_read_called, "read callback should be called");
    ASSERT_EQ(g_read_result, (int)data_len, "read should read all bytes");
    ASSERT(memcmp(g_read_buf, test_data, data_len) == 0, "read data should match written data");

    free(g_read_buf);
    close(read_fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_openat_create_write_read");
}

static void test_read_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    uint8_t *buf = (uint8_t*)malloc(64);
    ASSERT_NOT_NULL(buf, "buffer alloc should succeed");

    g_read_called = 0;
    g_read_result = 0;

    lio_read(lio, 999999, buf, 64, read_callback);
    tick_until_flag(lio, &g_read_called, 1000);

    ASSERT(g_read_called, "read callback should be called");
    ASSERT_LT(g_read_result, 0, "read on invalid fd should fail");

    free(g_read_buf);
    lio_destroy(lio);
    TEST_PASS("test_read_invalid_fd");
}

static void test_write_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    uint8_t *buf = alloc_test_buffer(32, 0xAA);
    ASSERT_NOT_NULL(buf, "buffer alloc should succeed");

    g_write_called = 0;
    g_write_result = 0;

    lio_write(lio, 999999, buf, 32, write_callback);
    tick_until_flag(lio, &g_write_called, 1000);

    ASSERT(g_write_called, "write callback should be called");
    ASSERT_LT(g_write_result, 0, "write on invalid fd should fail");

    free(g_write_buf);
    lio_destroy(lio);
    TEST_PASS("test_write_invalid_fd");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Openat/Read/Write Tests ===\n");

    test_openat_read_file();
    test_openat_nonexistent();
    test_openat_create_write_read();
    test_read_invalid_fd();
    test_write_invalid_fd();

    printf(GREEN "All openat/read/write tests passed\n" RESET);
    return 0;
}
