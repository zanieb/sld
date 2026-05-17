//#ExpectSym:_main section="__text"
//#ExpectSectionBytes:__got=0x0000000000000080
//#RunEnabled:true

extern int printf(const char*, ...);

void main(void) {
  printf("hi\n");
  __asm__ __volatile__(
      "mov x16, #1\n"
      "mov x0, #42\n"
      "svc #0x80\n");
  __builtin_unreachable();
}
