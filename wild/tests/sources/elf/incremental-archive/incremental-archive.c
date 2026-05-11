//#Config:incremental-archive
//#Archive:incremental-archive-member.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-archive-member.a
//#TestIncrementalChangedSection:.data

int value(void);

void _start(void) { (void)value(); }
