//#Config:incremental-eh-frame-removed
//#Object:incremental-eh-frame-removed-unchanged.c
//#LinkArgs:--eh-frame-hdr --no-gc-sections
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalCompareFull:false
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-eh-frame-removed.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_EH_FRAME_REMOVED=1
//#TestIncrementalChangedSection:.data.incremental_eh_frame_removed
//#TestIncrementalChangedSection:generated:.eh_frame
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedFallbackReason:relocation target moved
//#TestIncrementalChangedPatchedSectionCount:1
//#TestIncrementalChangedPatchedSectionCount:2
//#TestIncrementalChangedCompareFull:false
//#TestIncrementalChangedSymbolBytes:incremental_eh_frame_removed_value=0x2b000000
//#TestIncrementalStateContains:fde\t
//#SkipArch:aarch64,loongarch64,riscv64
//#Config:incremental-eh-frame-removed-aarch64:incremental-eh-frame-removed
//#Arch:aarch64
//#TestIncrementalChangedSection:.data.incremental_eh_frame_removed
//#Config:incremental-eh-frame-removed-loongarch64:incremental-eh-frame-removed
//#Arch:loongarch64
//#TestIncrementalChangedSection:.data.incremental_eh_frame_removed
//#Config:incremental-eh-frame-removed-riscv64-fallback:incremental-eh-frame-removed
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:relocation target moved
#ifdef INCREMENTAL_EH_FRAME_REMOVED
#define INCREMENTAL_EH_FRAME_REMOVED_VALUE 43
#else
#define INCREMENTAL_EH_FRAME_REMOVED_VALUE 42
#endif

__attribute__((section(".data.incremental_eh_frame_removed"),
               used)) volatile int incremental_eh_frame_removed_value =
    INCREMENTAL_EH_FRAME_REMOVED_VALUE;

__attribute__((section(".text.incremental_eh_frame_removed_primary"), noinline,
               used)) int
incremental_eh_frame_removed_primary(void) {
  return incremental_eh_frame_removed_value;
}

#ifndef INCREMENTAL_EH_FRAME_REMOVED
// clang-format off
__attribute__((section(".text.incremental_eh_frame_removed_extra"), noinline,
               used)) int
incremental_eh_frame_removed_extra(void) {
  return incremental_eh_frame_removed_value + 1;
}
// clang-format on
#endif

int unchanged(void);

void _start(void) {
  (void)incremental_eh_frame_removed_primary();
  (void)unchanged();
}
