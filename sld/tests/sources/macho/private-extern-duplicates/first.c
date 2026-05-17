__attribute__((visibility("hidden"))) int private_external_duplicate(void) {
  return 1;
}

int first_value(void) { return private_external_duplicate(); }
