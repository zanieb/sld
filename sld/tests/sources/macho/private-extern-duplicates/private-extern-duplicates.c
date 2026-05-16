//#Object:runtime.c
//#Archive:first.c
//#Archive:second.c
//#LinkArgs:-dead_strip
//#DiffEnabled:false
//#RunEnabled:true

#include "../common/runtime.h"

int first_value(void);
int second_value(void);

void main(void) {
  int value = first_value() + second_value();
  if (value != 2) {
    exit_syscall(value);
  }
  exit_syscall(42);
}
