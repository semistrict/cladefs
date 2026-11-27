Bucket: cells 1 objects 202 bytes 31784616 largest-objects 202 largest-bytes 31784616 (tenants 1, cells 1, segments 4, largest 200).

| side | at once | round | sweep | seconds | cells | deleted | requests | peak MiB |
|---|---|---|---|---|---|---|---|---|
| a | 1 | 1 | 1 | 0.7 | 1 | 0 | 475 | 561 |
| b | 1 | 1 | 1 | 0.2 | 1 | 0 | 518 | 287 |
| b | 1 | 2 | 1 | 0.2 | 1 | 0 | 518 | 283 |
| a | 1 | 2 | 1 | 0.6 | 1 | 0 | 475 | 563 |
| a | 1 | 3 | 1 | 0.6 | 1 | 0 | 475 | 515 |
| b | 1 | 3 | 1 | 0.2 | 1 | 0 | 518 | 289 |

| side | at once | sweep | rounds | median s | least s | most s |
|---|---|---|---|---|---|---|
| a | 1 | 1 | 3 | 0.6 | 0.6 | 0.7 |
| b | 1 | 1 | 3 | 0.2 | 0.2 | 0.2 |
