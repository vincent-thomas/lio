/* test_fs_ops.c - Tests for filesystem operations: symlinkat, linkat, unlinkat, renameat, mkdirat */
#include "test_utils.h"
#include <sys/stat.h>

#ifndef AT_FDCWD
#define AT_FDCWD -100
#endif

#ifndef AT_REMOVEDIR
#define AT_REMOVEDIR 0x200
#endif

/* ─── Callback state ─────────────────────────────────────────────────────── */

static volatile int g_op_called = 0;
static int g_op_result = -999;

static void op_callback(int result) {
    g_op_result = result;
    g_op_called = 1;
}

/* ─── Tests ──────────────────────────────────────────────────────────────── */

static void test_mkdirat_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    char path[256];
    snprintf(path, sizeof(path), "/tmp/lio_ffi_mkdir_test_%d", getpid());

    g_op_called = 0;
    g_op_result = -999;

    lio_mkdirat(lio, AT_FDCWD, path, 0755, op_callback);
    tick_until_flag(lio, &g_op_called, 1000);

    ASSERT(g_op_called, "mkdirat callback should be called");
    ASSERT_EQ(g_op_result, 0, "mkdirat should succeed");

    /* Verify directory exists */
    struct stat st;
    ASSERT_EQ(stat(path, &st), 0, "directory should exist");
    ASSERT(S_ISDIR(st.st_mode), "should be a directory");

    /* Clean up */
    rmdir(path);
    lio_destroy(lio);
    TEST_PASS("test_mkdirat_basic");
}

static void test_unlinkat_file(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file */
    char path[256];
    int fd = create_temp_file(path, sizeof(path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");
    close(fd);

    g_op_called = 0;
    g_op_result = -999;

    lio_unlinkat(lio, AT_FDCWD, path, 0, op_callback);
    tick_until_flag(lio, &g_op_called, 1000);

    ASSERT(g_op_called, "unlinkat callback should be called");
    ASSERT_EQ(g_op_result, 0, "unlinkat should succeed");

    /* Verify file is gone */
    struct stat st;
    ASSERT(stat(path, &st) != 0, "file should be removed");

    lio_destroy(lio);
    TEST_PASS("test_unlinkat_file");
}

static void test_unlinkat_dir(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp directory */
    char path[256];
    snprintf(path, sizeof(path), "/tmp/lio_ffi_rmdir_test_%d", getpid());
    ASSERT_EQ(mkdir(path, 0755), 0, "mkdir should succeed");

    g_op_called = 0;
    g_op_result = -999;

    lio_unlinkat(lio, AT_FDCWD, path, AT_REMOVEDIR, op_callback);
    tick_until_flag(lio, &g_op_called, 1000);

    ASSERT(g_op_called, "unlinkat callback should be called");
    ASSERT_EQ(g_op_result, 0, "unlinkat with AT_REMOVEDIR should succeed");

    /* Verify directory is gone */
    struct stat st;
    ASSERT(stat(path, &st) != 0, "directory should be removed");

    lio_destroy(lio);
    TEST_PASS("test_unlinkat_dir");
}

static void test_renameat_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file */
    char old_path[256];
    int fd = create_temp_file(old_path, sizeof(old_path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");
    close(fd);

    char new_path[256];
    snprintf(new_path, sizeof(new_path), "%s_renamed", old_path);

    g_op_called = 0;
    g_op_result = -999;

    lio_renameat(lio, AT_FDCWD, old_path, AT_FDCWD, new_path, op_callback);
    tick_until_flag(lio, &g_op_called, 1000);

    ASSERT(g_op_called, "renameat callback should be called");
    ASSERT_EQ(g_op_result, 0, "renameat should succeed");

    /* Verify old path is gone, new path exists */
    struct stat st;
    ASSERT(stat(old_path, &st) != 0, "old path should not exist");
    ASSERT_EQ(stat(new_path, &st), 0, "new path should exist");

    /* Clean up */
    unlink(new_path);
    lio_destroy(lio);
    TEST_PASS("test_renameat_basic");
}

static void test_symlinkat_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file to link to */
    char target_path[256];
    int fd = create_temp_file(target_path, sizeof(target_path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");
    close(fd);

    char link_path[256];
    snprintf(link_path, sizeof(link_path), "%s_symlink", target_path);

    g_op_called = 0;
    g_op_result = -999;

    lio_symlinkat(lio, AT_FDCWD, target_path, link_path, op_callback);
    tick_until_flag(lio, &g_op_called, 1000);

    ASSERT(g_op_called, "symlinkat callback should be called");
    ASSERT_EQ(g_op_result, 0, "symlinkat should succeed");

    /* Verify symlink exists */
    struct stat st;
    ASSERT_EQ(lstat(link_path, &st), 0, "symlink should exist");
    ASSERT(S_ISLNK(st.st_mode), "should be a symlink");

    /* Clean up */
    unlink(link_path);
    unlink(target_path);
    lio_destroy(lio);
    TEST_PASS("test_symlinkat_basic");
}

static void test_linkat_basic(void) {
    lio_handle_t *lio = lio_create(TEST_CAPACITY);
    ASSERT_NOT_NULL(lio, "lio_create should succeed");

    /* Create a temp file */
    char old_path[256];
    int fd = create_temp_file(old_path, sizeof(old_path));
    ASSERT_GE(fd, 0, "temp file creation should succeed");
    close(fd);

    char new_path[256];
    snprintf(new_path, sizeof(new_path), "%s_hardlink", old_path);

    g_op_called = 0;
    g_op_result = -999;

    lio_linkat(lio, AT_FDCWD, old_path, AT_FDCWD, new_path, op_callback);
    tick_until_flag(lio, &g_op_called, 1000);

    ASSERT(g_op_called, "linkat callback should be called");
    ASSERT_EQ(g_op_result, 0, "linkat should succeed");

    /* Verify hard link exists with same inode */
    struct stat st1, st2;
    ASSERT_EQ(stat(old_path, &st1), 0, "old path should exist");
    ASSERT_EQ(stat(new_path, &st2), 0, "new path should exist");
    ASSERT_EQ(st1.st_ino, st2.st_ino, "should have same inode");
    ASSERT_EQ(st1.st_nlink, 2, "should have 2 links");

    /* Clean up */
    unlink(new_path);
    unlink(old_path);
    lio_destroy(lio);
    TEST_PASS("test_linkat_basic");
}

/* ─── Main ───────────────────────────────────────────────────────────────── */

int main(void) {
    printf("=== Filesystem Operation Tests ===\n");

    test_mkdirat_basic();
    test_unlinkat_file();
    test_unlinkat_dir();
    test_renameat_basic();
    test_symlinkat_basic();
    test_linkat_basic();

    printf(GREEN "All filesystem operation tests passed\n" RESET);
    return 0;
}
