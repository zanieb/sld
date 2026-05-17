//#Config:incremental-removed-input
//#Object:incremental-removed-input-unchanged.c
//#Object:incremental-removed-input-extra.c
//#LinkArgs:--no-gc-sections
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalRemovedInput:incremental-removed-input-extra.c.o

volatile int incremental_removed_input_value = 42;

int value(void) { return incremental_removed_input_value; }

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
