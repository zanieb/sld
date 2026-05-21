//#RunEnabled:true

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    adrp x8, _pointer@PAGE
    add x8, x8, _pointer@PAGEOFF
    ldr x8, [x8]

    adrp x9, _target@PAGE
    add x9, x9, _target@PAGEOFF
    add x9, x9, #8

    cmp x8, x9
    mov w0, #1
    b.ne 1f
    mov w0, #42
1:
    mov x16, #1
    svc #0x80

.section __DATA,__const
.p2align 3
.globl _pointer
_pointer:
    .quad _target + 8

.p2align 3
.globl _target
_target:
    .quad 0x1111
    .quad 0x2222
