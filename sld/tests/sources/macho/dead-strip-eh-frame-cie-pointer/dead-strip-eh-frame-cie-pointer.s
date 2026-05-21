//#Object:runtime.c
//#LinkArgs:-dead_strip
//#DiffEnabled:false
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    adrp x9, _live_cie@PAGE
    add x9, x9, _live_cie@PAGEOFF
    add x9, x9, #8
    ldr w10, [x9, #4]
    cmp w10, #12
    b.ne L_bad_pointer
    add x10, x9, #8
    ldrsw x11, [x10]
    add x10, x10, x11
    adrp x11, _main@PAGE
    add x11, x11, _main@PAGEOFF
    cmp x10, x11
    b.ne L_bad_pointer
    mov w0, #42
    b _exit_syscall

L_bad_pointer:
    mov w0, #13
    b _exit_syscall

.section __TEXT,__eh_frame
.p2align 3
L_eh_frame_start:
_live_cie:
    .long 4
    .long 0

_dead_cie:
    .long 4
    .long 0

L_live_fde:
    .long 12
    .long (L_live_fde + 4) - _live_cie
L_live_pc_begin:
    .long _main - L_eh_frame_start - (L_live_pc_begin - L_eh_frame_start)
    .long 4
