__attribute__((visibility("hidden"))) int private_external_duplicate(void) {
  return 2;
}

int second_value(void) { return private_external_duplicate(); }
