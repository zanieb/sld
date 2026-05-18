//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectSym:_main section="__text",offset-in-section=0
//#NoSym:_dead_text
//#ExpectMachOUnwindInfoSld:_main
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
_dead_text:
    mov w0, #13
    b _exit_syscall

.p2align 2
.globl _main
_main:
L_main_unwind_start:
    mov w0, #42
    b _exit_syscall
L_main_unwind_end:

.section __LD,__compact_unwind,regular,debug
.p2align 3
    .quad L_main_unwind_start
    .long L_main_unwind_end - L_main_unwind_start
    .long 0x02000000
    .quad 0
    .quad 0
