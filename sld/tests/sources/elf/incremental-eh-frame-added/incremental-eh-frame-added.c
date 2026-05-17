//#AbstractConfig:incremental-eh-frame-added-base
//#Object:incremental-eh-frame-added-unchanged.c
//#LinkArgs:--eh-frame-hdr --no-gc-sections
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-eh-frame-added.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_EH_FRAME_ADDED=1
//#TestIncrementalChangedSection:.data.incremental_eh_frame_added
//#TestIncrementalChangedSection:.text.incremental_eh_frame_added
//#TestIncrementalChangedSection:generated:.eh_frame
//#TestIncrementalChangedSection:generated:.eh_frame_hdr
//#TestIncrementalChangedSymbolBytes:incremental_eh_frame_added_value=0x2b000000
//#TestIncrementalStateContains:fde\t
//#Config:incremental-eh-frame-added:incremental-eh-frame-added-base
//#SldExtraLinkArgs:--incremental-padding-percent=100
//#TestIncrementalCompareFull:false
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedCompareFull:false
//#Config:incremental-eh-frame-added-no-padding:incremental-eh-frame-added-base
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:could not resolve patchable sections

#ifdef INCREMENTAL_EH_FRAME_ADDED
#define INCREMENTAL_EH_FRAME_ADDED_VALUE 43
#else
#define INCREMENTAL_EH_FRAME_ADDED_VALUE 42
#endif

__attribute__((section(".data.incremental_eh_frame_added"),
               used)) volatile int incremental_eh_frame_added_value =
    INCREMENTAL_EH_FRAME_ADDED_VALUE;

__attribute__((section(".text.incremental_eh_frame_added"), noinline,
               used)) int
incremental_eh_frame_added_primary(void) {
  return INCREMENTAL_EH_FRAME_ADDED_VALUE;
}

#ifdef INCREMENTAL_EH_FRAME_ADDED
__attribute__((section(".text.incremental_eh_frame_added"), noinline,
               used)) int incremental_eh_frame_added_extra(void) {
  return 1;
}
#endif

int unchanged(void);

void _start(void) {
  (void)incremental_eh_frame_added_primary();
  (void)unchanged();
}
