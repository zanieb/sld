//#Object:runtime.c
//#Object:classes.s
//#LinkArgs:-framework CoreFoundation -framework Foundation -framework AppKit -lobjc
//#RunEnabled:true

void exit_syscall(int exit_code);

void main(void) { exit_syscall(42); }
