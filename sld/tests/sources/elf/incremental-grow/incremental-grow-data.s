.section .data.incremental_grow,"aw",@progbits
.globl incremental_grow_value
incremental_grow_value:
.byte 1, 2, 3, 4

.section .rodata.incremental_grow_after,"a",@progbits
.balign 8
.byte 7
