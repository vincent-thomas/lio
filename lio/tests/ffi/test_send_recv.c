/* test_send_recv.c - Tests for send/recv operations with buffer ownership */
#include "test_utils.h"

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_send_called = 0;
static int g_send_result = -999;
static uint8_t *g_send_buf = NULL;
static size_t g_send_len = 0;

static volatile int g_recv_called = 0;
static int g_recv_result = -999;
static uint8_t *g_recv_buf = NULL;
static size_t g_recv_len = 0;

static void send_callback(int result, uint8_t *buf, size_t len) {
    g_send_result = result;
    g_send_buf = buf;
    g_send_len = len;
    g_send_called = 1;
}

static void recv_callback(int result, uint8_t *buf, size_t len) {
    g_recv_result = result;
    g_recv_buf = buf;
    g_recv_len = len;
    g_recv_called = 1;
}

/* ─── Helper: Create connected socket pair ──────────────────────────────── */

static int create_socket_pair(int *server_fd, int *client_fd, int *accepted_fd) {
    /* Create server socket */
    *server_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (*server_fd < 0) return -1;

    int opt = 1;
    setsockopt(*server_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr = make_loopback_addr(0);
    if (bind(*server_fd, (struct sockaddr*)&addr, sizeof(addr)) < 0) {
        close(*server_fd);
        return -1;
    }

    socklen_t addr_len = sizeof(addr);
    getsockname(*server_fd, (struct sockaddr*)&addr, &addr_len);

    if (listen(*server_fd, 1) < 0) {
        close(*server_fd);
        return -1;
    }

    /* Create and connect client */
    *client_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (*client_fd < 0) {
        close(*server_fd);
        return -1;
    }

    if (connect(*client_fd, (struct sockaddr*)&addr, sizeof(addr)) < 0) {
        close(*server_fd);
        close(*client_fd);
        return -1;
    }

    /* Accept connection */
    *accepted_fd = accept(*server_fd, NULL, NULL);
    if (*accepted_fd < 0) {
        close(*server_fd);
        close(*client_fd);
        return -1;
    }

    return 0;
}

/* ─── Send Tests ─────────────────────────────────────────────────────────── */

static void test_send_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    int server_fd, client_fd, accepted_fd;
    int ret = create_socket_pair(&server_fd, &client_fd, &accepted_fd);
    ASSERT_EQ(ret, 0, "socket pair creation should succeed");

    g_send_called = 0;
    g_send_result = -999;
    g_send_buf = NULL;

    const char *msg = "Hello from FFI!";
    size_t msg_len = strlen(msg);
    uint8_t *buf = alloc_test_buffer(msg_len, 0);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");
    memcpy(buf, msg, msg_len);

    lio_send(lio, client_fd, buf, msg_len, 0, send_callback);
    tick_until_flag(lio, &g_send_called, 1000);

    ASSERT(g_send_called, "send callback should be called");
    ASSERT_EQ(g_send_result, (int)msg_len, "send should return bytes sent");
    ASSERT_NOT_NULL(g_send_buf, "buffer should be returned");
    ASSERT_EQ(g_send_len, msg_len, "buffer length should be preserved");
    free(g_send_buf);

    close(accepted_fd);
    close(client_fd);
    close(server_fd);
    lio_destroy(lio);
    TEST_PASS("test_send_basic");
}

static void test_send_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_send_called = 0;
    g_send_result = 0;
    g_send_buf = NULL;

    uint8_t *buf = alloc_test_buffer(16, 0xEE);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    lio_send(lio, 999999, buf, 16, 0, send_callback);
    tick_until_flag(lio, &g_send_called, 1000);

    ASSERT(g_send_called, "send callback should be called");
    ASSERT_LT(g_send_result, 0, "send on invalid fd should return error");
    ASSERT_NOT_NULL(g_send_buf, "buffer must be returned on error");
    ASSERT_EQ(g_send_len, 16, "buffer length should be preserved");
    /* Verify buffer wasn't corrupted */
    ASSERT(verify_buffer_pattern(g_send_buf, 16, 0xEE), "buffer should be intact");
    free(g_send_buf);

    lio_destroy(lio);
    TEST_PASS("test_send_invalid_fd");
}

/* ─── Recv Tests ─────────────────────────────────────────────────────────── */

