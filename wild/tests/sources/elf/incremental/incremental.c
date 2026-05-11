//#Config:incremental
//#Object:incremental-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalInterrupted:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental.c.o
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedSymbolBytes:incremental_value=0x2b000000

volatile int incremental_value = 42;

int value(void) { return incremental_value; }

int unchanged(void);

void _start(void) {
    (void)value();
    (void)unchanged();
}
