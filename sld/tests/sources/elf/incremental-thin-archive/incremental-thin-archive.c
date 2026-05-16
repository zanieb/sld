//#Config:incremental-thin-archive
//#ThinArchive:incremental-thin-archive-member.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-thin-archive-member.c.o
//#TestIncrementalChangedSection:.data

int value(void);

void _start(void) { (void)value(); }
