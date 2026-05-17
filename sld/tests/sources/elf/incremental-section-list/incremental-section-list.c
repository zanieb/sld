//#Config:incremental-section-list
//#Object:incremental-section-list-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-section-list.c.o
//#TestIncrementalChangedSection:.data.incremental_section_a
//#TestIncrementalChangedSection:.data.incremental_section_b
//#SkipArch:riscv64
//#Config:incremental-section-list-riscv64-fallback:incremental-section-list
//#Arch:riscv64
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing output symbol for incremental value patch

__attribute__((section(".data.incremental_section_a"),
               used)) volatile unsigned char incremental_section_a = 42;
__attribute__((section(".data.incremental_section_b"),
               used)) volatile unsigned char incremental_section_b = 7;

int unchanged(void);

int value(void) { return incremental_section_a + incremental_section_b; }

void _start(void) {
  (void)value();
  (void)unchanged();
}
