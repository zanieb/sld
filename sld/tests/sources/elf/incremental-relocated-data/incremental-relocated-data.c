//#Config:incremental-relocated-data
//#Object:incremental-relocated-data-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedInput:incremental-relocated-data.c.o
//#TestIncrementalChangedSection:.data.rel.local.incremental_relocated
//#TestIncrementalStateContains:reloc2\t
//#TestIncrementalStateContains:72656c6f63617465645f746172676574
//#SkipArch:riscv64
//#Config:incremental-relocated-data-riscv64-fallback:incremental-relocated-data
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch
//#Config:relocation-metadata:incremental-relocated-data
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedSection:.rela.data.rel.local.incremental_relocated
//#TestIncrementalChangedSectionOffset:16
//#SkipArch:riscv64
//#Config:relocation-metadata-riscv64-fallback:relocation-metadata
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch

extern int relocated_target;

struct IncrementalRelocatedPayload {
  volatile int value;
  void* pointer;
};

__attribute__((
    section(".data.rel.local.incremental_relocated"),
    used)) struct IncrementalRelocatedPayload incremental_relocated_payload = {
    42, &relocated_target};

int value(void) {
  return incremental_relocated_payload.value +
         (incremental_relocated_payload.pointer != 0);
}

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
