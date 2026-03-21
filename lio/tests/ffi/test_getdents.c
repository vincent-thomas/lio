/* test_getdents.c - Tests for lio_getdents (read directory entries) */
#include "test_utils.h"
#include <dirent.h>
#include <sys/stat.h>

#ifndef O_DIRECTORY
#define O_DIRECTORY 0x100000
#endif

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_getdents_called = 0;
static int g_getdents_result = -999;
static uint8_t *g_getdents_buf = NULL;
static size_t g_getdents_len = 0;

static void getdents_callback(int result, uint8_t *buf, size_t len) {
    g_getdents_result = result;
    g_getdents_buf = buf;
    g_getdents_len = len;
    g_getdents_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_getdents_tmp(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Open /tmp directory */
    int dir_fd = open("/tmp", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dir_fd, 0, "open /tmp should succeed");

    /* Allocate buffer */
    uint8_t *buf = (uint8_t*)malloc(4096);
    ASSERT_NOT_NULL(buf, "malloc should succeed");

    g_getdents_called = 0;
    g_getdents_result = -999;

    lio_getdents(lio, dir_fd, buf, 4096, getdents_callback);
    tick_until_flag(lio, &g_getdents_called, 2000);

    ASSERT(g_getdents_called, "getdents callback should be called");
    ASSERT_GE(g_getdents_result, 0, "getdents should succeed");
    ASSERT(g_getdents_len > 0, "should read some entries");

    /* The buffer now contains raw dirent structures */
    printf("  Read %d bytes of directory entries\n", g_getdents_result);

    free(g_getdents_buf);
    close(dir_fd);
    lio_destroy(lio);
    TEST_PASS("test_getdents_tmp");
}

static void test_getdents_with_files(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp directory with some files */
    char dir_path[256];
    snprintf(dir_path, sizeof(dir_path), "/tmp/lio_getdents_test_%d", getpid());
    ASSERT_EQ(mkdir(dir_path, 0755), 0, "mkdir should succeed");

    /* Create some test files */
    char file1[512], file2[512], file3[512];
    snprintf(file1, sizeof(file1), "%s/file1.txt", dir_path);
    snprintf(file2, sizeof(file2), "%s/file2.txt", dir_path);
    snprintf(file3, sizeof(file3), "%s/subdir", dir_path);

    int fd = open(file1, O_CREAT | O_WRONLY, 0644);
    close(fd);
    fd = open(file2, O_CREAT | O_WRONLY, 0644);
    close(fd);
    mkdir(file3, 0755);

    /* Open the directory */
    int dir_fd = open(dir_path, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dir_fd, 0, "open dir should succeed");

    /* Read all entries */
    int total_bytes = 0;
    int iterations = 0;

    while (iterations < 10) {
        uint8_t *buf = (uint8_t*)malloc(4096);
        ASSERT_NOT_NULL(buf, "malloc should succeed");

        g_getdents_called = 0;
        g_getdents_result = -999;

        lio_getdents(lio, dir_fd, buf, 4096, getdents_callback);
        tick_until_flag(lio, &g_getdents_called, 2000);

        ASSERT(g_getdents_called, "getdents callback should be called");
        ASSERT_GE(g_getdents_result, 0, "getdents should succeed");

        if (g_getdents_result == 0) {
            /* End of directory */
            free(g_getdents_buf);
            break;
        }

        total_bytes += g_getdents_result;
        free(g_getdents_buf);
        iterations++;
    }

    printf("  Read %d total bytes in %d iterations\n", total_bytes, iterations);
    ASSERT(total_bytes > 0, "should have read some directory entries");

    /* Cleanup */
    close(dir_fd);
    unlink(file1);
    unlink(file2);
    rmdir(file3);
    rmdir(dir_path);
    lio_destroy(lio);
    TEST_PASS("test_getdents_with_files");
}

static void test_getdents_invalid_fd(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    uint8_t *buf = (uint8_t*)malloc(4096);
    ASSERT_NOT_NULL(buf, "malloc should succeed");

    g_getdents_called = 0;
    g_getdents_result = 0;

    lio_getdents(lio, 999999, buf, 4096, getdents_callback);
    tick_until_flag(lio, &g_getdents_called, 1000);

    ASSERT(g_getdents_called, "getdents callback should be called");
    ASSERT_LT(g_getdents_result, 0, "getdents on invalid fd should fail");

    free(g_getdents_buf);
    lio_destroy(lio);
    TEST_PASS("test_getdents_invalid_fd");
}

static void test_getdents_regular_file(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Try to getdents on a regular file (should fail) */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file should succeed");

    uint8_t *buf = (uint8_t*)malloc(4096);
    ASSERT_NOT_NULL(buf, "malloc should succeed");

    g_getdents_called = 0;
    g_getdents_result = 0;

    lio_getdents(lio, fd, buf, 4096, getdents_callback);
    tick_until_flag(lio, &g_getdents_called, 1000);

    ASSERT(g_getdents_called, "getdents callback should be called");
    ASSERT_LT(g_getdents_result, 0, "getdents on regular file should fail");

    free(g_getdents_buf);
    close(fd);
    unlink(path);
    lio_destroy(lio);
    TEST_PASS("test_getdents_regular_file");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Getdents Tests ===\n");

    test_getdents_tmp();
    test_getdents_with_files();
    test_getdents_invalid_fd();
    test_getdents_regular_file();

    printf(GREEN "All getdents tests passed\n" RESET);
    return 0;
}
