//#Object:runtime.c
//#Arch:x86_64
//#RunEnabled:false
//#ExpectSectionType:.eh_frame=SHT_PROGBITS
//#NoProgramHeader:GNU_RELRO

#include "../common/runtime.h"

void _start(void) {
  runtime_init();
  exit_syscall(42);
}
