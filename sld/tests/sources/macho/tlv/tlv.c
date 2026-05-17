//#ExpectSym:_main section="__text"
//#ExpectSym:_counter section="__thread_vars"
//#EnableLinker:apple-ld
//#ExpectSectionBytes:__thread_data=0x2a000000
//#RunEnabled:true

__thread int counter = 42;

void main(void) {
  __asm__ __volatile__(
      "mov x16, #1\n"
      "mov x0, %0\n"
      "svc #0x80\n"
      :
      : "r"(counter));
  __builtin_unreachable();
}
