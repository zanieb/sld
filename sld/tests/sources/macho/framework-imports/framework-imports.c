//#Object:runtime.c
//#Object:imports.s
//#LinkArgs:-framework AudioToolbox -framework CoreAudio -framework CoreGraphics -framework CoreMedia -framework CoreVideo -framework VideoToolbox -framework IOKit -framework IOSurface -framework CoreServices -framework Security -framework ScreenCaptureKit
//#RunEnabled:true

void exit_syscall(int exit_code);

void main(void) { exit_syscall(42); }
