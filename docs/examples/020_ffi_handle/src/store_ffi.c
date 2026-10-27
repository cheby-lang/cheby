/* Foreign helpers for store.cheby on native targets.

   A store is a C struct. To Cheby it is a value of the external type
   `Store`: the runtime passes its pointer back to these functions
   unchanged and never looks inside. The runtime counts references to it
   like any other handle, and calls `ffi_handle_store_close`, the type's
   drop function, exactly once, when the last one goes away.

   Cheby values cross as `cheby_value *`, through the functions of the
   runtime's `cheby.h`. Arguments are only borrowed for the duration of a
   call, and a returned value belongs to the caller. */

#include <cheby.h>

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct Store {
    /* The name, kept with `cheby_ffi_dup` for the closing message. */
    cheby_value *name;
    /* Copies of the entries' bytes, oldest first. */
    char **entries;
    size_t *lengths;
    size_t count;
    size_t capacity;
} Store;

Store *ffi_handle_store_open(cheby_value *name) {
    Store *store = calloc(1, sizeof(Store));
    if (store == NULL) {
        return NULL;
    }
    store->name = cheby_ffi_dup(name);
    return store;
}

void ffi_handle_store_write(Store *store, cheby_value *entry) {
    if (store == NULL) {
        return;
    }
    if (store->count == store->capacity) {
        size_t capacity = store->capacity == 0 ? 4 : store->capacity * 2;
        char **entries = realloc(store->entries, capacity * sizeof(char *));
        if (entries == NULL) {
            return;
        }
        store->entries = entries;
        size_t *lengths = realloc(store->lengths, capacity * sizeof(size_t));
        if (lengths == NULL) {
            return;
        }
        store->lengths = lengths;
        store->capacity = capacity;
    }
    size_t len = 0;
    const char *bytes = cheby_ffi_string_bytes(entry, &len);
    char *copy = malloc(len == 0 ? 1 : len);
    if (copy == NULL) {
        return;
    }
    memcpy(copy, bytes, len);
    store->entries[store->count] = copy;
    store->lengths[store->count] = len;
    store->count += 1;
}

cheby_value *ffi_handle_store_contents(Store *store) {
    static const char separator[] = "; ";
    size_t total = 0;
    size_t count = store == NULL ? 0 : store->count;
    for (size_t i = 0; i < count; i++) {
        total += store->lengths[i] + (i == 0 ? 0 : sizeof separator - 1);
    }
    char *joined = malloc(total == 0 ? 1 : total);
    if (joined == NULL) {
        return NULL;
    }
    size_t at = 0;
    for (size_t i = 0; i < count; i++) {
        if (i > 0) {
            memcpy(joined + at, separator, sizeof separator - 1);
            at += sizeof separator - 1;
        }
        memcpy(joined + at, store->entries[i], store->lengths[i]);
        at += store->lengths[i];
    }
    cheby_value *text = cheby_ffi_string_new(joined, total);
    free(joined);
    return text;
}

int64_t ffi_handle_store_count(Store *store) {
    return store == NULL ? 0 : (int64_t)store->count;
}

/* The drop function of `Store`. It writes its line with one
   `cheby_ffi_write_stdout`, so the line comes out in order with the
   program's own output, then releases the name and frees the struct. */
void ffi_handle_store_close(Store *store) {
    static const char prefix[] = "closing ";
    size_t name_len = 0;
    const char *name = cheby_ffi_string_bytes(store->name, &name_len);
    size_t len = sizeof prefix - 1 + name_len + 1;
    char *line = malloc(len);
    if (line != NULL) {
        memcpy(line, prefix, sizeof prefix - 1);
        memcpy(line + sizeof prefix - 1, name, name_len);
        line[len - 1] = '\n';
        cheby_ffi_write_stdout(line, len);
        free(line);
    }
    cheby_ffi_drop(store->name);
    for (size_t i = 0; i < store->count; i++) {
        free(store->entries[i]);
    }
    free(store->entries);
    free(store->lengths);
    free(store);
}
