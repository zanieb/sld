//#Config:incremental-dynamic-relocation-removed
//#Mode:dynamic
//#Object:incremental-dynamic-relocation-removed-unchanged.c
//#Shared:incremental-dynamic-relocation-removed-shared.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-dynamic-relocation-removed.c.o
//#TestIncrementalChangedCompArgs:-DINCREMENTAL_DYNAMIC_RELOCATION_REMOVED=1
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedSymbolBytes:incremental_dynamic_removed_payload=0x2b000000
//#TestIncrementalStateContains:dynrel\t

extern int dynamic_relocation_removed_target;

struct IncrementalDynamicRemovedPayload {
    volatile int value;
    void *pointer;
};

#ifdef INCREMENTAL_DYNAMIC_RELOCATION_REMOVED
#define INCREMENTAL_DYNAMIC_REMOVED_VALUE 43
#define INCREMENTAL_DYNAMIC_REMOVED_POINTER 0
#else
#define INCREMENTAL_DYNAMIC_REMOVED_VALUE 42
#define INCREMENTAL_DYNAMIC_REMOVED_POINTER (&dynamic_relocation_removed_target)
#endif

__attribute__((section(".data.rel.incremental_dynamic_removed"), used)) struct
    IncrementalDynamicRemovedPayload incremental_dynamic_removed_payload = {
        INCREMENTAL_DYNAMIC_REMOVED_VALUE, INCREMENTAL_DYNAMIC_REMOVED_POINTER};

int value(void) {
    return incremental_dynamic_removed_payload.value +
           (incremental_dynamic_removed_payload.pointer != 0);
}

int unchanged(void);

void _start(void) {
    (void)value();
    (void)unchanged();
}
