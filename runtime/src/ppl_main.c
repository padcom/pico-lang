/* The program's `main` (its own object, so a library built with `ppl build --lib` has
 * none; spec §12.4). */
#include "ppl_rt.h"

#include <stdio.h>

void ppl_runtime_start(int argc, char **argv);

int main(int argc, char **argv) {
    ppl_runtime_start(argc, argv);
    ppl_entry();
    fflush(stdout);
    return 0;
}
