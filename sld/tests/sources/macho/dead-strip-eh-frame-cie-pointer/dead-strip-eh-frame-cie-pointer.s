//#Object:runtime.c
//#LinkArgs:-dead_strip
//#DiffEnabled:false
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    adrp x9, _live_fde@PAGE
    add x9, x9, _live_fde@PAGEOFF
    ldr w10, [x9, #4]
    cmp w10, #12
    b.ne L_bad_pointer
    mov w0, #42
    b _exit_syscall

L_bad_pointer:
    mov w0, #13
    b _exit_syscall

.section __TEXT,__eh_frame
.p2align 3
_live_cie:
    .long 4
    .long 0

_dead_cie:
    .long 4
    .long 0

.globl _live_fde
_live_fde:
    .long 20
    .long (_live_fde + 4) - _live_cie
    .quad _main
    .quad 4
