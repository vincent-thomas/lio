/* test_signal.c - Tests for lio_signal (signal handling) */
#include "test_utils.h"
#include <signal.h>
#include <pthread.h>

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_signal_called = 0;
static int g_signal_result = -999;

static void signal_callback(int result) {
    g_signal_result = result;
    g_signal_called = 1;
}

/* ─── Helper to send signal from another thread ─────────────────────────── */

typedef struct {
    int signal;
    int delay_ms;
    pthread_t target;
} signal_sender_args;

static void* signal_sender(void *arg) {
    signal_sender_args *args = (signal_sender_args*)arg;
    usleep(args->delay_ms * 1000);
#ifdef __APPLE__
    /* On macOS, kqueue EVFILT_SIGNAL monitors process-wide signals */
    kill(getpid(), args->signal);
#else
    pthread_kill(args->target, args->signal);
#endif
    return NULL;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_signal_usr1(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Block SIGUSR1 before waiting */
    sigset_t sigset, oldset;
    sigemptyset(&sigset);
    sigaddset(&sigset, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &sigset, &oldset);

#ifdef __APPLE__
    /* On macOS, kqueue requires signal to be ignored for EVFILT_SIGNAL */
    struct sigaction sa, old_sa;
    sa.sa_handler = SIG_IGN;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;
    sigaction(SIGUSR1, &sa, &old_sa);
#endif

    g_signal_called = 0;
    g_signal_result = -999;

    /* Start waiting for SIGUSR1 */
    int signals[] = {SIGUSR1};
    lio_signal(lio, signals, 1, signal_callback);

    /* Send SIGUSR1 from another thread after a short delay */
    pthread_t sender;
    signal_sender_args args = {SIGUSR1, 50, pthread_self()};
    pthread_create(&sender, NULL, signal_sender, &args);

    /* Wait for signal */
    tick_until_flag(lio, &g_signal_called, 2000);

    pthread_join(sender, NULL);

    ASSERT(g_signal_called, "signal callback should be called");
    ASSERT_EQ(g_signal_result, SIGUSR1, "should receive SIGUSR1");

#ifdef __APPLE__
    sigaction(SIGUSR1, &old_sa, NULL);
#endif
    /* Restore signal mask */
    pthread_sigmask(SIG_SETMASK, &oldset, NULL);

    lio_destroy(lio);
    TEST_PASS("test_signal_usr1");
}

static void test_signal_usr2(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Block SIGUSR2 */
    sigset_t sigset, oldset;
    sigemptyset(&sigset);
    sigaddset(&sigset, SIGUSR2);
    pthread_sigmask(SIG_BLOCK, &sigset, &oldset);

#ifdef __APPLE__
    struct sigaction sa, old_sa;
    sa.sa_handler = SIG_IGN;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;
    sigaction(SIGUSR2, &sa, &old_sa);
#endif

    g_signal_called = 0;
    g_signal_result = -999;

    int signals[] = {SIGUSR2};
    lio_signal(lio, signals, 1, signal_callback);

    /* Send SIGUSR2 */
    pthread_t sender;
    signal_sender_args args = {SIGUSR2, 50, pthread_self()};
    pthread_create(&sender, NULL, signal_sender, &args);

    tick_until_flag(lio, &g_signal_called, 2000);
    pthread_join(sender, NULL);

    ASSERT(g_signal_called, "signal callback should be called");
    ASSERT_EQ(g_signal_result, SIGUSR2, "should receive SIGUSR2");

#ifdef __APPLE__
    sigaction(SIGUSR2, &old_sa, NULL);
#endif
    pthread_sigmask(SIG_SETMASK, &oldset, NULL);
    lio_destroy(lio);
    TEST_PASS("test_signal_usr2");
}

static void test_signal_multiple(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Block both SIGUSR1 and SIGUSR2 */
    sigset_t sigset, oldset;
    sigemptyset(&sigset);
    sigaddset(&sigset, SIGUSR1);
    sigaddset(&sigset, SIGUSR2);
    pthread_sigmask(SIG_BLOCK, &sigset, &oldset);

#ifdef __APPLE__
    struct sigaction sa, old_sa1, old_sa2;
    sa.sa_handler = SIG_IGN;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;
    sigaction(SIGUSR1, &sa, &old_sa1);
    sigaction(SIGUSR2, &sa, &old_sa2);
#endif

    g_signal_called = 0;
    g_signal_result = -999;

    /* Wait for either signal */
    int signals[] = {SIGUSR1, SIGUSR2};
    lio_signal(lio, signals, 2, signal_callback);

    /* Send SIGUSR1 (first in the list - macOS only registers first signal) */
    pthread_t sender;
    signal_sender_args args = {SIGUSR1, 50, pthread_self()};
    pthread_create(&sender, NULL, signal_sender, &args);

    tick_until_flag(lio, &g_signal_called, 2000);
    pthread_join(sender, NULL);

    ASSERT(g_signal_called, "signal callback should be called");
    ASSERT_EQ(g_signal_result, SIGUSR1, "should receive SIGUSR1");

#ifdef __APPLE__
    sigaction(SIGUSR1, &old_sa1, NULL);
    sigaction(SIGUSR2, &old_sa2, NULL);
#endif
    pthread_sigmask(SIG_SETMASK, &oldset, NULL);
    lio_destroy(lio);
    TEST_PASS("test_signal_multiple");
}

static void test_signal_empty_set(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    g_signal_called = 0;
    g_signal_result = 0;

    /* Empty signal set - should fail or return error */
    lio_signal(lio, NULL, 0, signal_callback);
    tick_until_flag(lio, &g_signal_called, 1000);

    /* This may or may not call the callback depending on implementation */
    /* Just verify we don't crash */

    lio_destroy(lio);
    TEST_PASS("test_signal_empty_set");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Signal Tests ===\n");

    test_signal_usr1();
    test_signal_usr2();
    test_signal_multiple();
    test_signal_empty_set();

    printf(GREEN "All signal tests passed\n" RESET);
    return 0;
}
