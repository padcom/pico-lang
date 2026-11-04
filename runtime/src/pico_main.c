/* The program's `main` (its own object, so a library built with `pico build --lib` has
 * none; spec §12.4). */
#include "pico_rt.h"

#include <stdio.h>

void pico_runtime_start(int argc, char **argv);

int main(int argc, char **argv) {
    pico_runtime_start(argc, argv);
    pico_entry();
    fflush(stdout);
    return 0;
}
