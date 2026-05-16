//#Object:runtime.c
//#LinkArgs:-framework CoreFoundation -lobjc
//#ExpectSym:_main section="__text"
//#Contains:/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation
//#RunEnabled:true

#include "../common/runtime.h"

#include <CoreFoundation/CoreFoundation.h>

void main(void) {
  CFStringRef value = CFSTR("sld");
  if (CFStringGetLength(value) == 4) {
    exit_syscall(42);
  }
  exit_syscall(1);
}
