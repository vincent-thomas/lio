#ifndef LIO_URING_C11_ATOMICS_H
#define LIO_URING_C11_ATOMICS_H

/*
 * liburing's barrier.h includes <stdatomic.h>. On Ubuntu 26.04, libclang
 * follows Clang's header into libstdc++'s <stdatomic.h>, which is empty in
 * C, so atomic_load_explicit stays undeclared and bindgen aborts. Define the
 * operations liburing uses and close every copy of that header.
 */
#define __CLANG_STDATOMIC_H
#define _STDATOMIC_H
#define _GLIBCXX_STDATOMIC_H

#define memory_order_relaxed __ATOMIC_RELAXED
#define memory_order_consume __ATOMIC_CONSUME
#define memory_order_acquire __ATOMIC_ACQUIRE
#define memory_order_release __ATOMIC_RELEASE
#define memory_order_acq_rel __ATOMIC_ACQ_REL
#define memory_order_seq_cst __ATOMIC_SEQ_CST

#define atomic_store_explicit __c11_atomic_store
#define atomic_load_explicit __c11_atomic_load
#define atomic_thread_fence(order) __c11_atomic_thread_fence(order)

#endif
