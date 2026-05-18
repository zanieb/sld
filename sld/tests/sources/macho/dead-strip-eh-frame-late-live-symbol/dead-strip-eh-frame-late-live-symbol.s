//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectMachOUnwindInfoSld:_alt_target
//#NoSym:_dead_target
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    bl _primary_target
    mov w0, #42
    b _exit_syscall

.p2align 2
.globl _primary_target
_primary_target:
    adrp x9, _alt_target@PAGE
    add x9, x9, _alt_target@PAGEOFF
    br x9

.globl _alt_target
.alt_entry _alt_target
_alt_target:
    ret

.p2align 2
.globl _dead_target
_dead_target:
    ret

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
    .quad _alt_target
    .quad 4

.globl _dead_fde
_dead_fde:
    .long 20
    .long (_dead_fde + 4) - _dead_cie
    .quad _dead_target
    .quad 4
