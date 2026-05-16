//#Object:runtime.c
//#CompArgs:-fcommon
//#ExpectSym:_common_counter section="__bss"
//#ExpectSym:_common_slots section="__bss"
//#RunEnabled:true

#include "../common/runtime.h"

int common_counter;
unsigned long common_slots[8192];

void main(void) {
  if (common_counter != 0) {
    exit_syscall(1);
  }

  for (int i = 0; i < 8192; ++i) {
    if (common_slots[i] != 0) {
      exit_syscall(2);
    }
  }

  common_counter = 42;
  common_slots[4096] = 7;
  if (common_counter != 42 || common_slots[4096] != 7) {
    exit_syscall(3);
  }

  exit_syscall(42);
}
