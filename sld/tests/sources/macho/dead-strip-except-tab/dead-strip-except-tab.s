//#Object:runtime.c
//#LinkArgs:-dead_strip
//#ExpectSym:_main section="__text",offset-in-section=0
//#ExpectSym:_live_except section="__gcc_except_tab"
//#NoSym:_dead_except
//#RunEnabled:true

.subsections_via_symbols

.section __TEXT,__text
.p2align 2
.globl _main
_main:
    adrp x9, _live_except@PAGE
    add x9, x9, _live_except@PAGEOFF
    mov w0, #42
    b _exit_syscall

.p2align 2
_dead:
    adrp x9, _dead_except@PAGE
    add x9, x9, _dead_except@PAGEOFF
    mov w0, #13
    b _exit_syscall

.section __TEXT,__gcc_except_tab
.p2align 3
.globl _live_except
_live_except:
    .quad 0x1111

.p2align 3
.globl _dead_except
_dead_except:
    .quad 0x2222
