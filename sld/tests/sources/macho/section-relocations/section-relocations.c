//#Object:runtime.c
//#ExpectSym:_section_target section="__sectrel_tgt"
//#RunEnabled:true

#include "../common/runtime.h"

static long section_target __attribute__((section("__DATA,__sectrel_tgt"))) =
    42;
static long* section_pointer = &section_target;

void main(void) { exit_syscall((int)*section_pointer); }
