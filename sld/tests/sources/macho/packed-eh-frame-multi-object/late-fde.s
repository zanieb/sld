.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _late_target
_late_target:
    ret

.section __TEXT,__eh_frame
.p2align 3
_late_cie:
    .long 4
    .long 0

.globl _late_fde
_late_fde:
    .long 20
    .long (_late_fde + 4) - _late_cie
    .quad _late_target
    .quad 4
