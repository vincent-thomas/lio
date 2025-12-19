#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/socket.h>


#ifdef __cplusplus
extern "C" {
#endif  // __cplusplus

void lio_init(void);

int lio_try_init(void);

void lio_stop(void);
void lio_start(void);

/**
 * Shutdown the lio runtime and wait for all pending operations to complete.
 *
 * This function blocks until all pending I/O operations finish and their callbacks are called.
 * After calling this, no new operations should be submitted.
 */
void lio_exit(void);

/**
 * Shut down part of a full-duplex connection.
 *
 * # Parameters
 * - `fd`: Socket file descriptor
 * - `how`: How to shutdown (SHUT_RD=0, SHUT_WR=1, SHUT_RDWR=2)
 * - `callback(result)`: Called when complete
 *   - `result`: 0 on success, or negative errno on error
 */
void lio_shutdown(int fd,
                  int32_t how,
                  void (*callback)(int32_t));

void lio_symlinkat(int new_dir_fd,
                   const char *target,
                   const char *linkpath,
                   void (*callback)(int32_t));

void lio_linkat(int old_dir_fd,
                const char *old_path,
                int new_dir_fd,
                const char *new_path,
                void (*callback)(int32_t));

/**
 * Synchronize a file's in-core state with storage device.
 *
 * # Parameters
 * - `fd`: File descriptor
 * - `callback(result)`: Called when complete
 *   - `result`: 0 on success, or negative errno on error
 */
void lio_fsync(int fd,
               void (*callback)(int32_t));

/**
 * Write data to a file descriptor.
 *
 * Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
 *
 * # Parameters
 * - `fd`: File descriptor
 * - `buf`: malloc-allocated buffer containing data to write
 * - `buf_len`: Buffer length in bytes
 * - `offset`: File offset, or -1 for current position
 * - `callback(result, buf, len)`: Called when complete
 *   - `result`: Bytes written, or negative errno on error
 *   - `buf`: Original buffer pointer (must free)
 *   - `len`: Original buffer length
 */
void lio_write(int fd,
               uint8_t *buf,
               uintptr_t buf_len,
               int64_t offset,
               void (*callback)(int32_t,
                                uint8_t*,
                                uintptr_t));

/**
 * Read data from a file descriptor.
 *
 * Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
 *
 * # Parameters
 * - `fd`: File descriptor
 * - `buf`: malloc-allocated buffer to read into
 * - `buf_len`: Buffer length in bytes
 * - `offset`: File offset, or -1 for current position
 * - `callback(result, buf, len)`: Called when complete
 *   - `result`: Bytes read (check this, not `len`), 0 on EOF, or negative errno on error
 *   - `buf`: Original buffer pointer containing data (must free)
 *   - `len`: Original buffer length
 */
void lio_read(int fd,
              uint8_t *buf,
              uintptr_t buf_len,
              int64_t offset,
              void (*callback)(int32_t,
                               uint8_t*,
                               uintptr_t));

/**
 * Truncate a file to a specified length.
 *
 * # Parameters
 * - `fd`: File descriptor
 * - `len`: New file length in bytes
 * - `callback(result)`: Called when complete
 *   - `result`: 0 on success, or negative errno on error
 */
void lio_truncate(int fd,
                  uint64_t len,
                  void (*callback)(int32_t));

/**
 * Create a socket.
 *
 * # Parameters
 * - `domain`: Protocol family (AF_INET=2, AF_INET6=10, etc.)
 * - `ty`: Socket type (SOCK_STREAM=1, SOCK_DGRAM=2, etc.)
 * - `proto`: Protocol (IPPROTO_TCP=6, IPPROTO_UDP=17, or 0 for default)
 * - `callback(result)`: Called when complete
 *   - `result`: Socket file descriptor on success, or negative errno on error
 */
void lio_socket(int32_t domain,
                int32_t ty,
                int32_t proto,
                void (*callback)(int32_t));

/**
 * Bind a socket to an address.
 *
 * # Parameters
 * - `fd`: Socket file descriptor
 * - `sock`: Pointer to sockaddr structure (sockaddr_in or sockaddr_in6)
 * - `sock_len`: Pointer to size of sockaddr structure
 * - `callback(result)`: Called when complete
 *   - `result`: 0 on success, or negative errno on error
 */
void lio_bind(int fd,
              const struct sockaddr *sock,
              const socklen_t *sock_len,
              void (*callback)(int32_t));

/**
 * Accept a connection on a socket.
 *
 * # Parameters
 * - `fd`: Listening socket file descriptor
 * - `callback(result, addr)`: Called when complete
 *   - `result`: New socket file descriptor on success, or negative errno on error
 *   - `addr`: Pointer to peer address (null on error, caller must free on success)
 */
void lio_accept(int fd,
                void (*callback)(int32_t,
                                 const struct sockaddr_storage*));

/**
 * Listen for connections on a socket.
 *
 * # Parameters
 * - `fd`: Socket file descriptor
 * - `backlog`: Maximum length of pending connections queue
 * - `callback(result)`: Called when complete
 *   - `result`: 0 on success, or negative errno on error
 */
void lio_listen(int fd,
                int32_t backlog,
                void (*callback)(int32_t));

/**
 * Send data to a socket.
 *
 * Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
 *
 * # Parameters
 * - `fd`: Socket file descriptor
 * - `buf`: malloc-allocated buffer containing data to send
 * - `buf_len`: Buffer length in bytes
 * - `flags`: Send flags (e.g., MSG_DONTWAIT, MSG_NOSIGNAL)
 * - `callback(result, buf, len)`: Called when complete
 *   - `result`: Bytes sent, or negative errno on error
 *   - `buf`: Original buffer pointer (must free)
 *   - `len`: Original buffer length
 */
void lio_send(int fd,
              uint8_t *buf,
              uintptr_t buf_len,
              int32_t flags,
              void (*callback)(int32_t,
                               uint8_t*,
                               uintptr_t));

/**
 * Receive data from a socket.
 *
 * Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
 *
 * # Parameters
 * - `fd`: Socket file descriptor
 * - `buf`: malloc-allocated buffer to receive into
 * - `buf_len`: Buffer length in bytes
 * - `flags`: Receive flags (e.g., MSG_PEEK, MSG_WAITALL)
 * - `callback(result, buf, len)`: Called when complete
 *   - `result`: Bytes received (check this, not `len`), or negative errno on error
 *   - `buf`: Original buffer pointer containing data (must free)
 *   - `len`: Original buffer length
 */
void lio_recv(int fd,
              uint8_t *buf,
              uintptr_t buf_len,
              int32_t flags,
              void (*callback)(int32_t,
                               uint8_t*,
                               uintptr_t));

/**
 * Close a file descriptor.
 *
 * # Parameters
 * - `fd`: File descriptor to close
 * - `callback(result)`: Called when complete
 * - `result`: 0 on success, or negative errno on error
 */
void lio_close(int fd,
               void (*callback)(int32_t));

/**
 * Close a file descriptor.
 *
 * # Parameters
 * - `fd`: File descriptor to close
 * - `callback(result)`: Called when complete
 * - `result`: 0 on success, or negative errno on error
 */
void lio_timeout(int duration,
               void (*callback)(int32_t));


#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus
