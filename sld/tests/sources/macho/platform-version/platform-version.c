//#Object:runtime.c
//#SkipLinker:lld
//#SldExtraLinkArgs:-platform_version macos 13.0 14.4
//#ExpectMachOBuildVersion:macos 13.0 14.4
//#RunEnabled:true

#include "../common/runtime.h"

void main(void) { exit_syscall(42); }
