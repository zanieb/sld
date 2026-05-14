//#Arch:x86_64
//#RunEnabled:false
//#ExpectSectionTypeWild:.eh_frame=SHT_PROGBITS

.text
.globl _start
_start:
.cfi_startproc
  ret
.cfi_endproc

.section .eh_frame,"a",@unwind
