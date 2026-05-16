.section __TEXT,__custom,regular,pure_instructions
.globl _custom_exit_code
.p2align 2
_custom_exit_code:
    mov x0, #42
    ret
