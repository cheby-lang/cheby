# `std::filepath`

Lexical file paths: joining, taking apart and comparing paths as text (D-1432). No function touches the file system or reads the host. To read the working directory, see [`env::current_dir`](env.md#current_dir).

- **Paths are `String`s.** There is no `Path` type (D-1077). Names that are not UTF-8 never reach Cheby, because the functions of [`std::file`](file.md) fail on them (D-311, D-997).
- **Separators.** `/` and `\` both separate components, on every platform and target, so `C:\Temp\x` from [`file::with_temp_dir`](file.md#with_temp_dir) on Windows reads the same on every host (D-1433, [ADR-0079](../adr/0079-path-separators-on-every-platform.md)).
- **Roots.** A path's root is read from its start. It is a plain root, `/`; a drive root, an ASCII letter, `:` and a separator, as in `C:/`; or a UNC root, exactly two separators, a server name, a separator and a share name, as in `//server/share`. A letter and `:` with no separator after them, as in `C:x`, are a drive prefix: the path is relative to that drive's current directory, and is not absolute. Verbatim paths such as `\\?\C:\x` get no reading of their own, and letter case is never compared loosely (D-1433).
- **Normalized results.** Every function that returns a path returns it as [`normalize`](#normalize) writes it, with `/` between components, except [`child`](#child) (D-1434, D-1436).
- **Text only.** The functions read paths as text. `normalize("a/link/..")` is `"a"` even when `link` is a symbolic link that the operating system would follow somewhere else, and [`join_within`](#join_within) checks only the text (D-1434, D-1435).
- **Names that hold `\`.** On Unix, `\` may be part of a file name, and every function here except `child` reads it as a separator. A program that walks a directory builds each path from a name of [`file::list_dir`](file.md#list_dir) with `child`, at every level, so that `a\b` stays one name (D-1436).
- **Written in Cheby.** Every function is Cheby over `std::list` and `std::string`, with no kernels, so every function runs in constants and gives the same text on every target, JS included (D-232, D-991).
- **The name `filepath`.** The module is named as Go's is, so that a module that imports it may still call its values `path` (D-062, D-1432).
- **Tier.** `std::filepath` ships before M4's JS work, so it is a tier-4 module, although it needs no host (D-1054).

## Joining

### `join`

```cheby
pub fn join(base: String, relative: String) -> String
```

Returns `base` followed by the components of `relative`, normalized. `relative`'s plain or drive root is dropped, and the `X:` of a drive and the server and share of a UNC root become components, so the result stays under `base` (D-1077). An empty `base` is `.`. A `..` in `relative` may still climb out of `base`. [`join_within`](#join_within) refuses that (D-1435).

- **Cost:** O(n) in the total length of `base` and `relative`. Informative.

```cheby
test "join keeps an absolute second argument under its base" {
  assert filepath::join("a", "/b") == "a/b"
  assert filepath::join("a", "C:/b") == "a/C:/b"
  assert filepath::join("a", "//srv/share/x") == "a/srv/share/x"
  assert filepath::join("", "/b") == "b"
  assert filepath::join("", "") == "."
}

test "join normalizes its result" {
  assert filepath::join("a/b", "../c") == "a/c"
  assert filepath::join("uploads", "../../etc") == "../etc"
  assert filepath::join("C:\\Temp", "x") == "C:/Temp/x"
  assert filepath::join("/", "..") == "/"
}
```

### `join_all`

```cheby
pub fn join_all(parts: List<String>) -> String
```

Joins `parts` from left to right with [`join`](#join), starting from the first part normalized, so the first part keeps its root. No parts give `"."` (D-1435).

- **Cost:** O(n · k) in the total length of the parts and their number. Informative.

```cheby
test "join_all joins from left to right" {
  assert filepath::join_all([]) == "."
  assert filepath::join_all(["/usr", "local", "bin"]) == "/usr/local/bin"
  assert filepath::join_all(["a", "/b", "c"]) == "a/b/c"
}
```

### `join_within`

```cheby
pub fn join_within(base: String, relative: String) -> Option<String>
```

Returns `Some` of `join(base, relative)`, or `None` when a `..` of `relative`, read after its root is dropped and its `.` components are skipped, would climb above `base`. Use it to join a path that comes from outside the program. The check reads only the text, so a symbolic link inside `base` can still lead out of it (D-1435).

- **Cost:** O(n) in the total length of `base` and `relative`. Informative.

```cheby
test "join_within refuses to climb above its base" {
  assert filepath::join_within("up", "x/../y") == Some("up/y")
  assert filepath::join_within("up", "../../etc") == None
  assert filepath::join_within("up", "x/../..") == None
  assert filepath::join_within("up", "/etc") == Some("up/etc")
  assert filepath::join_within("../up", "x") == Some("../up/x")
}
```

### `child`

```cheby
pub fn child(dir: String, name: String) -> Option<String>
```

Appends the directory entry `name` to `dir`, with neither normalized: `name` alone when `dir` is `""`, `dir + name` when `dir` ends with a separator, and `dir + "/" + name` otherwise. Returns `None` when `name` is `""`, `.` or `..`, or contains `/`. A `\` in `name` is kept, because on Unix it is part of the name and on Windows a listed name never holds one. Build the paths of a directory walk with `child`, from names that `file::list_dir` gives (D-1436).

- **Cost:** O(n) in the total length of `dir` and `name`. Informative.

```cheby
test "child keeps a name as written" {
  assert filepath::child("dir", "a\\b") == Some("dir/a\\b")
  assert filepath::child("C:\\Temp", "x") == Some("C:\\Temp/x")
  assert filepath::child("/", "etc") == Some("/etc")
  assert filepath::child("", "a") == Some("a")
}

test "child refuses what is not one name" {
  assert filepath::child("dir", "") == None
  assert filepath::child("dir", ".") == None
  assert filepath::child("dir", "..") == None
  assert filepath::child("dir", "x/y") == None
}
```

## Normalizing

### `normalize`

```cheby
pub fn normalize(path: String) -> String
```

Returns `path` cleaned as Go's `Clean` does. It reads the root, splits the rest at separators, drops empty components and `.`, and resolves `..` from left to right. A `..` removes the name before it, is dropped directly after a plain, drive or UNC root, and is kept at the start of a path with no root or with a drive prefix. The root is written `/`, `X:/` with the letter as written, `//server/share` or the prefix `X:`, and the components follow, joined by `/`, with no trailing separator. When no component is left, the result is the root alone, `X:.` after a drive prefix, and `"."` otherwise. A result with no root whose first component starts with an ASCII letter and `:` gets `./` in front, so that it is never read back as a drive (D-1434).

- **Cost:** O(n). Informative.

```cheby
test "normalize cleans a path as Go does" {
  assert filepath::normalize("a//./b/../c/") == "a/c"
  assert filepath::normalize("../a/..") == ".."
  assert filepath::normalize("/..") == "/"
  assert filepath::normalize("") == "."
  assert filepath::normalize("C:\\a\\..\\b") == "C:/b"
}

test "normalize reads drives and shares" {
  assert filepath::normalize("\\\\srv\\share\\x\\") == "//srv/share/x"
  assert filepath::normalize("//a") == "/a"
  assert filepath::normalize("C:") == "C:."
  assert filepath::normalize("C:x/..") == "C:."
  assert filepath::normalize("a/../C:x") == "./C:x"
}
```

## Taking a path apart

Each of these functions reads the normalized path (D-1437).

### `components`

```cheby
pub fn components(path: String) -> List<String>
```

Returns the root of `path` as `normalize` writes it, if it has one, followed by its names. A path with no root whose first name could be read as a drive starts with `"."`. So `join_all(components(path)) == normalize(path)` (D-1437).

- **Cost:** O(n). Informative.

```cheby
test "components lists the root and the names" {
  assert filepath::components("/a/b") == ["/", "a", "b"]
  assert filepath::components("a\\b") == ["a", "b"]
  assert filepath::components("C:x") == ["C:", "x"]
  assert filepath::components("//srv/share/x") == ["//srv/share", "x"]
  assert filepath::components(".") == []
}

test "components joins back into the normalized path" {
  assert filepath::join_all(filepath::components("C:x")) == filepath::normalize("C:x")
  assert filepath::join_all(filepath::components("/a/./b/")) == filepath::normalize("/a/./b/")
  assert filepath::join_all(filepath::components("a/../C:x")) == filepath::normalize("a/../C:x")
}
```

### `file_name`

```cheby
pub fn file_name(path: String) -> Option<String>
```

Returns the last name of `path`, or `None` when it has no name or the last one is `..` (D-1437).

- **Cost:** O(n). Informative.

```cheby
test "file_name reads the last name" {
  assert filepath::file_name("a/b") == Some("b")
  assert filepath::file_name("a/b/") == Some("b")
  assert filepath::file_name("/") == None
  assert filepath::file_name("a/..") == None
  assert filepath::file_name("..") == None
}
```

### `parent`

```cheby
pub fn parent(path: String) -> Option<String>
```

Returns `path` without its last name, or `None` when it has no name or the last one is `..`. When no name is left, the result is the root, `X:.` after a drive prefix, and `"."` otherwise, so the result is always a path the functions of `std::file` accept (D-1437).

- **Cost:** O(n). Informative.

```cheby
test "parent removes the last name" {
  assert filepath::parent("a/b") == Some("a")
  assert filepath::parent("a") == Some(".")
  assert filepath::parent("/a") == Some("/")
  assert filepath::parent("C:\\Temp\\x") == Some("C:/Temp")
  assert filepath::parent("C:x") == Some("C:.")
}

test "parent has no answer at a root or above" {
  assert filepath::parent("/") == None
  assert filepath::parent("") == None
  assert filepath::parent("../..") == None
}
```

### `split`

```cheby
pub fn split(path: String) -> Option<(String, String)>
```

Returns `Some((parent, name))`, as [`parent`](#parent) and [`file_name`](#file_name) give them, exactly when `path` has a file name (D-1437).

- **Cost:** O(n). Informative.

```cheby
test "split gives the parent and the name" {
  assert filepath::split("a/b") == Some(("a", "b"))
  assert filepath::split("b") == Some((".", "b"))
  assert filepath::split("/") == None
}
```

### `ancestors`

```cheby
pub fn ancestors(path: String) -> List<String>
```

Returns the normalized `path`, followed by each of its parents, until [`parent`](#parent) gives `None` (D-1437).

- **Cost:** O(n · d) in the length and the number of names. Informative.

```cheby
test "ancestors starts with the path itself" {
  assert filepath::ancestors("a/b/c") == ["a/b/c", "a/b", "a", "."]
  assert filepath::ancestors("/x") == ["/x", "/"]
  assert filepath::ancestors("") == ["."]
  assert filepath::ancestors("../a") == ["../a", ".."]
}
```

## Extensions

A file name's stem and extension come from its last `.`, by Rust's rules. When the name is `..`, has no `.`, or has nothing before its last `.`, as `.bashrc` has, it has no extension and its stem is the whole name. Otherwise the stem is the text before that `.` and the extension the text after it, which may be empty (D-1438).

### `stem`

```cheby
pub fn stem(path: String) -> Option<String>
```

Returns the stem of the file name of `path`, or `None` when it has no file name (D-1438).

- **Cost:** O(n). Informative.

```cheby
test "stem drops the last extension" {
  assert filepath::stem("a.tar.gz") == Some("a.tar")
  assert filepath::stem(".bashrc") == Some(".bashrc")
  assert filepath::stem("dir/notes") == Some("notes")
  assert filepath::stem("/") == None
}
```

### `extension`

```cheby
pub fn extension(path: String) -> Option<String>
```

Returns the extension of the file name of `path`, without its dot, or `None` when it has no file name or no extension (D-1438).

- **Cost:** O(n). Informative.

```cheby
test "extension reads the text after the last dot" {
  assert filepath::extension("a.tar.gz") == Some("gz")
  assert filepath::extension("a") == None
  assert filepath::extension("a.") == Some("")
  assert filepath::extension(".bashrc") == None
  assert filepath::extension("v1.2/notes") == None
}
```

### `extensions`

```cheby
pub fn extensions(path: String) -> List<String>
```

Returns the extension of the file name of `path`, then the extension of its stem, and so on, front to back. The last element is [`extension`](#extension)'s, and a name with no extension gives `[]` (D-1438).

- **Cost:** O(n). Informative.

```cheby
test "extensions lists every extension" {
  assert filepath::extensions("a.tar.gz") == ["tar", "gz"]
  assert filepath::extensions("v1.2.3") == ["2", "3"]
  assert filepath::extensions(".config.json") == ["json"]
  assert filepath::extensions(".bashrc") == []
}
```

### `with_extension`

```cheby
pub fn with_extension(path: String, new_extension: String) -> String
```

Returns `path` with the extension of its file name replaced by `new_extension`, or added when it has none. One leading `.` of `new_extension` is dropped first, so `"md"` and `".md"` give the same result, and when that leaves `""` the extension is removed. A path with no file name comes back normalized. The result is normalized, so a separator in `new_extension` adds components (D-1438).

- **Cost:** O(n) in the total length of `path` and `new_extension`. Informative.

```cheby
test "with_extension replaces the last extension" {
  assert filepath::with_extension("a.txt", "md") == "a.md"
  assert filepath::with_extension("a.txt", ".md") == "a.md"
  assert filepath::with_extension("a.tar.gz", "zip") == "a.tar.zip"
  assert filepath::with_extension("dir/a", "md") == "dir/a.md"
  assert filepath::with_extension(".bashrc", "bak") == ".bashrc.bak"
}

test "with_extension removes the extension with empty text" {
  assert filepath::with_extension("a.txt", "") == "a"
  assert filepath::with_extension("/", "md") == "/"
}
```

### `with_file_name`

```cheby
pub fn with_file_name(path: String, name: String) -> String
```

Returns `path` with its file name replaced by `name`, as `join` of its parent and `name` writes it. When `path` has no file name, `name` is appended (D-1438).

- **Cost:** O(n) in the total length of `path` and `name`. Informative.

```cheby
test "with_file_name replaces the last name" {
  assert filepath::with_file_name("a/b", "c") == "a/c"
  assert filepath::with_file_name("b", "c") == "c"
  assert filepath::with_file_name("/", "c") == "/c"
  assert filepath::with_file_name("..", "c") == "../c"
}
```

## Comparing and relating

### `is_absolute`

```cheby
pub fn is_absolute(path: String) -> Bool
```

Tells whether `path` has a plain, drive or UNC root. A drive prefix such as `C:x` is relative (D-1439).

- **Cost:** O(n). Informative.

```cheby
test "is_absolute reads every kind of root" {
  assert filepath::is_absolute("/x")
  assert filepath::is_absolute("C:/x")
  assert filepath::is_absolute("C:\\x")
  assert filepath::is_absolute("//srv/share/x")
  assert !filepath::is_absolute("C:x")
  assert !filepath::is_absolute("x")
}
```

### `starts_with`

```cheby
pub fn starts_with(path: String, prefix: String) -> Bool
```

Tells whether the [`components`](#components) of `prefix` begin the components of `path`. Whole components are compared, so `"/ab"` does not start with `"/a"`, and the empty path begins every path (D-1439).

- **Cost:** O(n) in the total length of `path` and `prefix`. Informative.

```cheby
test "starts_with compares whole components" {
  assert filepath::starts_with("/a/b", "/a")
  assert !filepath::starts_with("/ab", "/a")
  assert !filepath::starts_with("a/b", "/a")
  assert filepath::starts_with("a", "")
}
```

### `ends_with`

```cheby
pub fn ends_with(path: String, suffix: String) -> Bool
```

Tells whether the components of `suffix` end the components of `path`. A suffix with a root matches only a whole path, and the empty path ends every path (D-1439).

- **Cost:** O(n) in the total length of `path` and `suffix`. Informative.

```cheby
test "ends_with compares whole components" {
  assert filepath::ends_with("a/b/c", "b/c")
  assert !filepath::ends_with("a/bc", "c")
  assert filepath::ends_with("/a/b", "/a/b")
  assert !filepath::ends_with("x/a/b", "/a/b")
  assert filepath::ends_with("a", "")
}
```

### `relative_to`

```cheby
pub fn relative_to(path: String, base: String) -> Option<String>
```

Returns a path that leads from `base` to `path`, as Go's `Rel` does. Both are normalized, and their common leading names are removed. The result is one `..` for each name left in `base`, followed by the names left in `path`, and `"."` when nothing is left. Returns `None` when their roots differ, which includes one having a root and the other none, and when what is left of `base` holds a `..`, which cannot be climbed back without the file system (D-1439).

- **Cost:** O(n) in the total length of `path` and `base`. Informative.

```cheby
test "relative_to climbs out of the base" {
  assert filepath::relative_to("/a/b/c", "/a") == Some("b/c")
  assert filepath::relative_to("/a/x", "/a/b") == Some("../x")
  assert filepath::relative_to("/a", "/a") == Some(".")
  assert filepath::relative_to("../a", "../b") == Some("../a")
}

test "relative_to needs one root and a known base" {
  assert filepath::relative_to("a", "/a") == None
  assert filepath::relative_to("C:/x", "D:/x") == None
  assert filepath::relative_to("x", "../y") == None
}
```

### `absolute`

```cheby
pub fn absolute(path: String, cwd: String) -> String
```

Returns `path` normalized when it is absolute, and `join(cwd, path)` otherwise. A path with a drive prefix is joined like any relative path. The result is absolute when `cwd` is, as [`env::current_dir`](env.md#current_dir)'s result is (D-1439):

```cheby
fn absolute_argument(argument: String) -> Result<String, io::Error> {
  use cwd <- result::try(env::current_dir())
  Ok(filepath::absolute(argument, cwd))
}
```

- **Cost:** O(n) in the total length of `path` and `cwd`. Informative.

```cheby
test "absolute keeps an absolute path and joins a relative one" {
  assert filepath::absolute("/etc", "/home") == "/etc"
  assert filepath::absolute("x", "/home") == "/home/x"
  assert filepath::absolute("../x", "C:\\Users\\me") == "C:/Users/x"
}
```
