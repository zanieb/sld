//#Config:incremental-strict
//#Object:incremental-strict-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedExpectReuse:true
//#TestIncrementalChangedInput:incremental-strict.c.o
//#TestIncrementalChangedSection:.init
//#SkipArch:riscv64
//#Config:incremental-strict-riscv64-fallback:incremental-strict
//#Arch:riscv64
//#TestIncrementalChangedFallbackReason:relocation target moved
//#Config:init-array:incremental-strict
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedExpectReuse:false
//#TestIncrementalChangedSection:.rela.init_array
//#TestIncrementalChangedSectionOffset:16
//#SkipArch:riscv64
//#Config:init-array-riscv64-fallback:init-array
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:relocation target moved

__attribute__((section(".init"), used)) void incremental_strict_init(void) {
  __asm__ volatile("nop");
}

static void incremental_strict_init_array_target(void) {}

__attribute__((section(".init_array"),
               used)) void (*incremental_strict_init_array)(void) =
    incremental_strict_init_array_target;

volatile int incremental_strict_value = 7;

int value(void) { return incremental_strict_value; }

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
