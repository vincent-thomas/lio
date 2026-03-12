#include <assert.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <lio.h>

static int tests_passed = 0;
static int tests_total = 0;

#define TEST(name) do { \
    tests_total++; \
    printf("  test: %s... ", name); \
} while(0)

#define PASS() do { \
    tests_passed++; \
    printf("ok\n"); \
} while(0)

#define FAIL(msg) do { \
    printf("FAILED: %s\n", msg); \
    exit(1); \
} while(0)

/* Test 1: Timeout */
static int timeout_called = 0;
static void on_timeout(int result) {
    if (result == 0) timeout_called = 1;
}

static void test_timeout(lio_handle_t *lio) {
    TEST("timeout");

    timeout_called = 0;
    lio_timeout(lio, 1, on_timeout);

    for (int i = 0; i < 100 && !timeout_called; i++) {
        lio_tick(lio);
        usleep(1000);
    }

    if (!timeout_called) FAIL("callback not invoked");
    PASS();
}

/* Test 2: File write/read */
static int write_result = -999;
static int read_result = -999;
static uint8_t *read_buf_out = NULL;

static void on_write(int result, uint8_t *buf, uintptr_t len) {
    write_result = result;
    free(buf);
}

static void on_read(int result, uint8_t *buf, uintptr_t len) {
    read_result = result;
    read_buf_out = buf;
}

static void test_file_io(lio_handle_t *lio) {
    TEST("file write/read");

    char path[] = "/tmp/lio_test_XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) FAIL("mkstemp failed");

    /* Write */
    const char *msg = "hello lio";
    size_t msg_len = strlen(msg);
    uint8_t *wbuf = malloc(msg_len);
    memcpy(wbuf, msg, msg_len);

    write_result = -999;
    lio_write_at(lio, fd, wbuf, msg_len, 0, on_write);

    for (int i = 0; i < 100 && write_result == -999; i++) {
        lio_tick(lio);
        usleep(1000);
    }

    if (write_result != (int)msg_len) {
        unlink(path);
        close(fd);
        FAIL("write failed");
    }

    /* Read back */
    uint8_t *rbuf = malloc(msg_len);
    memset(rbuf, 0, msg_len);

    read_result = -999;
    read_buf_out = NULL;
    lio_read_at(lio, fd, rbuf, msg_len, 0, on_read);

    for (int i = 0; i < 100 && read_result == -999; i++) {
        lio_tick(lio);
        usleep(1000);
    }

    if (read_result != (int)msg_len) {
        unlink(path);
        close(fd);
        FAIL("read failed");
    }

    if (memcmp(read_buf_out, msg, msg_len) != 0) {
        free(read_buf_out);
        unlink(path);
        close(fd);
        FAIL("data mismatch");
    }

    free(read_buf_out);
    unlink(path);
    close(fd);
    PASS();
}

/* Test 3: Socket create */
static intptr_t socket_result = -999;

static void on_socket(intptr_t result) {
    socket_result = result;
}

static void test_socket(lio_handle_t *lio) {
    TEST("socket create");

    socket_result = -999;
    lio_socket(lio, AF_INET, SOCK_STREAM, 0, on_socket);

    for (int i = 0; i < 100 && socket_result == -999; i++) {
        lio_tick(lio);
        usleep(1000);
    }

    if (socket_result < 0) FAIL("socket creation failed");

    close((int)socket_result);
    PASS();
}

int main(void) {
    printf("lio FFI test (via pkg-config)\n");

    lio_handle_t *lio = lio_create(64);
    assert(lio != NULL && "lio_create failed");

    test_timeout(lio);
    test_file_io(lio);
    test_socket(lio);

    lio_destroy(lio);

    printf("\n%d/%d tests passed\n", tests_passed, tests_total);
    return tests_passed == tests_total ? 0 : 1;
}
