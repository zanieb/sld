//#Config:incremental-merge-string
//#Object:incremental-merge-string-value.s
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedCompareFull:false
//#TestIncrementalChangedSymbolBytes:incremental_merge_string_value=0x6365666f726500
//#TestIncrementalChangedInput:incremental-merge-string-value.s.o
//#TestIncrementalChangedSection:.rodata.str1.1
//#TestIncrementalChangedExpectPatch:true
//#Config:no-string-merge:incremental-merge-string
//#SldExtraLinkArgs:--no-string-merge
//#TestIncrementalChangedCompareFull:true
//#TestIncrementalChangedExpectPatch:true

extern const char incremental_merge_string_value[];

const char* value(void) { return incremental_merge_string_value; }

void _start(void) { (void)value(); }
