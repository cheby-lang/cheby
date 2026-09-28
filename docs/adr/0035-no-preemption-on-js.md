---
status: accepted
date: 2026-09-28
log: D-185
---

# No preemption on the JS target

Native fibers are preempted through a yield check at every function entry (D-028), but the JS backend inserts none. On JS only functions that can reach a suspension point become generators (D-014), and an entry yield check would be a suspension point in nearly every function, turning almost all code into slow generators. So a CPU-bound fiber on JS runs until its next real suspension point and can starve other fibers. This is documented as a semantic difference between targets, like earlier `Int` overflow (D-037) and shallower recursion (D-094).
