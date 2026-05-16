//#Object:runtime.c
//#ExpectSym:_main
//#NoSym:_.Ldata0
//#NoSym:l_.str
//#NoSym:ltmp0

.section __TEXT,__text
.globl _main
.p2align 2
_main:
    adrp x0, _.Ldata0@PAGE
    add x0, x0, _.Ldata0@PAGEOFF
ltmp0:
    mov w0, #42
    b _exit_syscall

.section __DATA,__const
.p2align 3
_.Ldata0:
    .quad 123
l_.str:
    .asciz "temp"
