/* The program entry for linux-gnu aarch64, replacing glibc's Scrt1.o (decision 245): sp
 * points at argc, argv…, and x0 holds the dynamic linker's fini function. */
    .text
    .globl _start
    .type _start, %function
_start:
    mov x29, #0
    mov x30, #0
    mov x5, x0                      /* rtld_fini */
    ldr x1, [sp]                    /* argc */
    add x2, sp, #8                  /* argv */
    mov x6, sp                      /* stack_end */
    adrp x0, :got:main
    ldr x0, [x0, :got_lo12:main]
    adrp x3, ppl_glibc_init         /* init */
    add x3, x3, :lo12:ppl_glibc_init
    mov x4, #0                      /* fini: ld.so runs .fini_array itself */
    bl __libc_start_main
    bl abort
    .size _start, . - _start

    .section .note.GNU-stack, "", %progbits
