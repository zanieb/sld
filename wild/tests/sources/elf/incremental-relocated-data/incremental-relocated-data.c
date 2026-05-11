//#Config:incremental-relocated-data
//#Object:incremental-relocated-data-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:true
//#TestIncrementalChangedInput:incremental-relocated-data.c.o
//#TestIncrementalChangedSection:.data.rel.local.incremental_relocated

extern int relocated_target;

struct IncrementalRelocatedPayload {
    volatile int value;
    void *pointer;
};

__attribute__((section(".data.rel.local.incremental_relocated"), used)) struct
    IncrementalRelocatedPayload incremental_relocated_payload = {42, &relocated_target};

int value(void) {
    return incremental_relocated_payload.value + (incremental_relocated_payload.pointer != 0);
}

int unchanged(void);

void _start(void) {
    (void)value();
    (void)unchanged();
}
