//#Object:runtime.c
//#LinkArgs:-framework CoreFoundation -lobjc
//#ExpectSym:_main section="__text"
//#Contains:/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation
//#RunEnabled:true

#include <CoreFoundation/CoreFoundation.h>

#include "../common/runtime.h"

void main(void) {
  CFStringRef value = CFSTR("sld");
  if (CFStringGetLength(value) == 3) {
    exit_syscall(42);
  }
  exit_syscall(1);
}
