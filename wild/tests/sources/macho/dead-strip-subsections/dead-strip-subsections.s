//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectSym:_main section="__text",offset-in-section=0
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
_.Ldead:
    mov w0, #13
    b _exit_syscall

.globl _main
.p2align 2
_main:
    adrp x9, _.Lkeep@PAGE
    add x9, x9, _.Lkeep@PAGEOFF
    br x9

.p2align 2
_.Lkeep:
    mov w0, #42
    b _exit_syscall
