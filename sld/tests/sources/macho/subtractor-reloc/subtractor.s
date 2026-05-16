.section __TEXT,__text
.globl _subtractor_base
_subtractor_base:
  nop
  nop
.globl _subtractor_target
_subtractor_target:
  nop

.section __DATA,__const
.p2align 3
.globl _subtractor_value
_subtractor_value:
  .quad _subtractor_target - _subtractor_base + 5
