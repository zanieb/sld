//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectSym:_main section="__text",offset-in-section=0
//#ExpectSym:_live_prefix section="__const",offset-in-section=0
//#ExpectSym:_live_aligned section="__const",offset-in-section=16
//#NoSym:_dead_blob
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    adrp x9, _live_prefix@PAGE
    add x9, x9, _live_prefix@PAGEOFF
    adrp x10, _live_aligned@PAGE
    add x10, x10, _live_aligned@PAGEOFF
    mov w0, #42
    b _exit_syscall

.section __DATA,__const
.globl _live_prefix
_live_prefix:
    .space 11

.globl _dead_blob
_dead_blob:
    .space 9

.p2align 3
.globl _live_aligned
_live_aligned:
    .quad 0x1122334455667788
