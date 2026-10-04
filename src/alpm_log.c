/* The only C shim handles libalpm's variadic log callback. Zig compiles it. */
#include <alpm.h>
#include <stdarg.h>
#include <stdio.h>

static void log_message(void *ctx, alpm_loglevel_t level,
                        const char *format, va_list args) {
    (void)ctx;
    if (!(level & (ALPM_LOG_ERROR | ALPM_LOG_WARNING))) return;
    char message[4096];
    int length = vsnprintf(message, sizeof(message), format, args);
    if (length < 0) return;
    for (size_t i = 0; i < sizeof(message) && message[i]; ++i) {
        unsigned char ch = (unsigned char)message[i];
        if ((ch < 32 && ch != '\n') || ch == 127) message[i] = ' ';
    }
    fputs("libalpm: ", stderr);
    fputs(message, stderr);
    fflush(stderr);
}
int zap_set_log_callback(alpm_handle_t *handle) {
    return alpm_option_set_logcb(handle, log_message, NULL);
}
