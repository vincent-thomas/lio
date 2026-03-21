/* test_udp_ops.c - Tests for lio_sendto and lio_recvfrom (UDP operations) */
#include "test_utils.h"

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_sendto_called = 0;
static int g_sendto_result = -999;
static uint8_t *g_sendto_buf = NULL;
static size_t g_sendto_len = 0;

static volatile int g_recvfrom_called = 0;
static int g_recvfrom_result = -999;
static uint8_t *g_recvfrom_buf = NULL;
static size_t g_recvfrom_len = 0;
static const struct sockaddr_storage *g_recvfrom_addr = NULL;

static void sendto_callback(int result, uint8_t *buf, size_t len) {
    g_sendto_result = result;
    g_sendto_buf = buf;
    g_sendto_len = len;
    g_sendto_called = 1;
}

static void recvfrom_callback(int result, uint8_t *buf, size_t len, const struct sockaddr_storage *addr) {
    g_recvfrom_result = result;
    g_recvfrom_buf = buf;
    g_recvfrom_len = len;
    g_recvfrom_addr = addr;
    g_recvfrom_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_udp_echo(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create two UDP sockets */
    int sender_fd = socket(AF_INET, SOCK_DGRAM, 0);
    int receiver_fd = socket(AF_INET, SOCK_DGRAM, 0);
    ASSERT_GE(sender_fd, 0, "sender socket should succeed");
    ASSERT_GE(receiver_fd, 0, "receiver socket should succeed");

    /* Bind receiver */
    struct sockaddr_in recv_addr = make_loopback_addr(0);
    int ret = bind(receiver_fd, (struct sockaddr*)&recv_addr, sizeof(recv_addr));
    ASSERT_EQ(ret, 0, "receiver bind should succeed");

    socklen_t addr_len = sizeof(recv_addr);
    getsockname(receiver_fd, (struct sockaddr*)&recv_addr, &addr_len);

    /* Send data first using blocking sendto */
    const char *msg = "Hello, UDP!";
    ret = sendto(sender_fd, msg, strlen(msg), 0,
                 (struct sockaddr*)&recv_addr, sizeof(recv_addr));
    ASSERT_EQ(ret, (int)strlen(msg), "blocking sendto should succeed");

    /* Now receive via FFI - data is already there */
    uint8_t *recv_buf = alloc_test_buffer(128, 0);
    ASSERT_NOT_NULL(recv_buf, "recv buffer alloc should succeed");

    g_recvfrom_called = 0;
    g_recvfrom_result = -999;
    g_recvfrom_addr = NULL;

    lio_recvfrom(lio, receiver_fd, recv_buf, 128, 0, recvfrom_callback);
    tick_until_flag(lio, &g_recvfrom_called, 1000);

    ASSERT(g_recvfrom_called, "recvfrom callback should be called");
    ASSERT_EQ(g_recvfrom_result, (int)strlen(msg), "recvfrom should receive all bytes");
    ASSERT_NOT_NULL(g_recvfrom_addr, "recvfrom should return sender address");
    ASSERT(memcmp(g_recvfrom_buf, msg, strlen(msg)) == 0, "received data should match");

    /* Clean up */
    free(g_recvfrom_buf);
    if (g_recvfrom_addr) free((void*)g_recvfrom_addr);
    close(sender_fd);
    close(receiver_fd);
    lio_destroy(lio);
    TEST_PASS("test_udp_echo");
}

static void test_sendto_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create UDP socket */
    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    ASSERT_GE(sock, 0, "socket should succeed");

    /* Target address (doesn't need to be listening for UDP) */
    struct sockaddr_in addr = make_loopback_addr(12345);

    /* Send data */
    uint8_t *buf = alloc_test_buffer(32, 0xAB);
    ASSERT_NOT_NULL(buf, "buffer alloc should succeed");

    g_sendto_called = 0;
    g_sendto_result = -999;

    lio_sendto(lio, sock, buf, 32, 0, (struct sockaddr*)&addr, sizeof(addr), sendto_callback);
    tick_until_flag(lio, &g_sendto_called, 1000);

    ASSERT(g_sendto_called, "sendto callback should be called");
    ASSERT_EQ(g_sendto_result, 32, "sendto should send all bytes");

    free(g_sendto_buf);
    close(sock);
    lio_destroy(lio);
    TEST_PASS("test_sendto_basic");
}

static void test_sendto_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    struct sockaddr_in addr = make_loopback_addr(12345);
    uint8_t *buf = alloc_test_buffer(16, 0);
    ASSERT_NOT_NULL(buf, "buffer alloc should succeed");

    g_sendto_called = 0;
    g_sendto_result = 0;

    lio_sendto(lio, 999999, buf, 16, 0, (struct sockaddr*)&addr, sizeof(addr), sendto_callback);
    tick_until_flag(lio, &g_sendto_called, 1000);

    ASSERT(g_sendto_called, "sendto callback should be called");
    ASSERT_LT(g_sendto_result, 0, "sendto on invalid fd should fail");

    free(g_sendto_buf);
    lio_destroy(lio);
    TEST_PASS("test_sendto_invalid_fd");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== UDP Operation Tests ===\n");

    test_sendto_basic();
    test_sendto_invalid_fd();
    test_udp_echo();

    printf(GREEN "All UDP operation tests passed\n" RESET);
    return 0;
}
