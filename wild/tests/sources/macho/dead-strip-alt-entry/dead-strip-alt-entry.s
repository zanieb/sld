//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectSym:_main section="__text",offset-in-section=0
//#ExpectSym:_primary_target section="__text"
//#ExpectSym:_alt_target section="__text"
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    bl _alt_target
    mov w0, #42
    b _exit_syscall

.p2align 2
.globl _primary_target
_primary_target:
    nop

.globl _alt_target
.alt_entry _alt_target
_alt_target:
    ret
