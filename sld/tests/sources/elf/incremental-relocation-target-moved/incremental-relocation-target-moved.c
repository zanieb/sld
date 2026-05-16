//#Config:incremental-relocation-target-moved
//#Object:incremental-relocation-target-moved-target.c
//#Object:incremental-relocation-target-moved-ref.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-relocation-target-moved-target.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_TARGET_MOVED=1
//#TestIncrementalChangedSection:.data.incremental_target_moved
//#TestIncrementalChangedExpectPatch:true

int incremental_relocation_target_ref_value(void);

void _start(void) { (void)incremental_relocation_target_ref_value(); }
