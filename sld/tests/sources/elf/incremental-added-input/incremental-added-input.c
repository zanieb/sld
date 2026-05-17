//#Config:incremental-added-input
//#Object:incremental-added-input-unchanged.c
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalAddedInput:incremental-added-input-extra.c

volatile int incremental_added_input_value = 42;

int value(void) { return incremental_added_input_value; }

int unchanged(void);

void _start(void) {
  (void)value();
  (void)unchanged();
}
