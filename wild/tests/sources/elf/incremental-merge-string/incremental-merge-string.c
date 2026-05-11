//#Config:incremental-merge-string
//#Object:incremental-merge-string-value.s
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:missing patch metadata
//#TestIncrementalChangedInput:incremental-merge-string-value.s.o
//#TestIncrementalChangedSection:.rodata.str1.1
//#Config:no-string-merge:incremental-merge-string
//#WildExtraLinkArgs:--no-string-merge
//#TestIncrementalChangedExpectPatch:true

extern const char incremental_merge_string_value[];

const char *value(void) { return incremental_merge_string_value; }

void _start(void) { (void)value(); }
