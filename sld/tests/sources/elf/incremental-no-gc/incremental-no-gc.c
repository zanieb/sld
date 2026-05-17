//#Config:incremental-no-gc
//#Object:incremental-no-gc-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalCompareFull:false
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-no-gc.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_NO_GC_CHANGED=1
//#TestIncrementalChangedSection:.data.incremental_no_gc_unused
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedCompareFull:false
//#TestIncrementalChangedSymbolBytes:incremental_no_gc_unused=0x2b000000
//#TestIncrementalStateContains:2e646174612e696e6372656d656e74616c5f6e6f5f67635f756e75736564

#ifdef INCREMENTAL_NO_GC_CHANGED
#define INCREMENTAL_NO_GC_VALUE 43
#else
#define INCREMENTAL_NO_GC_VALUE 42
#endif

__attribute__((section(".data.incremental_no_gc_unused"),
               used)) volatile int incremental_no_gc_unused =
    INCREMENTAL_NO_GC_VALUE;

int unchanged(void);

void _start(void) { (void)unchanged(); }
