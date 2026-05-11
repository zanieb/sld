//#Config:incremental
//#Object:incremental-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalInterrupted:true
//#TestIncrementalChanged:true

volatile int incremental_value = 42;

int value(void) { return incremental_value; }

int unchanged(void);

void _start(void) {
    (void)value();
    (void)unchanged();
}
