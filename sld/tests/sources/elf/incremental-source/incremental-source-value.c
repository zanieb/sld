__attribute__((section(".data.incremental_source"),
               used)) volatile int incremental_source_value =
#ifdef INCREMENTAL_SOURCE_CHANGED
    43;
#else
    42;
#endif
