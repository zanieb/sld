//#Config:incremental-dynamic-relocation
//#Mode:dynamic
//#Object:incremental-dynamic-relocation-unchanged.c
//#Shared:incremental-dynamic-relocation-shared.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-dynamic-relocation.c.o
//#TestIncrementalChangedSection:.rela.data.rel.incremental_dynamic
//#TestIncrementalChangedSectionOffset:16
//#TestIncrementalStateContains:dynrel\t
//#SkipArch:riscv64
//#Config:incremental-dynamic-relocation-riscv64-fallback:incremental-dynamic-relocation
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch

extern int dynamic_relocation_target;

struct IncrementalDynamicPayload {
  volatile int value;
  void* pointer;
};

__attribute__((
    section(".data.rel.incremental_dynamic"),
    used)) struct IncrementalDynamicPayload incremental_dynamic_payload = {
    42, &dynamic_relocation_target};

int value(void) {
  return incremental_dynamic_payload.value +
         (incremental_dynamic_payload.pointer != 0);
}

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
