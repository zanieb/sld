//#Config:incremental-grow-fallback
//#Object:incremental-grow-fallback-data.s
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-grow-fallback-data.s.o
//#TestIncrementalChangedSection:.data.incremental_grow_fallback
//#TestIncrementalChangedGrowSection:1
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:could not resolve patchable sections
//#TestIncrementalChangedSymbolBytes:incremental_grow_fallback_value=0x0102030480
//#Config:writer-bootstrap-fallback-relink:incremental-grow-fallback
//#SldExtraLinkArgs:--threads=1

extern volatile unsigned char incremental_grow_fallback_value[];

volatile unsigned char incremental_grow_fallback_sink;

void _start(void) {
  incremental_grow_fallback_sink = incremental_grow_fallback_value[0];
}
