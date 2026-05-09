//#Config:incremental
//#Object:incremental-unchanged.c
//#RunEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true

int value(void) { return 42; }

int unchanged(void);

int main(void) { return value() + unchanged(); }
