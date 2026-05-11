//#Config:incremental-text
//#Object:incremental-text-value.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-text-value.c.o
//#TestIncrementalChangedSection:.text.incremental_text
//#Config:eh-frame:incremental-text
//#TestIncrementalChangedExpectPatch:false
//#TestIncrementalChangedFallbackReason:changed bytes outside patchable sections
//#TestIncrementalChangedSection:.rela.eh_frame
//#TestIncrementalChangedSectionOffset:16

int incremental_text_value(void);

void _start(void) { (void)incremental_text_value(); }
