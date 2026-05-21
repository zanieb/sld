//#Object:tail.s
//#ExpectSectionBytes:__eh_frame=0x0800000000000000443322110400000000000000
//#RunEnabled:false

.section __TEXT,__text
.globl _main
_main:
    mov w0, #0
    ret

.section __TEXT,__eh_frame
.p2align 3
_first_cie:
    .long 8
    .long 0
    .long 0x11223344
    .long 0
