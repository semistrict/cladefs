Bucket: cells 2000 objects 212855 bytes 150919682 largest-objects 961 largest-bytes 724637 (tenants 20, cells 100, segments 95, largest 950).

| side | at once | round | sweep | seconds | cells | deleted | requests | peak MiB |
|---|---|---|---|---|---|---|---|---|
| before | 1 | 1 | 1 | 205.1 | 2000 | 94428 | 371302 | 20 |
| before | 1 | 1 | 2 | 43.4 | 2000 | 86688 | 369138 | 24 |
| before | 1 | 1 | 3 | 39.3 | 1836 | 7739 | 338603 | 14 |
| after | 1 | 1 | 1 | 45.6 | 2000 | 94428 | 30021 | 9 |
| after | 1 | 1 | 2 | 32.9 | 2000 | 0 | 26021 | 7 |
| after | 1 | 1 | 3 | 1.2 | 0 | 0 | 4021 | 5 |
| before | 16 | 1 | 1 | 30.7 | 2000 | 94428 | 371302 | 55 |
| before | 16 | 1 | 2 | 8.0 | 2000 | 0 | 367302 | 38 |
| before | 16 | 1 | 3 | 0.9 | 0 | 0 | 4021 | 19 |
| after | 16 | 1 | 1 | 9.9 | 2000 | 94428 | 30021 | 30 |
| after | 16 | 1 | 2 | 6.8 | 2000 | 0 | 26021 | 14 |
| after | 16 | 1 | 3 | 0.2 | 0 | 0 | 4021 | 10 |
