//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectSym:_main section="__text",offset-in-section=0
//#ExpectSectionBytes:__const=0x8877665544332211
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    adrp x9, _.Llive_value@PAGE
    add x9, x9, _.Llive_value@PAGEOFF
    mov w0, #42
    b _exit_syscall

.section __DATA,__const
.p2align 3
_.Llive_value:
    .quad 0x1122334455667788

.p2align 3
_.Ldead_value:
    .quad 0xaabbccddeeff0011
