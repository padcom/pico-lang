/* setjmp/longjmp on wasm (P8.5a): LLVM lowers them to these helpers and Wasm exception
 * handling (`-wasm-enable-sjlj`, for the runtime and generated code alike). `setjmp` records
 * which call it was in which frame; `longjmp` throws the `__c_longjmp` tag (tag index 1, what
 * LLVM's lowering catches), carrying the buffer and the value; the frame that called `setjmp`
 * catches it and asks `__wasm_setjmp_test` whether the buffer is one of its own. wasi-libc
 * ships these as `libsetjmp`, which Zig's copy doesn't include. The protocol is LLVM's
 * (`WebAssemblyLowerEmscriptenEHSjLj.cpp`). */
#if defined(__wasm__)
#include <stdint.h>
#include <stdlib.h>

/* The tag (an exception type carrying one pointer); C can't define one. */
__asm__(".globl __c_longjmp\n"
        ".tagtype __c_longjmp i32\n"
        "__c_longjmp:\n");

typedef struct {
    void *func_invocation_id; /* which invocation of the function called setjmp */
    uint32_t label;           /* which setjmp call in it (never 0) */
    struct {
        void *env;
        int val;
    } arg; /* what longjmp throws */
} SjljBuf;

void __wasm_setjmp(void *env, uint32_t label, void *func_invocation_id) {
    SjljBuf *b = env;
    if (label == 0 || !func_invocation_id) abort();
    b->func_invocation_id = func_invocation_id;
    b->label = label;
}

uint32_t __wasm_setjmp_test(void *env, void *func_invocation_id) {
    SjljBuf *b = env;
    return b->func_invocation_id == func_invocation_id ? b->label : 0;
}

void __wasm_longjmp(void *env, int val) {
    SjljBuf *b = env;
    b->arg.env = env;
    b->arg.val = val ? val : 1;
    __builtin_wasm_throw(1, &b->arg);
}
#endif
