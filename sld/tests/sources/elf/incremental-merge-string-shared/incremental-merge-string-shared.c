//#Config:incremental-merge-string-shared
//#Object:incremental-merge-string-shared-a.s
//#Object:incremental-merge-string-shared-b.s
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedInput:incremental-merge-string-shared-b.s.o
//#TestIncrementalChangedSection:.rodata.str1.1

extern const char incremental_merge_string_shared_a[];
extern const char incremental_merge_string_shared_b[];

const char* value_a(void) { return incremental_merge_string_shared_a; }
const char* value_b(void) { return incremental_merge_string_shared_b; }

void _start(void) {
  (void)value_a();
  (void)value_b();
}
