//#Object:runtime.c
//#Object:late-fde.s
//#ExpectMachOUnwindInfoSld:_late_target
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    bl _late_target
    mov w0, #42
    b _exit_syscall

.section __TEXT,__eh_frame
.p2align 3
_main_cie:
    .long 4
    .long 0

.globl _main_fde
_main_fde:
    .long 20
    .long (_main_fde + 4) - _main_cie
    .quad _main
    .quad 4
