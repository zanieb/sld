//#Object:runtime.c
//#ExpectSym:_main
//#EnableLinker:apple-ld
//#TestUpdateInPlace:true
//#RunEnabled:true

//#Config:clang-driver:default
//#LinkerDriver:clang
//#LinkArgs:-nostdlib
//#SkipLinker:apple-ld
//#TestUpdateInPlace:false

//#Config:no-fork:default
//#SldExtraLinkArgs:--no-fork
//#TestUpdateInPlace:false

#include "../common/runtime.h"

void main(void) { exit_syscall(42); }
