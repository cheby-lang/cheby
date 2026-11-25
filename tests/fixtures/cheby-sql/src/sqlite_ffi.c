/* SQLite for main.cheby, on native targets.

   A pool is a fixed set of open connections. A lease is one of them, taken
   from the pool until Cheby drops the last reference to it. Both are handles
   of external types, so a fiber that panics or is cancelled gives its lease
   back as it unwinds, and the drop function rolls back what it left open.

   A value of a cell or a parameter is an Option of a Result: None is NULL,
   Ok holds an integer and Err holds a text. */

#include <cheby.h>

#include <errno.h>
#include <pthread.h>
#include <sqlite3.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* How long a query waits for a lock, and a fiber for a connection. */
#define WAIT_SECONDS 5

typedef struct Pool {
    pthread_mutex_t lock;
    pthread_cond_t freed;
    sqlite3 **idle;
    size_t idle_count;
    size_t size;
    /* The pool handle and every lease. */
    size_t references;
    /* Why the pool did not open, or NULL. */
    char *problem;
} Pool;

typedef struct Lease {
    Pool *pool;
    sqlite3 *db;
} Lease;

static char *copy_text(const char *text) {
    size_t len = strlen(text) + 1;
    char *copy = malloc(len);
    if (copy != NULL) {
        memcpy(copy, text, len);
    }
    return copy;
}

static cheby_value *text_value(const char *text) {
    return cheby_ffi_string_new(text, strlen(text));
}

/* The bytes of a Cheby string, ended by a NUL. */
static char *c_string(cheby_value *text) {
    size_t len = 0;
    const char *bytes = cheby_ffi_string_bytes(text, &len);
    char *copy = malloc(len + 1);
    if (copy != NULL) {
        memcpy(copy, bytes, len);
        copy[len] = '\0';
    }
    return copy;
}

static void release_pool(Pool *pool) {
    pthread_mutex_lock(&pool->lock);
    pool->references -= 1;
    bool last = pool->references == 0;
    pthread_mutex_unlock(&pool->lock);
    if (!last) {
        return;
    }
    for (size_t i = 0; i < pool->idle_count; i++) {
        sqlite3_close_v2(pool->idle[i]);
    }
    pthread_cond_destroy(&pool->freed);
    pthread_mutex_destroy(&pool->lock);
    free(pool->idle);
    free(pool->problem);
    free(pool);
}

Pool *cheby_sql_pool_open(cheby_value *path, int64_t size) {
    Pool *pool = calloc(1, sizeof(Pool));
    if (pool == NULL) {
        return NULL;
    }
    pthread_mutex_init(&pool->lock, NULL);
    pthread_cond_init(&pool->freed, NULL);
    pool->references = 1;
    char *name = c_string(path);
    if (size < 1 || size > 1000 || name == NULL) {
        pool->problem = copy_text("a pool holds from 1 to 1000 connections");
        free(name);
        return pool;
    }
    pool->size = (size_t)size;
    pool->idle = calloc(pool->size, sizeof(sqlite3 *));
    int flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX;
    while (pool->idle != NULL && pool->idle_count < pool->size) {
        sqlite3 *db = NULL;
        int status = sqlite3_open_v2(name, &db, flags, NULL);
        if (status == SQLITE_OK) {
            sqlite3_busy_timeout(db, WAIT_SECONDS * 1000);
            status = sqlite3_exec(db, "PRAGMA foreign_keys = ON", NULL, NULL, NULL);
        }
        if (status != SQLITE_OK) {
            pool->problem = copy_text(db == NULL ? "out of memory" : sqlite3_errmsg(db));
            sqlite3_close_v2(db);
            break;
        }
        pool->idle[pool->idle_count] = db;
        pool->idle_count += 1;
    }
    if (pool->idle == NULL) {
        pool->problem = copy_text("out of memory");
    }
    free(name);
    return pool;
}

cheby_value *cheby_sql_pool_problem(Pool *pool) {
    if (pool == NULL) {
        return text_value("out of memory");
    }
    return text_value(pool->problem == NULL ? "" : pool->problem);
}

