//#Arch:x86_64
//#CompArgs:-Wa,-mx86-used-note=no
//#DiffEnabled:false
//#LinkArgs:-nostdlib
//#RequiresCompilerFlags:-Wa,-mx86-used-note=no
//#RunEnabled:false
//#SkipLinker:ld
//#ExpectSectionBytes:.note.gnu.property=0x040000001000000005000000474e5500020000c004000000b100000000000000

.text
.globl _start
_start:
  .byte 0

.section ".note.gnu.property", "a", @note
.p2align 3
.long 4
.long 2f - 1f
.long 5
.asciz "GNU"
1:
.long 0xc0000002
.long 4
.long 0xb3
.p2align 3
.long 0xc0000002
.long 4
.long 0xb1
.p2align 3
2:
