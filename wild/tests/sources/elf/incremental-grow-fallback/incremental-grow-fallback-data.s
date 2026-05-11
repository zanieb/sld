.section .data.incremental_grow_fallback,"aw",@progbits
.globl incremental_grow_fallback_value
incremental_grow_fallback_value:
.byte 1, 2, 3, 4

.section .rodata.incremental_grow_fallback_after,"a",@progbits
.balign 8
.byte 7
