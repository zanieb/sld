//#Config:incremental-merge-string-scattered
//#Object:incremental-merge-string-scattered-value.s
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedInput:incremental-merge-string-scattered-value.s.o
//#TestIncrementalChangedSection:.rodata.str1.1

extern const char incremental_merge_string_scattered_a[];
extern const char incremental_merge_string_scattered_c[];

const char* value_a(void) { return incremental_merge_string_scattered_a; }
const char* value_c(void) { return incremental_merge_string_scattered_c; }

void _start(void) {
  (void)value_a();
  (void)value_c();
}
