//#Config:incremental
//#Object:incremental-value.c
//#RunEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedSection:__data

extern int value(void);

volatile int unchanged_value = 7;

int main(void) { return value() + unchanged_value; }
