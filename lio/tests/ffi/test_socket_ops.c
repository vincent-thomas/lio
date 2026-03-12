/* test_socket_ops.c - Tests for socket operations: socket, bind, listen, accept, shutdown */
#include "test_utils.h"

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_socket_called = 0;
static intptr_t g_socket_result = -999;

static volatile int g_bind_called = 0;
static int g_bind_result = -999;

static volatile int g_listen_called = 0;
static int g_listen_result = -999;

static volatile int g_accept_called = 0;
static intptr_t g_accept_result = -999;
static const struct sockaddr_storage *g_accept_addr = NULL;

static volatile int g_shutdown_called = 0;
static int g_shutdown_result = -999;

static void socket_callback(intptr_t result) {
    g_socket_result = result;
    g_socket_called = 1;
}

static void bind_callback(int result) {
    g_bind_result = result;
    g_bind_called = 1;
}

static void listen_callback(int result) {
    g_listen_result = result;
    g_listen_called = 1;
}

static void accept_callback(intptr_t result, const struct sockaddr_storage *addr) {
    g_accept_result = result;
    g_accept_addr = addr;
    g_accept_called = 1;
}

static void shutdown_callback(int result) {
    g_shutdown_result = result;
    g_shutdown_called = 1;
}

/* ─── Socket Creation Tests ─────────────────────────────────────────────── */

static void test_socket_tcp(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_socket_called = 0;
    g_socket_result = -999;

    lio_socket(lio, AF_INET, SOCK_STREAM, 0, socket_callback);
    tick_until_flag(lio, &g_socket_called, 1000);

    ASSERT(g_socket_called, "socket callback should be called");
    ASSERT_GE(g_socket_result, 0, "socket should return valid fd");

    /* Clean up */
    close((int)g_socket_result);
    lio_destroy(lio);
    TEST_PASS("test_socket_tcp");
}

static void test_socket_udp(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_socket_called = 0;
    g_socket_result = -999;

    lio_socket(lio, AF_INET, SOCK_DGRAM, 0, socket_callback);
    tick_until_flag(lio, &g_socket_called, 1000);

    ASSERT(g_socket_called, "socket callback should be called");
    ASSERT_GE(g_socket_result, 0, "UDP socket should return valid fd");

    close((int)g_socket_result);
    lio_destroy(lio);
    TEST_PASS("test_socket_udp");
}

static void test_socket_invalid_domain(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_socket_called = 0;
    g_socket_result = 0;

    /* Invalid domain (-1) should fail */
    lio_socket(lio, -1, SOCK_STREAM, 0, socket_callback);
    tick_until_flag(lio, &g_socket_called, 1000);

    ASSERT(g_socket_called, "socket callback should be called");
    ASSERT_LT(g_socket_result, 0, "invalid domain should return error");

    lio_destroy(lio);
    TEST_PASS("test_socket_invalid_domain");
}

/* ─── Bind Tests ─────────────────────────────────────────────────────────── */

static void test_bind_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create socket */
    int sock = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(sock, 0, "socket creation should succeed");

    /* Allow address reuse */
    int opt = 1;
    setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    g_bind_called = 0;
    g_bind_result = -999;

    /* Bind to any available port */
    struct sockaddr_in addr = make_loopback_addr(0);
    lio_bind(lio, sock, (struct sockaddr*)&addr, sizeof(addr), bind_callback);
    tick_until_flag(lio, &g_bind_called, 1000);

    ASSERT(g_bind_called, "bind callback should be called");
    ASSERT_EQ(g_bind_result, 0, "bind should return 0 on success");

    close(sock);
    lio_destroy(lio);
    TEST_PASS("test_bind_basic");
}

static void test_bind_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_bind_called = 0;
    g_bind_result = 0;

    struct sockaddr_in addr = make_loopback_addr(0);
    lio_bind(lio, 999999, (struct sockaddr*)&addr, sizeof(addr), bind_callback);
    tick_until_flag(lio, &g_bind_called, 1000);

    ASSERT(g_bind_called, "bind callback should be called");
    ASSERT_LT(g_bind_result, 0, "bind on invalid fd should return error");

    lio_destroy(lio);
    TEST_PASS("test_bind_invalid_fd");
}

/* ─── Listen Tests ───────────────────────────────────────────────────────── */

static void test_listen_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create and bind socket */
    int sock = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(sock, 0, "socket creation should succeed");

    int opt = 1;
    setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr = make_loopback_addr(0);
    int ret = bind(sock, (struct sockaddr*)&addr, sizeof(addr));
    ASSERT_EQ(ret, 0, "bind should succeed");

    g_listen_called = 0;
    g_listen_result = -999;

    lio_listen(lio, sock, 128, listen_callback);
    tick_until_flag(lio, &g_listen_called, 1000);

    ASSERT(g_listen_called, "listen callback should be called");
    ASSERT_EQ(g_listen_result, 0, "listen should return 0 on success");

    close(sock);
    lio_destroy(lio);
    TEST_PASS("test_listen_basic");
}

