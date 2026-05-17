//#Config:incremental-archive
//#Archive:incremental-archive-member.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-archive-member.a
//#TestIncrementalChangedSection:.data
//#SkipArch:riscv64
//#Config:incremental-archive-riscv64-fallback:incremental-archive
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch
//#Config:archive-membership:incremental-archive
//#TestIncrementalChangedAppendArchiveMember:true
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:archive members changed

int value(void);

void _start(void) { (void)value(); }
