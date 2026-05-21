int loaded_marker(void) { return 42; }

__attribute__((visibility("hidden"))) int retained_target(void) { return 7; }