static void test_recv_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    int server_fd, client_fd, accepted_fd;
    int ret = create_socket_pair(&server_fd, &client_fd, &accepted_fd);
    ASSERT_EQ(ret, 0, "socket pair creation should succeed");

    /* Send data from client (blocking) */
    const char *msg = "FFI test message";
    ssize_t sent = send(client_fd, msg, strlen(msg), 0);
    ASSERT_EQ(sent, (ssize_t)strlen(msg), "blocking send should succeed");

    g_recv_called = 0;
    g_recv_result = -999;
    g_recv_buf = NULL;

    uint8_t *buf = alloc_test_buffer(64, 0);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    lio_recv(lio, accepted_fd, buf, 64, 0, recv_callback);
    tick_until_flag(lio, &g_recv_called, 1000);

    ASSERT(g_recv_called, "recv callback should be called");
    ASSERT_EQ(g_recv_result, (int)strlen(msg), "recv should return bytes received");
    ASSERT_NOT_NULL(g_recv_buf, "buffer should be returned");
    ASSERT(memcmp(g_recv_buf, msg, strlen(msg)) == 0, "received data should match");
    free(g_recv_buf);

    close(accepted_fd);
    close(client_fd);
    close(server_fd);
    lio_destroy(lio);
    TEST_PASS("test_recv_basic");
}

static void test_recv_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_recv_called = 0;
    g_recv_result = 0;
    g_recv_buf = NULL;

    uint8_t *buf = alloc_test_buffer(32, 0xDD);
    ASSERT_NOT_NULL(buf, "buffer allocation should succeed");

    lio_recv(lio, 999999, buf, 32, 0, recv_callback);
    tick_until_flag(lio, &g_recv_called, 1000);

    ASSERT(g_recv_called, "recv callback should be called");
    ASSERT_LT(g_recv_result, 0, "recv on invalid fd should return error");
    ASSERT_NOT_NULL(g_recv_buf, "buffer must be returned on error");
    ASSERT_EQ(g_recv_len, 32, "buffer length should be preserved");
    free(g_recv_buf);

    lio_destroy(lio);
    TEST_PASS("test_recv_invalid_fd");
}

/* ─── Buffer Ownership Tests ─────────────────────────────────────────────── */

static void test_send_recv_buffer_roundtrip(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    int server_fd, client_fd, accepted_fd;
    int ret = create_socket_pair(&server_fd, &client_fd, &accepted_fd);
    ASSERT_EQ(ret, 0, "socket pair creation should succeed");

    /* Send specific pattern */
    const size_t test_len = 256;
    uint8_t *send_buf = (uint8_t*)malloc(test_len);
    ASSERT_NOT_NULL(send_buf, "send buffer allocation should succeed");
    for (size_t i = 0; i < test_len; i++) {
        send_buf[i] = (uint8_t)(i & 0xFF);
    }

    g_send_called = 0;
    lio_send(lio, client_fd, send_buf, test_len, 0, send_callback);
    tick_until_flag(lio, &g_send_called, 1000);
    ASSERT(g_send_called, "send should complete");
    ASSERT_EQ(g_send_result, (int)test_len, "all bytes should be sent");
    free(g_send_buf);

    /* Receive and verify pattern */
    g_recv_called = 0;
    uint8_t *recv_buf = alloc_test_buffer(test_len, 0);
    ASSERT_NOT_NULL(recv_buf, "recv buffer allocation should succeed");

    lio_recv(lio, accepted_fd, recv_buf, test_len, 0, recv_callback);
    tick_until_flag(lio, &g_recv_called, 1000);
    ASSERT(g_recv_called, "recv should complete");
    ASSERT_EQ(g_recv_result, (int)test_len, "all bytes should be received");

    /* Verify pattern integrity */
    int pattern_ok = 1;
    for (size_t i = 0; i < test_len; i++) {
        if (g_recv_buf[i] != (uint8_t)(i & 0xFF)) {
            pattern_ok = 0;
            break;
        }
    }
    ASSERT(pattern_ok, "data pattern should be preserved through send/recv");
    free(g_recv_buf);

    close(accepted_fd);
    close(client_fd);
    close(server_fd);
    lio_destroy(lio);
    TEST_PASS("test_send_recv_buffer_roundtrip");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Send/Recv Tests ===\n");

    test_send_basic();
    test_send_invalid_fd();
    test_recv_basic();
    test_recv_invalid_fd();
    test_send_recv_buffer_roundtrip();

    printf(GREEN "All send/recv tests passed\n" RESET);
    return 0;
}
