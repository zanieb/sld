//#Object:runtime.c
//#ExpectSym:_main section="__text"
//#ExpectSym:_message section="__const"
//#ExpectSym:_answer section="__const"
//#ExpectSym:_counter section="__data"
//#RunEnabled:true

#include "../common/runtime.h"

const char message[] = "hello";
const int answer = 42;
int counter = 7;

void main(void) {
  if (message[1] == 'e' && answer == 42 && counter == 7) {
    exit_syscall(42);
  }
  exit_syscall(1);
}
