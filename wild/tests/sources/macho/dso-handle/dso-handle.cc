//#Object:runtime.c
//#RunEnabled:true

extern "C" void exit_syscall(int exit_code);

struct NeedsDsoHandle {
  ~NeedsDsoHandle();
};

NeedsDsoHandle::~NeedsDsoHandle() {}

static NeedsDsoHandle needs_dso_handle;

int main(void) { exit_syscall(42); }
