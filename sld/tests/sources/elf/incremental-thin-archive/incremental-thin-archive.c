//#Config:incremental-thin-archive
//#ThinArchive:incremental-thin-archive-member.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-thin-archive-member.c.o
//#TestIncrementalChangedSection:.data
//#SkipArch:riscv64
//#Config:incremental-thin-archive-riscv64-fallback:incremental-thin-archive
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch

int value(void);

void _start(void) { (void)value(); }
