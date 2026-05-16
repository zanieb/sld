//#Object:runtime.c
//#ExpectSym:_zero section="__bss"
//#RunEnabled:true

#include "../common/runtime.h"

static int zero;

void main(void) {
  zero = 42;
  exit_syscall(zero);
}