void cheby_sql_pool_close(Pool *pool) {
    release_pool(pool);
}

/* Waits for a connection. NULL when none is free in time. */
Lease *cheby_sql_lease(Pool *pool) {
    Lease *lease = calloc(1, sizeof(Lease));
    if (lease == NULL) {
        return NULL;
    }
    struct timespec until;
    clock_gettime(CLOCK_REALTIME, &until);
    until.tv_sec += WAIT_SECONDS;
    pthread_mutex_lock(&pool->lock);
    while (pool->idle_count == 0) {
        if (pthread_cond_timedwait(&pool->freed, &pool->lock, &until) == ETIMEDOUT) {
            break;
        }
    }
    if (pool->idle_count == 0) {
        pthread_mutex_unlock(&pool->lock);
        free(lease);
        return NULL;
    }
    pool->idle_count -= 1;
    lease->pool = pool;
    lease->db = pool->idle[pool->idle_count];
    pool->references += 1;
    pthread_mutex_unlock(&pool->lock);
    return lease;
}

bool cheby_sql_lease_is_open(Lease *lease) {
    return lease != NULL;
}

/* The drop function of a lease: ends a transaction that nothing committed,
   then gives the connection back. */
void cheby_sql_lease_close(Lease *lease) {
    Pool *pool = lease->pool;
    if (sqlite3_get_autocommit(lease->db) == 0) {
        sqlite3_exec(lease->db, "ROLLBACK", NULL, NULL, NULL);
    }
    pthread_mutex_lock(&pool->lock);
    pool->idle[pool->idle_count] = lease->db;
    pool->idle_count += 1;
    pthread_cond_signal(&pool->freed);
    pthread_mutex_unlock(&pool->lock);
    free(lease);
    release_pool(pool);
}

static cheby_value *failure(const char *message) {
    return cheby_ffi_err(text_value(message));
}

/* Binds $1, $2 and so on. Returns a problem, or NULL. */
static const char *bind_all(sqlite3_stmt *statement, cheby_value *params) {
    int64_t count = cheby_ffi_list_length(params);
    if (sqlite3_bind_parameter_count(statement) != count) {
        return "the statement and its parameters differ in number";
    }
    for (int64_t i = 0; i < count; i++) {
        char name[32];
        snprintf(name, sizeof name, "$%lld", (long long)i + 1);
        int index = sqlite3_bind_parameter_index(statement, name);
        if (index == 0) {
            return "the parameters of a statement are $1, $2 and so on";
        }
        cheby_value *cell = cheby_ffi_option_get(cheby_ffi_list_get(params, i));
        int status;
        if (cell == NULL) {
            status = sqlite3_bind_null(statement, index);
        } else if (cheby_ffi_result_is_ok(cell)) {
            status = sqlite3_bind_int64(statement, index, cheby_ffi_int_get(cheby_ffi_result_get(cell)));
        } else {
            size_t len = 0;
            const char *bytes = cheby_ffi_string_bytes(cheby_ffi_result_get(cell), &len);
            status = sqlite3_bind_text64(statement, index, bytes, len, SQLITE_TRANSIENT, SQLITE_UTF8);
        }
        if (status != SQLITE_OK) {
            return sqlite3_errstr(status);
        }
    }
    return NULL;
}

/* One column of the current row, or NULL for a type Cheby cannot read. */
static cheby_value *cell_value(sqlite3_stmt *statement, int column) {
    switch (sqlite3_column_type(statement, column)) {
    case SQLITE_NULL:
        return cheby_ffi_none();
    case SQLITE_INTEGER:
        return cheby_ffi_some(cheby_ffi_ok(cheby_ffi_int_new(sqlite3_column_int64(statement, column))));
    case SQLITE_TEXT: {
        const char *text = (const char *)sqlite3_column_text(statement, column);
        size_t len = (size_t)sqlite3_column_bytes(statement, column);
        return cheby_ffi_some(cheby_ffi_err(cheby_ffi_string_new(text, len)));
    }
    default:
        return NULL;
    }
}

