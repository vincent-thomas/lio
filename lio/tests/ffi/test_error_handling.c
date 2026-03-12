/* test_error_handling.c - Tests for error conditions and edge cases */
#include "test_utils.h"

/* ─── Close error tests ─────────────────────────────────────────────────────── */

static volatile int close_result = -999;
static volatile int close_done = 0;
static void on_close(int result) { close_result = result; close_done = 1; }

static void test_close_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    close_result = -999;
    close_done = 0;
    lio_close(lio, -1, on_close);
    tick_until_flag(lio, &close_done, 1000);

    ASSERT(close_done, "callback should be invoked");
    ASSERT_EQ(close_result, -EBADF, "close(-1) should return EBADF");

    lio_destroy(lio);
    TEST_PASS("test_close_invalid_fd");
}

static void test_close_already_closed(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    /* Create and close a fd */
    int fd = open("/dev/null", O_RDONLY);
    ASSERT_GE(fd, 0, "open /dev/null");

    close_result = -999;
    close_done = 0;
    lio_close(lio, fd, on_close);
    tick_until_flag(lio, &close_done, 1000);
    ASSERT(close_done, "first close callback");
    ASSERT_EQ(close_result, 0, "first close should succeed");

    /* Try to close again */
    close_result = -999;
    close_done = 0;
    lio_close(lio, fd, on_close);
    tick_until_flag(lio, &close_done, 1000);
    ASSERT(close_done, "second close callback");
    ASSERT_EQ(close_result, -EBADF, "double close should return EBADF");

    lio_destroy(lio);
    TEST_PASS("test_close_already_closed");
}

/* ─── Read/Write error tests ────────────────────────────────────────────────── */

static volatile int rw_result = -999;
static volatile int rw_done = 0;
static volatile uint8_t *rw_buf_out = NULL;

static void on_rw(int result, uint8_t *buf, uintptr_t len) {
    rw_result = result;
    rw_buf_out = buf;
    rw_done = 1;
}

static void test_read_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    uint8_t *buf = alloc_test_buffer(64, 0);
    ASSERT_NOT_NULL(buf, "alloc buffer");

    rw_result = -999;
    rw_done = 0;
    rw_buf_out = NULL;
    lio_read_at(lio, -1, buf, 64, 0, on_rw);
    tick_until_flag(lio, &rw_done, 1000);

    ASSERT(rw_done, "callback should be invoked");
    ASSERT_EQ(rw_result, -EBADF, "read on invalid fd should return EBADF");
    ASSERT_NOT_NULL((void*)rw_buf_out, "buffer should be returned");
    free((void*)rw_buf_out);

    lio_destroy(lio);
    TEST_PASS("test_read_invalid_fd");
}

static void test_write_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    uint8_t *buf = alloc_test_buffer(64, 0xAB);
    ASSERT_NOT_NULL(buf, "alloc buffer");

    rw_result = -999;
    rw_done = 0;
    rw_buf_out = NULL;
    lio_write_at(lio, -1, buf, 64, 0, on_rw);
    tick_until_flag(lio, &rw_done, 1000);

    ASSERT(rw_done, "callback should be invoked");
    ASSERT_EQ(rw_result, -EBADF, "write on invalid fd should return EBADF");
    ASSERT_NOT_NULL((void*)rw_buf_out, "buffer should be returned");
    free((void*)rw_buf_out);

    lio_destroy(lio);
    TEST_PASS("test_write_invalid_fd");
}

static void test_read_empty_buffer(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    char path[64];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "create temp file");

    /* Write some data first */
    write(fd, "hello", 5);
    lseek(fd, 0, SEEK_SET);

    /* Read with zero-length buffer */
    uint8_t *buf = (uint8_t*)malloc(1); /* minimal allocation */
    rw_result = -999;
    rw_done = 0;
    lio_read_at(lio, fd, buf, 0, 0, on_rw);
    tick_until_flag(lio, &rw_done, 1000);

    ASSERT(rw_done, "callback should be invoked");
    ASSERT_EQ(rw_result, 0, "zero-length read should return 0");
    free((void*)rw_buf_out);

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_read_empty_buffer");
}

/* ─── Socket error tests ────────────────────────────────────────────────────── */

static volatile intptr_t socket_result = -999;
static volatile int socket_done = 0;
static void on_socket(intptr_t result) { socket_result = result; socket_done = 1; }

static volatile int bind_result = -999;
static volatile int bind_done = 0;
static void on_bind(int result) { bind_result = result; bind_done = 1; }

static void test_bind_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    struct sockaddr_in addr = make_loopback_addr(0);

    bind_result = -999;
    bind_done = 0;
    lio_bind(lio, -1, (struct sockaddr*)&addr, sizeof(addr), on_bind);
    tick_until_flag(lio, &bind_done, 1000);

    ASSERT(bind_done, "callback should be invoked");
    ASSERT_LT(bind_result, 0, "bind on invalid fd should fail");

    lio_destroy(lio);
    TEST_PASS("test_bind_invalid_fd");
}