static void test_listen_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_listen_called = 0;
    g_listen_result = 0;

    lio_listen(lio, 999999, 128, listen_callback);
    tick_until_flag(lio, &g_listen_called, 1000);

    ASSERT(g_listen_called, "listen callback should be called");
    ASSERT_LT(g_listen_result, 0, "listen on invalid fd should return error");

    lio_destroy(lio);
    TEST_PASS("test_listen_invalid_fd");
}

/* ─── Shutdown Tests ─────────────────────────────────────────────────────── */

static void test_shutdown_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create socket */
    int sock = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(sock, 0, "socket creation should succeed");

    g_shutdown_called = 0;
    g_shutdown_result = -999;

    /* Shutdown write side (should work even without connection on some systems) */
    lio_shutdown(lio, sock, SHUT_RDWR, shutdown_callback);
    tick_until_flag(lio, &g_shutdown_called, 1000);

    ASSERT(g_shutdown_called, "shutdown callback should be called");
    /* May succeed or fail with ENOTCONN depending on OS */

    close(sock);
    lio_destroy(lio);
    TEST_PASS("test_shutdown_basic");
}

static void test_shutdown_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_shutdown_called = 0;
    g_shutdown_result = 0;

    lio_shutdown(lio, 999999, SHUT_RDWR, shutdown_callback);
    tick_until_flag(lio, &g_shutdown_called, 1000);

    ASSERT(g_shutdown_called, "shutdown callback should be called");
    ASSERT_LT(g_shutdown_result, 0, "shutdown on invalid fd should return error");

    lio_destroy(lio);
    TEST_PASS("test_shutdown_invalid_fd");
}

/* ─── Integration: Full TCP Server Flow ─────────────────────────────────── */

static void test_full_server_flow(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Step 1: Create socket via FFI */
    g_socket_called = 0;
    lio_socket(lio, AF_INET, SOCK_STREAM, 0, socket_callback);
    tick_until_flag(lio, &g_socket_called, 1000);
    ASSERT(g_socket_called && g_socket_result >= 0, "socket should succeed");
    int server_fd = (int)g_socket_result;

    int opt = 1;
    setsockopt(server_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    /* Step 2: Bind */
    g_bind_called = 0;
    struct sockaddr_in addr = make_loopback_addr(0);
    lio_bind(lio, server_fd, (struct sockaddr*)&addr, sizeof(addr), bind_callback);
    tick_until_flag(lio, &g_bind_called, 1000);
    ASSERT(g_bind_called && g_bind_result == 0, "bind should succeed");

    /* Get assigned port */
    socklen_t addr_len = sizeof(addr);
    getsockname(server_fd, (struct sockaddr*)&addr, &addr_len);

    /* Step 3: Listen */
    g_listen_called = 0;
    lio_listen(lio, server_fd, 1, listen_callback);
    tick_until_flag(lio, &g_listen_called, 1000);
    ASSERT(g_listen_called && g_listen_result == 0, "listen should succeed");

    /* Step 4: Connect a client (using blocking connect for simplicity) */
    int client_fd = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(client_fd, 0, "client socket should succeed");

    int ret = connect(client_fd, (struct sockaddr*)&addr, sizeof(addr));
    ASSERT_EQ(ret, 0, "connect should succeed");

    /* Step 5: Accept via FFI */
    g_accept_called = 0;
    g_accept_addr = NULL;
    lio_accept(lio, server_fd, accept_callback);
    tick_until_flag(lio, &g_accept_called, 2000);
    ASSERT(g_accept_called, "accept callback should be called");
    ASSERT_GE(g_accept_result, 0, "accept should return valid fd");
    ASSERT_NOT_NULL(g_accept_addr, "accept should return peer address");

    int accepted_fd = (int)g_accept_result;

    /* Clean up */
    free((void*)g_accept_addr);
    close(accepted_fd);
    close(client_fd);
    close(server_fd);
    lio_destroy(lio);
    TEST_PASS("test_full_server_flow");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Socket Operation Tests ===\n");

    /* Socket creation */
    test_socket_tcp();
    test_socket_udp();
    test_socket_invalid_domain();

    /* Bind */
    test_bind_basic();
    test_bind_invalid_fd();

    /* Listen */
    test_listen_basic();
    test_listen_invalid_fd();

    /* Shutdown */
    test_shutdown_basic();
    test_shutdown_invalid_fd();

    /* Integration */
    test_full_server_flow();

    printf(GREEN "All socket operation tests passed\n" RESET);
    return 0;
}