typedef struct Rows {
    cheby_value **items;
    size_t count;
    size_t capacity;
} Rows;

static bool push_row(Rows *rows, cheby_value *row) {
    if (rows->count == rows->capacity) {
        size_t capacity = rows->capacity == 0 ? 8 : rows->capacity * 2;
        cheby_value **items = realloc(rows->items, capacity * sizeof(cheby_value *));
        if (items == NULL) {
            return false;
        }
        rows->items = items;
        rows->capacity = capacity;
    }
    rows->items[rows->count] = row;
    rows->count += 1;
    return true;
}

static void drop_rows(Rows *rows) {
    for (size_t i = 0; i < rows->count; i++) {
        cheby_ffi_drop(rows->items[i]);
    }
    free(rows->items);
}

/* The names of the columns, or the cells of the current row. */
static cheby_value *row_value(sqlite3_stmt *statement, int columns, bool names) {
    cheby_value **cells = calloc(columns == 0 ? 1 : (size_t)columns, sizeof(cheby_value *));
    if (cells == NULL) {
        return NULL;
    }
    for (int i = 0; i < columns; i++) {
        cells[i] = names ? cheby_ffi_some(cheby_ffi_err(text_value(sqlite3_column_name(statement, i))))
                         : cell_value(statement, i);
        if (cells[i] == NULL) {
            for (int j = 0; j < i; j++) {
                cheby_ffi_drop(cells[j]);
            }
            free(cells);
            return NULL;
        }
    }
    cheby_value *row = cheby_ffi_list_new(cells, (size_t)columns);
    free(cells);
    return row;
}

/* Runs one statement. Ok holds the names of the columns, then the rows. */
cheby_value *cheby_sql_query(Lease *lease, cheby_value *text, cheby_value *params) {
    size_t len = 0;
    const char *bytes = cheby_ffi_string_bytes(text, &len);
    sqlite3_stmt *statement = NULL;
    const char *rest = NULL;
    if (len > 1000000 || sqlite3_prepare_v2(lease->db, bytes, (int)len, &statement, &rest) != SQLITE_OK) {
        return failure(len > 1000000 ? "the statement is too long" : sqlite3_errmsg(lease->db));
    }
    if (statement == NULL) {
        return failure("the statement is empty");
    }
    for (; rest < bytes + len; rest++) {
        if (strchr(" \t\r\n;", *rest) == NULL) {
            sqlite3_finalize(statement);
            return failure("a query is one statement");
        }
    }
    const char *problem = bind_all(statement, params);
    Rows rows = {NULL, 0, 0};
    int columns = sqlite3_column_count(statement);
    int status = SQLITE_ROW;
    bool names = true;
    while (problem == NULL && status == SQLITE_ROW) {
        cheby_value *row = row_value(statement, columns, names);
        names = false;
        if (row == NULL || !push_row(&rows, row)) {
            cheby_ffi_drop(row);
            problem = "a column holds neither an integer nor a text";
            break;
        }
        status = sqlite3_step(statement);
        if (status != SQLITE_ROW && status != SQLITE_DONE) {
            problem = sqlite3_errmsg(lease->db);
        }
    }
    cheby_value *result;
    if (problem != NULL) {
        result = failure(problem);
        drop_rows(&rows);
    } else {
        result = cheby_ffi_ok(cheby_ffi_list_new(rows.items, rows.count));
        free(rows.items);
    }
    sqlite3_finalize(statement);
    return result;
}

/* How many rows the last statement of the connection changed. */
int64_t cheby_sql_changes(Lease *lease) {
    return sqlite3_changes(lease->db);
}

/* Runs every statement of a script. */
cheby_value *cheby_sql_script(Lease *lease, cheby_value *text) {
    char *statements = c_string(text);
    if (statements == NULL) {
        return failure("out of memory");
    }
    int status = sqlite3_exec(lease->db, statements, NULL, NULL, NULL);
    free(statements);
    if (status != SQLITE_OK) {
        return failure(sqlite3_errmsg(lease->db));
    }
    return cheby_ffi_ok(cheby_ffi_int_new(0));
}
