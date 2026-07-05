/* Minimal Windows test payload; version injected via -DVERSION. */
#include <stdio.h>

#ifndef VERSION
#define VERSION "0.0.0"
#endif

int main(void) {
    printf("hello from embala fixture %s\n", VERSION);
    return 0;
}
