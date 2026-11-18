# Task generation report

Generated 2026-11-17T05:26:31Z by `python -m sandfork_tasks.generate`.

| Family | Produced | Kept | Quarantined | Validated |
|---|---|---|---|---|
| math-tool | 200 | 200 | 0 | 200 |
| data-analysis | 200 | 200 | 0 | 200 |
| shell-ops | 200 | 200 | 0 | 200 |
| py-bugfix | 640 | 290 | 0 | 290 |
| py-heavy | 288 | 126 | 0 | 126 |

For the bug-fix families, *produced* counts every single-site mutant and *kept* those that passed the keep-rules. Discards by reason:

* **py-bugfix / intervals**: 266 mutants, 124 kept, 84 hidden tests. Discarded: collection error or timeout: 2, duplicate: 2, equivalent: no test fails: 23, unfocused: more than 5 tests fail: 115. Kept by operator: change_constant: 2, delete_statement: 13, drop_branch: 5, flip_compare: 8, invert_bool: 4, off_by_one: 7, swap_args: 4, wrong_variable: 81.
* **py-bugfix / lrucache**: 199 mutants, 80 kept, 46 hidden tests. Discarded: collection error or timeout: 7, duplicate: 4, equivalent: no test fails: 31, unfocused: more than 5 tests fail: 77. Kept by operator: change_constant: 5, delete_statement: 19, drop_branch: 7, flip_compare: 6, invert_bool: 3, swap_args: 1, wrong_variable: 39.
* **py-bugfix / ratelimit**: 175 mutants, 86 kept, 40 hidden tests. Discarded: collection error or timeout: 1, duplicate: 3, equivalent: no test fails: 34, unfocused: more than 5 tests fail: 51. Kept by operator: change_constant: 6, delete_statement: 17, drop_branch: 9, flip_compare: 11, invert_bool: 6, off_by_one: 2, swap_args: 1, wrong_variable: 34.
* **py-heavy / ledger**: 288 mutants, 126 kept, 45 hidden tests. Discarded: collection error or timeout: 23, duplicate: 2, equivalent: no test fails: 75, unfocused: more than 5 tests fail: 62. Kept by operator: change_constant: 14, delete_statement: 14, drop_branch: 6, flip_compare: 4, invert_bool: 2, off_by_one: 6, swap_args: 5, wrong_variable: 75.
