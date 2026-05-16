//#Arch:aarch64
//#LinkerDriver:gcc
//#LinkArgs:-nostdlib -Wl,--section-start=.low=0x10000000,--section-start=.high=0x20000000
//#RunEnabled:false
//#SkipLinker:ld
//#ExpectSym:__thunk_fn1 section=".text"
//#ExpectSym:__thunk_fn3 section=".low"
//#MaxThunks:4

.section .text,"ax"
.globl _start
_start:
  bl fn1
  b .

.section .low,"ax"
.globl fn1
fn1:
  bl fn3
  ret

.section .high,"ax"
.globl fn3
fn3:
  ret
