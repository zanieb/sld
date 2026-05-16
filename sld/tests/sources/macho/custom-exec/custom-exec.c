//#Object:runtime.c
//#Object:custom.s
//#ExpectSym:_custom_exit_code section="__custom"
//#RunEnabled:true

#include "../common/runtime.h"

int custom_exit_code(void);

void main(void) { exit_syscall(custom_exit_code()); }
