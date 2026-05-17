extern volatile int incremental_relocation_target_moved;

__attribute__((section(".data.rel.local.incremental_target_ref"),
               used)) volatile int* incremental_relocation_target_ref =
    &incremental_relocation_target_moved;

int incremental_relocation_target_ref_value(void) {
  return *incremental_relocation_target_ref;
}