static void test_bind_already_bound(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    /* Create socket */
    socket_result = -999;
    socket_done = 0;
    lio_socket(lio, AF_INET, SOCK_STREAM, 0, on_socket);
    tick_until_flag(lio, &socket_done, 1000);
    ASSERT(socket_done, "socket callback");
    ASSERT_GE(socket_result, 0, "socket create");
    intptr_t sock = socket_result;

    /* First bind */
    struct sockaddr_in addr = make_loopback_addr(0);
    bind_result = -999;
    bind_done = 0;
    lio_bind(lio, sock, (struct sockaddr*)&addr, sizeof(addr), on_bind);
    tick_until_flag(lio, &bind_done, 1000);
    ASSERT(bind_done, "first bind callback");
    ASSERT_EQ(bind_result, 0, "first bind should succeed");

    /* Get the bound port */
    socklen_t len = sizeof(addr);
    getsockname((int)sock, (struct sockaddr*)&addr, &len);

    /* Try to bind again */
    bind_result = -999;
    bind_done = 0;
    lio_bind(lio, sock, (struct sockaddr*)&addr, sizeof(addr), on_bind);
    tick_until_flag(lio, &bind_done, 1000);
    ASSERT(bind_done, "second bind callback");
    ASSERT_LT(bind_result, 0, "double bind should fail");

    close((int)sock);
    lio_destroy(lio);
    TEST_PASS("test_bind_already_bound");
}

static volatile int listen_result = -999;
static volatile int listen_done = 0;
static void on_listen(int result) { listen_result = result; listen_done = 1; }

static void test_listen_unbound(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    /* Create socket without binding */
    socket_result = -999;
    socket_done = 0;
    lio_socket(lio, AF_INET, SOCK_STREAM, 0, on_socket);
    tick_until_flag(lio, &socket_done, 1000);
    ASSERT(socket_done, "socket callback");
    ASSERT_GE(socket_result, 0, "socket create");
    intptr_t sock = socket_result;

    /* Try to listen without binding - this actually succeeds on most systems
       as the kernel auto-binds. So just verify it doesn't crash. */
    listen_result = -999;
    listen_done = 0;
    lio_listen(lio, sock, 5, on_listen);
    tick_until_flag(lio, &listen_done, 1000);
    ASSERT(listen_done, "listen callback");
    /* Result may be 0 (auto-bind) or error depending on OS */

    close((int)sock);
    lio_destroy(lio);
    TEST_PASS("test_listen_unbound");
}

/* ─── Shutdown error tests ──────────────────────────────────────────────────── */

static volatile int shutdown_result = -999;
static volatile int shutdown_done = 0;
static void on_shutdown(int result) { shutdown_result = result; shutdown_done = 1; }

static void test_shutdown_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    shutdown_result = -999;
    shutdown_done = 0;
    lio_shutdown(lio, -1, SHUT_RDWR, on_shutdown);
    tick_until_flag(lio, &shutdown_done, 1000);

    ASSERT(shutdown_done, "callback should be invoked");
    ASSERT_LT(shutdown_result, 0, "shutdown on invalid fd should fail");

    lio_destroy(lio);
    TEST_PASS("test_shutdown_invalid_fd");
}

static void test_shutdown_not_socket(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    /* Try shutdown on a regular file */
    char path[64];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "create temp file");

    shutdown_result = -999;
    shutdown_done = 0;
    lio_shutdown(lio, fd, SHUT_RDWR, on_shutdown);
    tick_until_flag(lio, &shutdown_done, 1000);

    ASSERT(shutdown_done, "callback should be invoked");
    ASSERT_LT(shutdown_result, 0, "shutdown on file should fail");

    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_shutdown_not_socket");
}

/* ─── Fsync/Truncate error tests ────────────────────────────────────────────── */

static volatile int fsync_result = -999;
static volatile int fsync_done = 0;
static void on_fsync(int result) { fsync_result = result; fsync_done = 1; }

static void test_fsync_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    fsync_result = -999;
    fsync_done = 0;
    lio_fsync(lio, -1, on_fsync);
    tick_until_flag(lio, &fsync_done, 1000);

    ASSERT(fsync_done, "callback should be invoked");
    ASSERT_EQ(fsync_result, -EBADF, "fsync on invalid fd should return EBADF");

    lio_destroy(lio);
    TEST_PASS("test_fsync_invalid_fd");
}

static volatile int truncate_result = -999;
static volatile int truncate_done = 0;
static void on_truncate(int result) { truncate_result = result; truncate_done = 1; }

static void test_truncate_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create");

    truncate_result = -999;
    truncate_done = 0;
    lio_truncate(lio, -1, 100, on_truncate);
    tick_until_flag(lio, &truncate_done, 1000);

    ASSERT(truncate_done, "callback should be invoked");
    ASSERT_EQ(truncate_result, -EBADF, "truncate on invalid fd should return EBADF");

    lio_destroy(lio);
    TEST_PASS("test_truncate_invalid_fd");
}

/* ─── Main ───────────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Error Handling Tests ===\n");

    test_close_invalid_fd();
    test_close_already_closed();
    test_read_invalid_fd();
    test_write_invalid_fd();
    test_read_empty_buffer();
    test_bind_invalid_fd();
    test_bind_already_bound();
    test_listen_unbound();
    test_shutdown_invalid_fd();
    test_shutdown_not_socket();
    test_fsync_invalid_fd();
    test_truncate_invalid_fd();

    printf(GREEN "All error handling tests passed\n" RESET);
    return 0;
}
