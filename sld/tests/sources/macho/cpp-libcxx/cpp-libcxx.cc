//#Object:runtime.c
//#LinkArgs:-lc++
//#RunEnabled:true

#include <iostream>

extern "C" void exit_syscall(int exit_code);

int main(void) {
  std::cout.flush();
  exit_syscall(42);
}
