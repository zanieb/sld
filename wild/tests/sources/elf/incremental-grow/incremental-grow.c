//#Config:incremental
//#Object:incremental-grow-data.s
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-grow-data.s.o
//#TestIncrementalChangedSection:.data.incremental_grow
//#TestIncrementalChangedGrowSection:1

extern volatile unsigned char incremental_grow_value[];

volatile unsigned char incremental_grow_sink;

void _start(void) { incremental_grow_sink = incremental_grow_value[0]; }
