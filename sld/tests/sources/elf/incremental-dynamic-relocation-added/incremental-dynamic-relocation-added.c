//#AbstractConfig:incremental-dynamic-relocation-added-base
//#Mode:dynamic
//#Object:incremental-dynamic-relocation-added-unchanged.c
//#Shared:incremental-dynamic-relocation-added-shared.c
//#LinkArgs:--no-gc-sections
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-dynamic-relocation-added.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_DYNAMIC_RELOCATION_ADDED=1
//#TestIncrementalChangedSection:.data.rel.incremental_dynamic_added
//#TestIncrementalChangedSection:.rela.data.rel.incremental_dynamic_added
//#TestIncrementalChangedSection:generated:.rela.dyn.general
//#TestIncrementalChangedSymbolBytes:incremental_dynamic_added_payload=0x2b000000
//#TestIncrementalStateContains:dynrel\t
//#Config:incremental-dynamic-relocation-added:incremental-dynamic-relocation-added-base
//#SldExtraLinkArgs:--incremental-padding-percent=100
//#TestIncrementalCompareFull:false
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedCompareFull:false
//#TestIncrementalChangedRestore:true
//#SkipArch:riscv64
//#Config:incremental-dynamic-relocation-added-riscv64-fallback:incremental-dynamic-relocation-added
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch
//#Config:incremental-dynamic-relocation-added-no-padding:incremental-dynamic-relocation-added-base
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections

extern int dynamic_relocation_added_target;

struct IncrementalDynamicAddedPayload {
  volatile int value;
  void* existing_pointer;
  void* pointer;
};

#ifdef INCREMENTAL_DYNAMIC_RELOCATION_ADDED
#define INCREMENTAL_DYNAMIC_ADDED_VALUE 43
#define INCREMENTAL_DYNAMIC_ADDED_POINTER (&dynamic_relocation_added_target)
#else
#define INCREMENTAL_DYNAMIC_ADDED_VALUE 42
#define INCREMENTAL_DYNAMIC_ADDED_POINTER 0
#endif

__attribute__((section(".data.rel.incremental_dynamic_added"),
               used)) struct IncrementalDynamicAddedPayload
    incremental_dynamic_added_payload = {INCREMENTAL_DYNAMIC_ADDED_VALUE,
                                         &dynamic_relocation_added_target,
                                         INCREMENTAL_DYNAMIC_ADDED_POINTER};

int value(void) {
  return incremental_dynamic_added_payload.value +
         (incremental_dynamic_added_payload.existing_pointer != 0) +
         (incremental_dynamic_added_payload.pointer != 0);
}

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
