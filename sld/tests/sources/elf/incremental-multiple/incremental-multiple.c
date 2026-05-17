//#Config:incremental-multiple
//#Object:incremental-multiple-a.c
//#Object:incremental-multiple-b.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-multiple-a.c.o
//#TestIncrementalChangedInput:incremental-multiple-b.c.o
//#SkipArch:riscv64
//#Config:incremental-multiple-riscv64-fallback:incremental-multiple
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch

int value_a(void);
int value_b(void);

void _start(void) {
  (void)value_a();
  (void)value_b();
}
