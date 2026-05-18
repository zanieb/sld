//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectMachOUnwindInfoSld:_live_target
//#NoSym:_dead_target
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    bl _live_target
    mov w0, #42
    b _exit_syscall

.p2align 2
.globl _dead_target
_dead_target:
    .cfi_startproc
    stp x29, x30, [sp, #-16]!
    mov x29, sp
    .cfi_def_cfa w29, 16
    .cfi_offset w30, -8
    .cfi_offset w29, -16
    ldp x29, x30, [sp], #16
    ret
    .cfi_endproc

.p2align 2
.globl _live_target
_live_target:
    .cfi_startproc
    stp x29, x30, [sp, #-16]!
    mov x29, sp
    .cfi_def_cfa w29, 16
    .cfi_offset w30, -8
    .cfi_offset w29, -16
    ldp x29, x30, [sp], #16
    ret
    .cfi_endproc
