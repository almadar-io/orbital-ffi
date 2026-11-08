#include <os/log.h>

void orbital_ffi_os_log(int level, const char *line) {
    os_log_type_t type = level >= 4 ? OS_LOG_TYPE_FAULT
        : level == 3 ? OS_LOG_TYPE_ERROR
        : level == 2 ? OS_LOG_TYPE_DEFAULT
        : level == 1 ? OS_LOG_TYPE_INFO
        : OS_LOG_TYPE_DEBUG;
    os_log_with_type(OS_LOG_DEFAULT, type, "%{public}s", line);
}
