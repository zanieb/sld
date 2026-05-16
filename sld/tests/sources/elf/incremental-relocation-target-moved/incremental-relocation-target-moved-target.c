__asm__(
    ".section .data.incremental_target_moved,\"aw\"\n"
    ".globl incremental_relocation_target_padding\n"
    "incremental_relocation_target_padding:\n"
    ".word 1\n"
#ifdef INCREMENTAL_TARGET_MOVED
    ".word 0\n"
#endif
    ".globl incremental_relocation_target_moved\n"
    "incremental_relocation_target_moved:\n"
    ".word 2\n"
#ifndef INCREMENTAL_TARGET_MOVED
    ".word 0\n"
#endif
);
