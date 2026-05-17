//#Config:incremental-reordered-inputs
//#Object:incremental-reordered-inputs-a.c
//#Object:incremental-reordered-inputs-b.c
//#LinkArgs:--no-gc-sections
//#RunEnabled:false
//#DiffEnabled:false
//#TestIncremental:true
//#TestIncrementalReorderedInputs:true

extern volatile int incremental_reordered_inputs_a;
extern volatile int incremental_reordered_inputs_b;

void _start(void) {
  (void)incremental_reordered_inputs_a;
  (void)incremental_reordered_inputs_b;
}
