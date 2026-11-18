## e1

| Family | Mode | G | p50 ms | p99 ms |
|---|---|---|---|---|
| data-analysis | F1 | 1 | 2956 | 3323 |
| data-analysis | F1 | 4 | 5555 | 12005 |
| data-analysis | F1 | 8 | 12517 | 13102 |
| data-analysis | F1 | 16 | 26420 | 30071 |
| data-analysis | F2 | 1 | 3144 | 3253 |
| data-analysis | F2 | 4 | 7106 | 9890 |
| data-analysis | F2 | 8 | 7625 | 9108 |
| data-analysis | F2 | 16 | 12904 | 13725 |
| data-analysis | F3 | 1 | 464 | 966 |
| data-analysis | F3 | 4 | 1480 | 3175 |
| data-analysis | F3 | 8 | 2166 | 2186 |
| data-analysis | F3 | 16 | 4192 | 4661 |
| py-bugfix | F1 | 1 | 3289 | 4309 |
| py-bugfix | F1 | 4 | 6186 | 9969 |
| py-bugfix | F1 | 8 | 14279 | 15326 |
| py-bugfix | F1 | 16 | 15848 | 21244 |
| py-bugfix | F2 | 1 | 295 | 501 |
| py-bugfix | F2 | 4 | 568 | 1389 |
| py-bugfix | F2 | 8 | 1472 | 2479 |
| py-bugfix | F2 | 16 | 2208 | 2530 |
| py-heavy | F1 | 1 | 9126 | 9463 |
| py-heavy | F1 | 4 | 11931 | 13307 |
| py-heavy | F1 | 8 | 16498 | 17851 |
| py-heavy | F1 | 16 | 29723 | 32148 |
| py-heavy | F2 | 1 | 289 | 305 |
| py-heavy | F2 | 4 | 744 | 1270 |
| py-heavy | F2 | 8 | 1698 | 1799 |
| py-heavy | F2 | 16 | 4067 | 4424 |

## e2

| Mode | G | PSS after fork (MB) | after query | after copying the data |
|---|---|---|---|---|
| F2 | 1 | 63 | 64 | 68 |
| F2 | 4 | 232 | 234 | 251 |
| F2 | 8 | 451 | 453 | 488 |
| F3 | 1 | 73 | 78 | 84 |
| F3 | 4 | 79 | 98 | 119 |
| F3 | 8 | 85 | 123 | 165 |

## e3

| Pool | Rollouts/s | Latency p50 s | p99 s | Errors |
|---|---|---|---|---|
| 1 | 0.45 | 27.90 | 51.53 | 0 |
| 2 | 0.83 | 24.47 | 46.10 | 0 |
| 4 | 1.02 | 23.20 | 44.70 | 0 |
| 6 | 0.97 | 23.39 | 45.34 | 0 |
| 8 | 0.80 | 26.71 | 45.56 | 0 |
| 12 | 0.55 | 28.79 | 51.27 | 0 |

## e5

| Strategy | Seed | Useful-group fraction | Sandbox-seconds per useful group |
|---|---|---|---|
| band | 1 | 0.89 | 180 |
| band | 2 | 0.91 | 176 |
| band | 3 | 0.89 | 180 |
| uniform | 1 | 0.84 | 191 |
| uniform | 2 | 0.84 | 191 |
| uniform | 3 | 0.85 | 188 |

## e6

| Shape | Command s/leaf | Env s/leaf | Prefill tokens/leaf | Prefix hit rate | Wall s |
|---|---|---|---|---|---|
| flat | 8.88 | 9.06 | 1150 | 0.80 | 25.5 |
| tree-2x4 | 1.13 | 1.67 | 408 | 0.91 | 21.2 |
| tree-4x2 | 2.51 | 2.94 | 655 | 0.87 | 18.0 |

