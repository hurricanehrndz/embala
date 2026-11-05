/* Minimal Windows test payload; version injected via -DVERSION. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#define sleep_seconds(n) Sleep((DWORD)(n) * 1000)
#else
#include <unistd.h>
#define sleep_seconds(n) sleep((unsigned)(n))
#endif

#ifndef VERSION
#define VERSION "0.0.0"
#endif

int main(int argc, char **argv) {
    printf("hello from embala fixture %s\n", VERSION);
    /* `--sleep N` keeps the process (and its exe) busy for N seconds, for
     * tests that need a running payload (just nupkg-e2e's lock check). */
    if (argc == 3 && strcmp(argv[1], "--sleep") == 0) {
        fflush(stdout);
        sleep_seconds(atoi(argv[2]));
    }
    return 0;
}
