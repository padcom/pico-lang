/* The program entry for linux-gnu x86-64, replacing glibc's Scrt1.o (decision 245): the
 * kernel's stack is argc, argv…, and rdx holds the dynamic linker's fini function. */
    .text
    .globl _start
    .type _start, @function
_start:
    xorl %ebp, %ebp
    movq %rdx, %r9                  /* rtld_fini */
    popq %rsi                       /* argc */
    movq %rsp, %rdx                 /* argv */
    andq $~15, %rsp
    pushq %rax                      /* keep the stack 16-byte aligned */
    pushq %rsp                      /* stack_end */
    xorl %r8d, %r8d                 /* fini: ld.so runs .fini_array itself */
    leaq pico_glibc_init(%rip), %rcx /* init */
    movq main@GOTPCREL(%rip), %rdi
    call *__libc_start_main@GOTPCREL(%rip)
    hlt
    .size _start, . - _start

    .section .note.GNU-stack, "", @progbits
