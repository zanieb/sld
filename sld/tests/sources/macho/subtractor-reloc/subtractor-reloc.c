//#Object:runtime.c
//#Object:subtractor.s
//#RunEnabled:true
//#DiffEnabled:false

#include "../common/runtime.h"

extern unsigned long subtractor_value;

void main(void) {
  if (subtractor_value != 13) {
    exit_syscall(1);
  }

  exit_syscall(42);
}
