//#Config:incremental-source
//#Object:incremental-source-value.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-source-value.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_SOURCE_CHANGED=1
//#TestIncrementalChangedSection:.data.incremental_source
//#TestIncrementalChangedSymbolBytes:incremental_source_value=0x2b000000

extern volatile int incremental_source_value;

void _start(void) { (void)incremental_source_value; }
