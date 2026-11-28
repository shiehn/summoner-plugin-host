// The allocation counter's positive control (ctest, with the counter injected): three mallocs on the armed thread
// count 3, nothing counts 0, another thread's mallocs count nothing.
#include <dlfcn.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

// Kept through a volatile, so the optimiser cannot remove an allocation whose result is only freed.
static void* volatile keep[16];

static void* other(void* arg) {
    for (int i = 0; i < 10; ++i) {
        keep[i] = malloc(32);
        free(keep[i]);
    }
    return arg;
}

int main(void) {
    void (*arm)(void) = (void (*)(void))dlsym(RTLD_DEFAULT, "summoner_malloccount_arm");
    uint64_t (*disarm)(void) = (uint64_t(*)(void))dlsym(RTLD_DEFAULT, "summoner_malloccount_disarm");
    if (arm == NULL || disarm == NULL) {
        fprintf(stderr, "the counter is not injected\n");
        return 2;
    }
    arm();
    keep[0] = malloc(16);
    keep[1] = calloc(2, 16);
    keep[2] = realloc(NULL, 16);
    const uint64_t three = disarm();
    free(keep[0]), free(keep[1]), free(keep[2]);
    arm();
    const uint64_t none = disarm();
    pthread_t t;
    pthread_create(&t, NULL, other, NULL);
    arm();
    pthread_join(t, NULL);
    const uint64_t elsewhere = disarm();
    printf("three %llu none %llu elsewhere %llu\n", (unsigned long long)three, (unsigned long long)none,
           (unsigned long long)elsewhere);
    return three == 3 && none == 0 && elsewhere == 0 ? 0 : 1;
}
