__attribute__((section(".text.incremental_text"), noinline, used)) int
incremental_text_value(void) {
  return 42;
}
