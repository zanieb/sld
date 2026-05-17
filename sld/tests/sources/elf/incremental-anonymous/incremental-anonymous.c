//#Config:incremental-anonymous
//#Object:incremental-anonymous-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedInput:incremental-anonymous.c.o
//#TestIncrementalChangedSection:.rodata..L__unnamed_1
//#SkipArch:riscv64
//#Config:incremental-anonymous-riscv64-fallback:incremental-anonymous
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch

__attribute__((section(".rodata..L__unnamed_1"),
               used)) volatile const int anonymous_value = 42;

int value(void) { return anonymous_value; }

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
