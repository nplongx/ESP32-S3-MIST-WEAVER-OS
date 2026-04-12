#include <stdint.h>
#include <stdbool.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/gpio.h"
#include "onewire.h"
#include "ds18b20.h"

#define DS18B20_GPIO 6

static bool is_initialized = false;
static onewire_addr_t sensor_addr;

#ifdef __cplusplus
extern "C" {
#endif

float read_ds18b20_temperature_from_c(void) {
    if (!is_initialized) {
        gpio_set_pull_mode(DS18B20_GPIO, GPIO_PULLUP_ONLY);
        onewire_search_t search;
        onewire_search_start(&search);
        if (onewire_search_next(&search, DS18B20_GPIO, &sensor_addr)) {
            is_initialized = true;
        } else {
            return -1000.0; 
        }
    }

    float temp = 0.0;
    esp_err_t res = ds18b20_measure_and_read(DS18B20_GPIO, sensor_addr, &temp);
    if (res == ESP_OK) {
        return temp;
    } else {
        is_initialized = false; 
        return -1000.0;
    }
}

#ifdef __cplusplus
}
#endif
