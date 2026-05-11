//#Config:incremental-eh-frame-added
//#Object:incremental-eh-frame-added-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-eh-frame-added.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_EH_FRAME_ADDED=1
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedSymbolBytes:incremental_eh_frame_added_value=0x2b000000
//#TestIncrementalStateContains:fde\t

#ifdef INCREMENTAL_EH_FRAME_ADDED
#define INCREMENTAL_EH_FRAME_ADDED_VALUE 43
#else
#define INCREMENTAL_EH_FRAME_ADDED_VALUE 42
#endif

__attribute__((section(".data.incremental_eh_frame_added"), used)) volatile int
    incremental_eh_frame_added_value = INCREMENTAL_EH_FRAME_ADDED_VALUE;

__attribute__((section(".text.incremental_eh_frame_added_primary"), noinline, used)) int
incremental_eh_frame_added_primary(void) {
    return incremental_eh_frame_added_value;
}

#ifdef INCREMENTAL_EH_FRAME_ADDED
__attribute__((section(".text.incremental_eh_frame_added_extra"), noinline, used)) int
incremental_eh_frame_added_extra(void) {
    return incremental_eh_frame_added_value + 1;
}
#endif

int unchanged(void);

void _start(void) {
    (void)incremental_eh_frame_added_primary();
#ifdef INCREMENTAL_EH_FRAME_ADDED
    (void)incremental_eh_frame_added_extra();
#endif
    (void)unchanged();
}
