#ifdef INCREMENTAL_TARGET_MOVED
__attribute__((section(".data.incremental_target_moved"), used)) volatile int
    incremental_relocation_target_moved = 2;
__attribute__((section(".data.incremental_target_moved"), used)) volatile int
    incremental_relocation_target_padding = 1;
#else
__attribute__((section(".data.incremental_target_moved"), used)) volatile int
    incremental_relocation_target_padding = 1;
__attribute__((section(".data.incremental_target_moved"), used)) volatile int
    incremental_relocation_target_moved = 2;
#endif
