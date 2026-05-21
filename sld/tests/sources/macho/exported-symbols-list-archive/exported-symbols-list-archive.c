//#Archive:listed.c
//#LinkArgs:-dynamiclib -Wl,-exported_symbols_list -Wl,./exports.list
//#ExpectSym:_listed_value section="__text"
//#RunEnabled:false

int anchor(void) { return 0; }
