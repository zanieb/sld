//#Object:retained.c
//#Object:tail.c
//#LinkArgs:-dead_strip -dynamiclib -Wl,-exported_symbols_list -Wl,./exports.list
//#RunEnabled:false

int loaded_marker(void);
int retained_live_entry(void);

int exported_value(void) { return loaded_marker() + retained_live_entry(); }
