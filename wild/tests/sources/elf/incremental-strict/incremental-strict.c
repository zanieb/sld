//#Config:incremental-strict
//#Object:incremental-strict-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedInput:incremental-strict.c.o
//#TestIncrementalChangedSection:.init

__attribute__((section(".init"), used)) void incremental_strict_init(void) {
    __asm__ volatile("nop");
}

volatile int incremental_strict_value = 7;

int value(void) { return incremental_strict_value; }

int unchanged(void);

void _start(void) {
    (void)value();
    (void)unchanged();
}
