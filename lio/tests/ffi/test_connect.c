/* test_connect.c - Tests for lio_connect (socket connect) */
#include "test_utils.h"

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_connect_called = 0;
static int g_connect_result = -999;

static volatile int g_socket_called = 0;
static intptr_t g_socket_result = -999;

static void connect_callback(int result) {
    g_connect_result = result;
    g_connect_called = 1;
}

static void socket_callback(intptr_t result) {
    g_socket_result = result;
    g_socket_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_connect_to_server(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Set up a server socket first */
    int server_fd = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(server_fd, 0, "server socket creation should succeed");

    int opt = 1;
    setsockopt(server_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr = make_loopback_addr(0);
    int ret = bind(server_fd, (struct sockaddr*)&addr, sizeof(addr));
    ASSERT_EQ(ret, 0, "bind should succeed");

    socklen_t addr_len = sizeof(addr);
    getsockname(server_fd, (struct sockaddr*)&addr, &addr_len);

    ret = listen(server_fd, 1);
    ASSERT_EQ(ret, 0, "listen should succeed");

    /* Create client socket via FFI */
    g_socket_called = 0;
    lio_socket(lio, AF_INET, SOCK_STREAM, 0, socket_callback);
    tick_until_flag(lio, &g_socket_called, 1000);
    ASSERT(g_socket_called && g_socket_result >= 0, "client socket should succeed");
    int client_fd = (int)g_socket_result;

    /* Connect via FFI */
    g_connect_called = 0;
    g_connect_result = -999;

    lio_connect(lio, client_fd, (struct sockaddr*)&addr, sizeof(addr), connect_callback);
    tick_until_flag(lio, &g_connect_called, 2000);

    ASSERT(g_connect_called, "connect callback should be called");
    ASSERT_EQ(g_connect_result, 0, "connect should succeed");

    /* Clean up */
    close(client_fd);
    close(server_fd);
    lio_destroy(lio);
    TEST_PASS("test_connect_to_server");
}

static void test_connect_refused(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create socket via FFI */
    g_socket_called = 0;
    lio_socket(lio, AF_INET, SOCK_STREAM, 0, socket_callback);
    tick_until_flag(lio, &g_socket_called, 1000);
    ASSERT(g_socket_called && g_socket_result >= 0, "socket should succeed");
    int sock_fd = (int)g_socket_result;

    /* Try connecting to a port that nothing is listening on */
    struct sockaddr_in addr = make_loopback_addr(59999); /* unlikely to be in use */

    g_connect_called = 0;
    g_connect_result = 0;

    lio_connect(lio, sock_fd, (struct sockaddr*)&addr, sizeof(addr), connect_callback);
    tick_until_flag(lio, &g_connect_called, 3000);

    ASSERT(g_connect_called, "connect callback should be called");
    ASSERT_LT(g_connect_result, 0, "connect to refused port should fail");

    close(sock_fd);
    lio_destroy(lio);
    TEST_PASS("test_connect_refused");
}

static void test_connect_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_connect_called = 0;
    g_connect_result = 0;

    struct sockaddr_in addr = make_loopback_addr(8080);
    lio_connect(lio, 999999, (struct sockaddr*)&addr, sizeof(addr), connect_callback);
    tick_until_flag(lio, &g_connect_called, 1000);

    ASSERT(g_connect_called, "connect callback should be called");
    ASSERT_LT(g_connect_result, 0, "connect on invalid fd should return error");

    lio_destroy(lio);
    TEST_PASS("test_connect_invalid_fd");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Connect Tests ===\n");

    test_connect_to_server();
    test_connect_refused();
    test_connect_invalid_fd();

    printf(GREEN "All connect tests passed\n" RESET);
    return 0;
}
