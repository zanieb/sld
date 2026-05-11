//#Config:incremental-eh-frame-removed
//#Object:incremental-eh-frame-removed-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-eh-frame-removed.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_EH_FRAME_REMOVED=1
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedSymbolBytes:incremental_eh_frame_removed_value=0x2b000000
//#TestIncrementalStateContains:fde\t

#ifdef INCREMENTAL_EH_FRAME_REMOVED
#define INCREMENTAL_EH_FRAME_REMOVED_VALUE 43
#else
#define INCREMENTAL_EH_FRAME_REMOVED_VALUE 42
#endif

__attribute__((section(".data.incremental_eh_frame_removed"), used)) volatile int
    incremental_eh_frame_removed_value = INCREMENTAL_EH_FRAME_REMOVED_VALUE;

__attribute__((section(".text.incremental_eh_frame_removed_primary"), noinline, used)) int
incremental_eh_frame_removed_primary(void) {
    return incremental_eh_frame_removed_value;
}

#ifndef INCREMENTAL_EH_FRAME_REMOVED
__attribute__((section(".text.incremental_eh_frame_removed_extra"), noinline, used)) int
incremental_eh_frame_removed_extra(void) {
    return incremental_eh_frame_removed_value + 1;
}
#endif

int unchanged(void);

void _start(void) {
    (void)incremental_eh_frame_removed_primary();
#ifndef INCREMENTAL_EH_FRAME_REMOVED
    (void)incremental_eh_frame_removed_extra();
#endif
    (void)unchanged();
}
