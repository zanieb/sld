//#Config:incremental-text
//#Object:incremental-text-value.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-text-value.c.o
//#TestIncrementalChangedSection:.text.incremental_text
//#Config:eh-frame:incremental-text
//#TestIncrementalChangedSection:.rela.eh_frame
//#TestIncrementalChangedSectionOffset:16
//#TestIncrementalStateContains:fde\t
//#Config:eh-frame-hdr:eh-frame
//#SldExtraLinkArgs:--eh-frame-hdr
//#Config:eh-frame-data:eh-frame
//#TestIncrementalChangedSection:.eh_frame
//#TestIncrementalChangedSectionOffset:36

int incremental_text_value(void);

void _start(void) { (void)incremental_text_value(); }
