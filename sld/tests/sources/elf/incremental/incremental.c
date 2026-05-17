//#Config:incremental
//#Object:incremental-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalInterrupted:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental.c.o
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedSymbolBytes:incremental_value=0x2b000000
//#SkipArch:riscv64
//#Config:incremental-riscv64-fallback:incremental
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch

volatile int incremental_value = 42;

int value(void) { return incremental_value; }

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
