#include <cheby.h>

#include <stdlib.h>

/* A handle whose only state is that it exists. */
typedef struct Handle {
    int unused;
} Handle;

Handle *app_handle_open(void) { return calloc(1, sizeof(Handle)); }

/* The drop function of `Handle`. It writes its line to standard error,
   where the panic report goes, so the golden shows which comes first. */
void app_handle_close(Handle *handle) {
    static const char line[] = "handle released\n";
    cheby_ffi_write_stderr(line, sizeof line - 1);
    free(handle);
}
