// The lab's allocation counter (the rule it checks: process allocates nothing). Injected with
// DYLD_INSERT_LIBRARIES into the process under test; counts calls of the allocation functions below made on the ONE
// thread that armed it (summoner_malloccount_arm), between arm and disarm. The thread is compared by pthread_self(), not a
// thread-local, because a thread-local's first touch may itself allocate inside malloc.
#include <malloc/malloc.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>

#define INTERPOSE(replacement, replacee)                                                                 \
    __attribute__((used)) static struct {                                                                \
        const void* r;                                                                                   \
        const void* e;                                                                                   \
    } interpose_##replacee __attribute__((section("__DATA,__interpose"))) = {(const void*)&replacement, \
                                                                             (const void*)&replacee}

static _Atomic(uintptr_t) armed_thread = 0;
static _Atomic(uint64_t) count = 0;

static inline void tick(void) {
    const uintptr_t t = atomic_load_explicit(&armed_thread, memory_order_relaxed);
    if (t != 0 && t == (uintptr_t)pthread_self())
        atomic_fetch_add_explicit(&count, 1, memory_order_relaxed);
}

__attribute__((visibility("default"))) void summoner_malloccount_arm(void) {
    atomic_store(&count, 0);
    atomic_store(&armed_thread, (uintptr_t)pthread_self());
}

__attribute__((visibility("default"))) uint64_t summoner_malloccount_disarm(void) {
    atomic_store(&armed_thread, 0);
    return atomic_exchange(&count, 0);
}

static void* my_malloc(size_t n) { tick(); return malloc(n); }
static void* my_calloc(size_t c, size_t n) { tick(); return calloc(c, n); }
static void* my_realloc(void* p, size_t n) { tick(); return realloc(p, n); }
static void* my_valloc(size_t n) { tick(); return valloc(n); }
static void* my_aligned_alloc(size_t a, size_t n) { tick(); return aligned_alloc(a, n); }
static int my_posix_memalign(void** p, size_t a, size_t n) { tick(); return posix_memalign(p, a, n); }
static void* my_malloc_zone_malloc(malloc_zone_t* z, size_t n) { tick(); return malloc_zone_malloc(z, n); }

INTERPOSE(my_malloc, malloc);
INTERPOSE(my_calloc, calloc);
INTERPOSE(my_realloc, realloc);
INTERPOSE(my_valloc, valloc);
INTERPOSE(my_aligned_alloc, aligned_alloc);
INTERPOSE(my_posix_memalign, posix_memalign);
INTERPOSE(my_malloc_zone_malloc, malloc_zone_malloc);
