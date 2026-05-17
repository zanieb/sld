//#Config:incremental-text
//#Object:incremental-text-value.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-text-value.c.o
//#TestIncrementalChangedSection:.text.incremental_text
//#Config:eh-frame:incremental-text
//#TestIncrementalChangedSection:.rela.eh_frame
//#TestIncrementalChangedSectionOffset:16
//#TestIncrementalStateContains:fde\t
//#SkipArch:riscv64,loongarch64
//#Config:eh-frame-cross-fallback:eh-frame
//#Arch:riscv64,loongarch64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#Config:eh-frame-hdr:eh-frame
//#SldExtraLinkArgs:--eh-frame-hdr
//#SkipArch:riscv64,loongarch64
//#Config:eh-frame-hdr-cross-fallback:eh-frame-hdr
//#Arch:riscv64,loongarch64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#Config:eh-frame-data:eh-frame
//#TestIncrementalChangedSection:.eh_frame
//#TestIncrementalChangedSectionOffset:36
//#SkipArch:riscv64,loongarch64
//#Config:eh-frame-data-cross-fallback:eh-frame-data
//#Arch:riscv64,loongarch64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections

int incremental_text_value(void);

void _start(void) { (void)incremental_text_value(); }
