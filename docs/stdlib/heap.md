# `std::heap`

A persistent priority queue that pops its smallest or its largest element first and keeps duplicates (D-1612). It is the queue of Dijkstra's algorithm and A*, where a [`std::sorted_set`](sorted_set.md) used as one would drop elements that compare `Equal` unless each carried a tie-breaker.

```cheby
pub type Heap<T>
```

`Heap` is opaque. Values are immutable, so every update returns a new heap, and the old one stays usable.

- **Direction.** A heap pops its smallest element first when made by `new_min` or `from_list`, and its largest when made by `new_max` or `from_list_max`, so the call that makes a heap says which one it is (D-1612). `to_list` lists the elements in the order repeated pops would return them.
- **Elements.** The functions that compare elements carry the bound `T: Compare`: `from_list`, `from_list_max`, `push`, `pop` and `to_list`. `new_min`, `new_max`, `peek`, `size`, `is_empty` and the hooks of equality, hashing and debug printing carry none and never call `compare` (D-1612, D-482). Elements that compare `Equal` are all kept, and which of them pops first is fixed by the algorithm below.
- **The algorithm.** A heap is a binary heap in an array: element `i` has the children `2i + 1` and `2i + 2`, and its parent is `(i - 1) / 2` (D-1613). Every target runs this algorithm, so a `compare` that is not a consistent order gives the same heap everywhere and sees the same calls in the same order, as [`list::sort_with`](list.md#sort_with) does (D-1448). With `before(x, y)` meaning that `compare(x, y)` is `Less` for a heap that pops its smallest element first and `Greater` for one that pops its largest:
  - `push` puts `x` at the end and, while it is not at the root and `before(x, parent)` holds, swaps it with its parent.
  - `pop` takes the root, moves the last element to the root and sifts it down: at each position it chooses the right child over the left only when `before(right, left)` holds, and it swaps the element with the chosen child while `before(child, element)` holds.
  - `from_list` keeps the order of its list and sifts down each position from `n / 2 - 1` to `0`.
- **Runtime kernels.** The array is a list's tree (ADR-0018), and each update is a runtime kernel that changes a heap in place when nothing else holds it (D-525), so a heap used once per step costs no copy. The updates run in the runtime one comparison at a time: the runtime names the next pair and the caller's code calls `compare`, as for `list::sort_with` (D-1448, D-1787), except when the element type's `compare` is built in: `Int`, the sized integer types, `String`, and tuples whose elements are such types, recursively. For those the kernels compare natively, which gives the same `Order` and so the same heap. They find the element type from the type descriptor that the bound's function package carries (§8.6.1), never from the shape of a value, so a type with its own `compare` is always called back, even when its values look like `Int`s (D-1613). A panic inside `compare` unwinds like any other panic, and the heap it was updating is released once (D-697, D-1787).
- **Equality, hashing and printing.** Heaps that hold the same elements can store them in different orders, so the module supplies its own `==`, hashing and debug printing through the standard-library-only attribute of D-342 (§3.13). Two heaps are `==` when they pop in the same direction and hold the same elements as a multiset, compared with `==`, and the hash of a heap does not depend on the order its elements are stored in. Debug printing writes `heap::from_list([…])` or `heap::from_list_max([…])` with each element debug-printed and the elements sorted by that text, as a map's entries are (D-1612, D-344).
- **Interfaces.** `Heap<T>` satisfies neither `Show` nor `Compare`.
- **Costs.** For a heap of n elements, `push` and `pop` take O(log n) time and O(log n) calls to `compare`, `from_list` and `from_list_max` take O(n) time and calls, `to_list` takes O(n log n), and `peek`, `size` and `is_empty` take O(1).

```cheby
test "heaps compare by direction and contents" {
  let built = heap::from_list([3, 1, 2])
  let pushed = heap::push(heap::push(heap::push(heap::new_min(), 3), 2), 1)
  assert built == pushed
  assert built != heap::from_list_max([3, 1, 2])
  assert built != heap::from_list([1, 2])
  assert heap::from_list([1, 1, 2]) != heap::from_list([1, 2, 2])
  assert heap::new_min::<Int>() == heap::from_list([])
  assert heap::new_min::<Int>() != heap::new_max::<Int>()
}
```

```cheby
test "equal heaps hash alike" {
  let built = heap::from_list([1, 2, 3, 2])
  let pushed = heap::push(heap::push(heap::push(heap::push(heap::new_min(), 3), 2), 2), 1)
  assert built == pushed
  assert map::size(map::from_list([(built, 1), (pushed, 2)])) == 1
  assert map::size(map::from_list([(built, 1), (heap::from_list_max([1, 2, 3, 2]), 2)])) == 2
}
```

```cheby
test "debug text sorts the elements by their text" {
  let numbers = heap::from_list([10, 9, 1])
  let words = heap::from_list_max(["b", "a", "b"])
  let empty = heap::new_min::<Int>()
  assert "{numbers:?}" == "heap::from_list([1, 10, 9])"
  assert "{words:?}" == "heap::from_list_max([\"a\", \"b\", \"b\"])"
  assert "{empty:?}" == "heap::from_list([])"
}
```

## Building

### `new_min`

```cheby
pub fn new_min<T>() -> Heap<T>
```

Returns an empty heap that pops its smallest element first.

- **Cost:** O(1), required (D-300).

```cheby
test "new_min is empty" {
  let empty = heap::new_min::<Int>()
  assert heap::is_empty(empty)
  assert heap::pop(empty) == None
  assert heap::to_list(empty) == []
}
```

### `new_max`

```cheby
pub fn new_max<T>() -> Heap<T>
```

Returns an empty heap that pops its largest element first.

- **Cost:** O(1), required (D-300).

```cheby
test "new_max pops the largest element first" {
  let h = heap::push(heap::push(heap::push(heap::new_max(), 2), 7), 4)
  assert heap::to_list(h) == [7, 4, 2]
}
```

### `from_list`

```cheby
pub fn from_list<T: Compare>(items: List<T>) -> Heap<T>
```

Returns a heap of `items` that pops its smallest element first. It keeps the order of `items` and sifts down each position from `n / 2 - 1` to `0` (D-1613).

- **Cost:** O(n) time and calls to `compare`, where n is the length of `items`. Required (D-300).

```cheby
test "from_list pops the smallest element first and keeps duplicates" {
  let h = heap::from_list([5, 3, 8, 3, 1])
  assert heap::size(h) == 5
  assert heap::to_list(h) == [1, 3, 3, 5, 8]
}
```

### `from_list_max`

```cheby
pub fn from_list_max<T: Compare>(items: List<T>) -> Heap<T>
```

Returns a heap of `items` that pops its largest element first, built as `from_list` builds one.

- **Cost:** O(n), as for `from_list`. Required (D-300).

```cheby
test "from_list_max pops the largest element first" {
  assert heap::to_list(heap::from_list_max([5, 3, 8, 3, 1])) == [8, 5, 3, 3, 1]
}
```

## Updating

### `push`

```cheby
pub fn push<T: Compare>(h: Heap<T>, item: T) -> Heap<T>
```

Returns `h` with `item` added, kept beside any element equal to it. It goes at the end and moves up past each parent that it goes before (D-1613).

- **Cost:** O(log n) time and calls to `compare`; in place, with amortized O(1) allocation, when nothing else holds `h` (D-525). Required (D-300).

```cheby
test "push keeps duplicates" {
  let h = heap::push(heap::push(heap::push(heap::new_min(), "b"), "a"), "b")
  assert heap::to_list(h) == ["a", "b", "b"]
  assert heap::size(h) == 3
}
```

### `pop`

```cheby
pub fn pop<T: Compare>(h: Heap<T>) -> Option<(T, Heap<T>)>
```

Returns the first element, the smallest or the largest, and the heap without it, or `None` when `h` is empty. The last element moves to the root and down past each child that goes before it (D-1613).

- **Cost:** O(log n) time and calls to `compare`; in place when nothing else holds `h` (D-525). Required (D-300).

```cheby
test "pop returns the first element and the rest" {
  let assert Some((first, rest)) = heap::pop(heap::from_list([4, 1, 3]))
  assert first == 1
  assert heap::to_list(rest) == [3, 4]
  let assert Some((largest, _)) = heap::pop(heap::from_list_max([4, 1, 3]))
  assert largest == 4
  assert heap::pop(heap::new_max::<Int>()) == None
}
```

```cheby
test "updates leave the heap they started from unchanged" {
  let h = heap::from_list(list::map(list::range(0, 100), fn(n) { n * 37 % 100 }))
  let more = heap::push(h, -1)
  let assert Some((first, rest)) = heap::pop(h)
  let assert Some((again, _)) = heap::pop(h)
  assert first == 0
  assert again == 0
  assert heap::to_list(h) == list::range(0, 100)
  assert heap::peek(more) == Some(-1)
  assert heap::size(rest) == 99
  assert heap::size(h) == 100
}
```

## Reading

### `peek`

```cheby
pub fn peek<T>(h: Heap<T>) -> Option<T>
```

Returns the element `pop` would return, without removing it, or `None` when `h` is empty.

- **Cost:** O(1), required (D-300).

```cheby
test "peek reads the first element" {
  assert heap::peek(heap::from_list([4, 1, 3])) == Some(1)
  assert heap::peek(heap::from_list_max([4, 1, 3])) == Some(4)
  assert heap::peek(heap::new_min::<Int>()) == None
}
```

### `size`

```cheby
pub fn size<T>(h: Heap<T>) -> Int
```

Returns the number of elements, duplicates included.

- **Cost:** O(1), required (D-300).

```cheby
test "size counts duplicates" {
  assert heap::size(heap::from_list([2, 2, 2])) == 3
  assert heap::size(heap::new_max::<Int>()) == 0
}
```

### `is_empty`

```cheby
pub fn is_empty<T>(h: Heap<T>) -> Bool
```

Tells whether the heap has no elements.

- **Cost:** O(1), required (D-300).

```cheby
test "is_empty tells an empty heap" {
  assert heap::is_empty(heap::new_min::<Int>())
  assert !heap::is_empty(heap::from_list([1]))
}
```

### `to_list`

```cheby
pub fn to_list<T: Compare>(h: Heap<T>) -> List<T>
```

Returns the elements in the order repeated pops would return them. The heap does not change (D-1612).

- **Cost:** O(n log n) time and calls to `compare`. Required (D-300).

```cheby
test "to_list lists the elements in pop order" {
  let h = heap::from_list([(2, "b"), (1, "z"), (2, "a")])
  assert heap::to_list(h) == [(1, "z"), (2, "a"), (2, "b")]
  assert heap::to_list(h) == heap::to_list(h)
  assert heap::size(h) == 3
}
```

## Order of equal elements

Elements that compare `Equal` pop in the order the algorithm leaves them in, which depends on how the heap was built. This example orders jobs by rank alone, from the highest rank down, so two jobs of one rank compare `Equal`:

```cheby
type Job {
  rank: Int,
  name: String,
}

fn compare(a: Job, b: Job) -> Order {
  int::compare(b.rank, a.rank)
}
```

```cheby
test "a type's own compare is called, and ties pop in a fixed order" {
  let jobs = [
    Job { rank: 1, name: "a" },
    Job { rank: 1, name: "b" },
    Job { rank: 2, name: "c" },
    Job { rank: 2, name: "d" },
    Job { rank: 2, name: "e" },
    Job { rank: 2, name: "f" },
  ]
  assert names(heap::from_list(jobs)) == "dfecba"
  assert names(list::fold(jobs, heap::new_min(), heap::push)) == "cdefba"
  assert names(heap::from_list_max(jobs)) == "abedcf"
  assert compare(Job { rank: 1, name: "x" }, Job { rank: 2, name: "y" }) == Greater
}
```

`names` joins the names of the jobs in pop order.
