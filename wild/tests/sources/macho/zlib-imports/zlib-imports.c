//#Object:runtime.c
//#Object:imports.s
//#LinkArgs:-lz
//#RunEnabled:true

void exit_syscall(int exit_code);

void main(void) { exit_syscall(42); }
