//#Config:incremental-multiple
//#Object:incremental-multiple-a.c
//#Object:incremental-multiple-b.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalChanged:true
//#TestIncrementalChangedInput:incremental-multiple-a.c.o
//#TestIncrementalChangedInput:incremental-multiple-b.c.o

int value_a(void);
int value_b(void);

void _start(void) {
    (void)value_a();
    (void)value_b();
}
