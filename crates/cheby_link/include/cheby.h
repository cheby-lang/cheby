#ifndef CHEBY_H
#define CHEBY_H

/* The Cheby runtime's functions for native code in packages (spec
   §12.5.1, D-187, D-945). Arguments are borrowed for the call and results
   are owned by the caller; a function that takes a payload takes its
   reference. A NULL argument means no value. A value of another type than
   a function takes is undefined behavior, which a debug runtime stops on.
   A function that cannot allocate returns NULL, and the foreign call then
   panics with `out of memory` when it returns (D-946). This header only
   grows. */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct cheby_value cheby_value;

/* Reference counts. */
cheby_value *cheby_ffi_dup(cheby_value *v);
void cheby_ffi_drop(cheby_value *v);

/* `String`: invalid UTF-8 is replaced with U+FFFD; the bytes are not
   NUL-terminated and stay valid while `s` is held. */
cheby_value *cheby_ffi_string_new(const char *bytes, size_t len);
const char *cheby_ffi_string_bytes(cheby_value *s, size_t *len);

/* `Bytes`. */
cheby_value *cheby_ffi_bytes_new(const uint8_t *data, size_t len);
const uint8_t *cheby_ffi_bytes_data(cheby_value *b, size_t *len);

/* Scalars in generic positions, such as the elements of a list. */
cheby_value *cheby_ffi_int_new(int64_t value);
int64_t cheby_ffi_int_get(cheby_value *v);
cheby_value *cheby_ffi_float_new(double value);
double cheby_ffi_float_get(cheby_value *v);
cheby_value *cheby_ffi_bool_new(bool value);
bool cheby_ffi_bool_get(cheby_value *v);
cheby_value *cheby_ffi_nil(void);

/* `Option`: the payload read out is borrowed, NULL for `None`. */
cheby_value *cheby_ffi_some(cheby_value *payload);
cheby_value *cheby_ffi_none(void);
cheby_value *cheby_ffi_option_get(cheby_value *option);

/* `Result`: the payload read out is borrowed. */
cheby_value *cheby_ffi_ok(cheby_value *payload);
cheby_value *cheby_ffi_err(cheby_value *payload);
bool cheby_ffi_result_is_ok(cheby_value *result);
cheby_value *cheby_ffi_result_get(cheby_value *result);

/* `List`: built from `n` owned items; an element read out is borrowed,
   NULL out of range. */
cheby_value *cheby_ffi_list_new(cheby_value **items, size_t n);
size_t cheby_ffi_list_length(cheby_value *list);
cheby_value *cheby_ffi_list_get(cheby_value *list, size_t index);

/* One write to standard output or standard error, in order with the
   program's other output: 0, or -1 when the write failed (D-951). */
int cheby_ffi_write_stdout(const char *bytes, size_t len);
int cheby_ffi_write_stderr(const char *bytes, size_t len);

#endif
