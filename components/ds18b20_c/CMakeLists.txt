idf_component_register(SRCS "ds18b20_c.c"
                       INCLUDE_DIRS "."
                       REQUIRES driver freertos onewire ds18b20)
