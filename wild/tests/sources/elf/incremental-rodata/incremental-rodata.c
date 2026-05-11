//#Config:incremental
//#Object:incremental-rodata-value.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-rodata-value.c.o
//#TestIncrementalChangedSection:.rodata

extern const unsigned char incremental_rodata_value[4];

int value(void) { return incremental_rodata_value[0]; }

void _start(void) { (void)value(); }
