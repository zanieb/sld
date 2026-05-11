//#Config:incremental-dynamic-relocation-added
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
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedSymbolBytes:incremental_dynamic_added_payload=0x2b000000

extern int dynamic_relocation_added_target;

struct IncrementalDynamicAddedPayload {
    volatile int value;
    void *pointer;
};

#ifdef INCREMENTAL_DYNAMIC_RELOCATION_ADDED
#define INCREMENTAL_DYNAMIC_ADDED_VALUE 43
#define INCREMENTAL_DYNAMIC_ADDED_POINTER (&dynamic_relocation_added_target)
#else
#define INCREMENTAL_DYNAMIC_ADDED_VALUE 42
#define INCREMENTAL_DYNAMIC_ADDED_POINTER 0
#endif

__attribute__((section(".data.rel.incremental_dynamic_added"), used)) struct
    IncrementalDynamicAddedPayload incremental_dynamic_added_payload = {
        INCREMENTAL_DYNAMIC_ADDED_VALUE, INCREMENTAL_DYNAMIC_ADDED_POINTER};

int value(void) {
    return incremental_dynamic_added_payload.value +
           (incremental_dynamic_added_payload.pointer != 0);
}

int unchanged(void);

void _start(void) {
    (void)value();
    (void)unchanged();
}
