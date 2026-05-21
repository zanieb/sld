//#ExpectSectionBytes:__unwind_info=0x010000001c00000003000000280000000000000028000000020000000000000200000004000000007002000040000000400000008002000000000000400000000200000008000300700200000000000274020000000000007c0200000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000
//#RunEnabled:false

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    ret
L_main_unwind_end:
    nop
    nop

.p2align 2
.globl _next
_next:
    ret
L_next_end:

.section __LD,__compact_unwind,regular,debug
.p2align 3
    .quad _main
    .long L_main_unwind_end - _main
    .long 0x02000000
    .quad 0
    .quad 0

    .quad _next
    .long L_next_end - _next
    .long 0x02000000
    .quad 0
    .quad 0
