//#RunEnabled:true

.section __TEXT,__text
.globl _main
.p2align 2
_main:
    adrp x8, _target@GOTPAGE
    ldr x8, [x8, _target@GOTPAGEOFF]
    adrp x9, _target@PAGE
    add x9, x9, _target@PAGEOFF
    cmp x8, x9
    mov w0, #1
    b.ne 1f
    mov w0, #42
1:
    mov x16, #1
    svc #0x80

.section __DATA,__data
.globl _target
.p2align 3
_target:
    .quad 0x1234
