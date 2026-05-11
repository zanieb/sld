//#Object:runtime.c
//#ExpectSym:_main
//#TestUpdateInPlace:true
//#RunEnabled:true

//#Config:clang-driver:default
//#LinkerDriver:clang
//#LinkArgs:-nostdlib
//#TestUpdateInPlace:false

#include "../common/runtime.h"

void main(void) { exit_syscall(42); }
