//#Config:incremental
//#Object:incremental-grow-data.s
//#RunEnabled:false
//#DiffEnabled:false
//#SldExtraLinkArgs:--incremental-padding-percent=100
//#TestIncremental:true
//#TestIncrementalCompareFull:false
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-grow-data.s.o
//#TestIncrementalChangedSection:.data.incremental_grow
//#TestIncrementalChangedGrowSection:1
//#TestIncrementalChangedCompareFull:false
//#TestIncrementalChangedSymbolBytes:incremental_grow_value=0x0102030480

extern volatile unsigned char incremental_grow_value[];

volatile unsigned char incremental_grow_sink;

void _start(void) { incremental_grow_sink = incremental_grow_value[0]; }
