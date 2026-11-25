# cheby-sql

A small SQL package over the system's SQLite. It is the git dependency of `docs/examples/030_shop_service` (D-1889): the harnesses make it a git repository with a fixed commit, and map `https://github.com/example/cheby-sql` to it.

Building it needs SQLite's header and library, such as `libsqlite3-dev` on Debian and Ubuntu. An edit changes the commit, so it needs a new `cheby.lock` in the example.
