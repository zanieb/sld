int retained_target(void);

int retained_live_entry(void) { return 35; }

__attribute__((used)) static int retained_call(void) {
  return retained_target();
}
