Others' I/O beside it: UNCHECKED: /work/cladefs/target/host-runs/run-r14378b-20261127T154045Z-d52abb55/results: on cladefs (rootfs), no block device found
Took 2299s.
Host: linux. VMM: firecracker. Iterations: ext4-guest-kernel 3, ext4-dax-pmem 3, fxfs-guest 3, fxfs-guest-dax 3, fxfs-guest-dax-unsafe 3.

| Workload | Unit | ext4-guest-kernel | ext4-dax-pmem | fxfs-guest | fxfs-guest-dax | fxfs-guest-dax-unsafe | ext4-dax-pmem vs ext4-guest-kernel | fxfs-guest vs ext4-dax-pmem | fxfs-guest-dax vs ext4-dax-pmem | fxfs-guest-dax-unsafe vs fxfs-guest-dax |
|---|---|---|---|---|---|---|---|---|---|---|
| seq-write | MiB/s | 101 (86.8–103) | 56.1 (50.3–66.3) | 84.8 (75.8–86.1) | 80.9 (44.3–83.4) | 79.6 (73.2–81.1) | 1.80× | 0.66× | 0.69× | 1.02× |
| rand-read-cold | IOPS | 25,245 (23,883–26,728) | 45,894 (45,765–46,282) | 4,316 (4,301–4,789) | 6,668 (6,237–8,039) | 12,154 (8,450–13,665) | 0.55× | **10.63×** | **6.88×** | 0.55× |
| seq-read-cold | MiB/s | 480 (408–2,844) | 153 (149–155) | 40.2 (23.6–40.5) | 139 (132–140) | 139 (107–141) | **3.13×** | **3.82×** | 1.10× | 1.00× |
| seq-read-warm | MiB/s | 26,638 (26,611–26,779) | 32,619 (32,449–32,851) | 26,664 (23,708–26,876) | 28,790 (28,452–28,977) | 29,283 (29,014–29,407) | 0.82× | 1.22× | 1.13× | 0.98× |
| rand-read-warm | IOPS | 1,085,240 (1,066,413–1,097,137) | 836,977 (830,567–883,201) | 1,114,444 (1,089,017–1,124,018) | 843,855 (836,324–847,133) | 835,154 (834,796–837,776) | 1.30× | 0.75× | 0.99× | 1.01× |
| mmap-seq-read | MiB/s | 25,600 (7,314–25,600) | 18,286 (13,474–21,333) | 17,067 (17,067–18,286) | 23,273 (23,273–25,600) | 25,600 (23,273–25,600) | 1.40× | 1.07× | 0.79× | 0.91× |
| rand-write | IOPS | 60,458 (47,353–70,017) | 7,854 (7,250–8,359) | 46,152 (38,460–47,353) | 3,643 (3,249–4,217) | 3,712 (3,630–3,789) | **7.70×** | 0.17× | **2.16×** | 0.98× |
| mmap-rand-read | IOPS | 2,116,968 (2,033,655–2,159,121) | 31,928 (30,814–32,433) | 2,130,989 (2,127,071–2,135,658) | 2,332,283 (2,327,764–2,332,860) | 2,337,502 (2,267,257–2,345,424) | **66.31×** | 0.01× | 0.01× | 1.00× |
| mmap-rand-write | IOPS | 16,384 (13,631–17,945) | 4,046 (3,794–4,117) | 9,638 (9,293–10,145) | 40,157 (38,102–40,554) | 40,858 (40,157–41,903) | **4.05×** | 0.42× | 0.10× | 0.98× |
| fsync-append | IOPS | 861 (859–895) | 149 (132–223) | 1,686 (1,636–1,736) | 2,020 (1,992–2,229) | 1,900 (1,822–1,975) | **5.79×** | 0.09× | 0.07× | 1.06× |
| sqlite-pread-scan | MiB/s | 565 (436–1,266) | 120 (117–121) | 80.6 (51.5–84.7) | 62.1 (61.4–71.4) | 74.8 (70.0–75.1) | **4.71×** | 1.49× | 1.93× | 0.83× |
| sqlite-pread-scan-warm | MiB/s | 3,968 (3,832–4,161) | 3,238 (2,996–3,286) | 4,217 (3,095–4,268) | 2,891 (2,762–3,293) | 3,161 (2,723–3,257) | 1.23× | 0.77× | 1.12× | 0.91× |
| sqlite-mmap-scan | MiB/s | 1,544 (1,283–2,038) | 3,412 (1,832–3,663) | 535 (474–605) | 3,616 (2,659–4,571) | 4,348 (4,285–4,371) | 0.45× | **6.38×** | 0.94× | 0.83× |
| sqlite-mmap-scan-warm | MiB/s | 4,092 (3,977–4,238) | 4,768 (2,261–4,894) | 4,086 (3,960–4,513) | 4,934 (4,032–4,953) | 4,688 (2,369–4,940) | 0.86× | 1.17× | 0.97× | 1.05× |
| create | ops/s | 57,730 (38,945–58,368) | 18,946 (14,850–19,153) | 54,025 (48,072–64,430) | 49,479 (49,315–49,513) | 58,437 (47,220–63,347) | **3.05×** | 0.35× | 0.38× | 0.85× |
| stat-cold | ops/s | 197,134 (134,955–221,666) | 202,987 (115,192–226,406) | 126,621 (88,795–150,024) | 94,754 (93,082–98,321) | 137,991 (99,574–142,720) | 0.97× | 1.60× | **2.14×** | 0.69× |
| stat-warm | ops/s | 607,011 (479,962–909,216) | 886,208 (822,557–918,038) | 850,734 (759,396–866,314) | 847,404 (522,438–862,470) | 802,463 (704,272–888,623) | 0.68× | 1.04× | 1.05× | 1.06× |
| lookup-missing | ops/s | 297,625 (262,917–322,462) | 258,186 (222,078–321,294) | 389,865 (270,190–390,520) | 268,139 (228,667–294,143) | 273,573 (262,025–353,293) | 1.15× | 0.66× | 0.96× | 0.98× |
| readdir | ops/s | 3,333,578 (1,840,826–3,661,696) | 3,262,909 (2,834,331–3,604,182) | 1,855,455 (1,705,217–2,074,863) | 1,517,562 (1,103,929–1,894,368) | 1,777,350 (1,017,885–2,951,542) | 1.02× | 1.76× | **2.15×** | 0.85× |
| read-small-cold | ops/s | 18,818 (18,302–19,554) | 22,815 (22,624–23,996) | 8,206 (7,148–9,258) | 15,559 (14,107–18,293) | 18,858 (18,194–19,127) | 0.82× | **2.78×** | 1.47× | 0.83× |
| chmod | ops/s | 430,474 (419,883–432,841) | 435,001 (433,656–436,234) | 223,459 (210,291–326,654) | 313,609 (220,446–319,324) | 318,437 (135,259–326,633) | 0.99× | 1.95× | 1.39× | 0.98× |
| setxattr | ops/s | 308,262 (303,631–374,003) | 356,370 (302,910–372,087) | 151,330 (147,546–157,753) | 139,911 (101,730–143,081) | 137,871 (134,264–148,998) | 0.87× | **2.35×** | **2.55×** | 1.01× |
| link | ops/s | 97,062 (94,954–98,972) | 92,608 (89,710–100,407) | 77,058 (74,552–83,187) | 76,871 (50,729–79,078) | 76,191 (73,332–80,031) | 1.05× | 1.20× | 1.20× | 1.01× |
| rename | ops/s | 82,538 (79,200–83,405) | 82,563 (73,833–83,366) | 62,618 (62,001–66,405) | 55,695 (42,270–63,179) | 58,868 (57,643–64,703) | 1.00× | 1.32× | 1.48× | 0.95× |
| rename-across | ops/s | 67,165 (66,714–69,807) | 69,179 (66,056–69,748) | 55,683 (34,615–63,387) | 53,842 (39,994–54,853) | 55,089 (54,918–57,134) | 0.97× | 1.24× | 1.28× | 0.98× |
| unlink | ops/s | 220,442 (206,843–247,511) | 245,734 (179,580–246,820) | 227,530 (185,726–229,140) | 215,297 (179,979–267,181) | 224,022 (130,574–267,338) | 0.90× | 1.08× | 1.14× | 0.96× |
| create-fsync | ops/s | 666 (403–786) | 147 (117–437) | 2,051 (1,516–2,364) | 1,821 (1,033–1,985) | 1,911 (1,821–2,303) | **4.54×** | 0.07× | 0.08× | 0.95× |
| mkdir-tree | ops/s | 77,147 (62,768–78,543) | 17,476 (15,832–17,683) | 63,913 (38,790–65,310) | 63,042 (50,547–63,871) | 59,017 (49,597–61,336) | **4.41×** | 0.27× | 0.28× | 1.07× |
| find-cold | ops/s | 211,099 (161,110–219,121) | 223,708 (115,788–224,660) | 141,191 (125,666–142,712) | 88,809 (86,911–134,032) | 127,809 (99,111–140,316) | 0.94× | 1.58× | **2.52×** | 0.69× |
| rm-tree | ops/s | 67,317 (67,264–70,864) | 67,528 (64,567–71,288) | 60,613 (58,070–62,137) | 53,443 (41,655–61,142) | 59,443 (54,903–66,500) | 1.00× | 1.11× | 1.26× | 0.90× |
| large-dir-create | ops/s | 50,716 (44,395–66,626) | 44,260 (39,497–56,770) | 63,732 (49,054–73,700) | 50,505 (20,637–72,353) | 59,070 (20,148–72,434) | 1.15× | 0.69× | 0.88× | 0.86× |
| large-dir-stat-cold | ops/s | 201,384 (167,642–202,153) | 203,420 (191,786–203,611) | 148,008 (110,267–155,161) | 144,719 (88,151–148,792) | 114,242 (93,366–148,153) | 0.99× | 1.37× | 1.41× | 1.27× |
| large-dir-unlink | ops/s | 132,857 (130,630–135,505) | 129,719 (108,425–132,067) | 158,729 (153,927–185,608) | 172,875 (129,985–184,973) | 165,155 (133,289–185,900) | 1.02× | 0.82× | 0.75× | 1.05× |
| create-parallel | ops/s | 110,135 (106,657–121,314) | 119,210 (102,364–122,296) | 108,977 (107,957–128,539) | 98,537 (84,759–108,243) | 101,262 (39,833–120,052) | 0.92× | 1.09× | 1.21× | 0.97× |
| stat-parallel | ops/s | 999,568 (982,445–1,060,023) | 1,013,705 (933,350–1,140,349) | 614,522 (432,402–713,146) | 455,599 (352,823–488,155) | 562,231 (373,645–639,483) | 0.99× | 1.65× | **2.22×** | 0.81× |
| unlink-parallel | ops/s | 257,257 (248,578–266,571) | 249,269 (245,952–255,900) | 177,317 (174,642–184,717) | 142,927 (134,868–149,977) | 180,584 (133,840–184,405) | 1.03× | 1.41× | 1.74× | 0.79× |
| create-shared-dir | ops/s | 78,280 (76,115–79,199) | 76,712 (74,719–76,845) | 82,136 (73,437–82,642) | 79,576 (63,746–82,167) | 83,070 (78,551–83,422) | 1.02× | 0.93× | 0.96× | 0.96× |
| unlink-shared-dir | ops/s | 232,548 (208,556–235,817) | 218,415 (212,410–234,066) | 162,061 (154,977–169,312) | 148,494 (131,310–163,629) | 164,808 (164,090–171,209) | 1.06× | 1.35× | 1.47× | 0.90× |
| untar | s | 0.02 (0.01–0.04) | 0.04 (0.02–0.13) | 0.01 (0.01–0.01) | 0.02 (0.02–0.02) | 0.01 (0.01–0.01) | 1.62× | 0.33× | 0.51× | 0.70× |
| make | s | 3.31 (3.06–3.42) | 3.99 (3.42–10.3) | 3.08 (3.07–3.08) | 3.44 (3.29–8.16) | 3.21 (3.10–7.55) | 1.20× | 0.77× | 0.86× | 0.93× |
| build-total | s | 3.34 (3.08–3.45) | 4.01 (3.46–10.5) | 3.09 (3.09–3.09) | 3.45 (3.31–8.18) | 3.22 (3.12–7.56) | 1.20× | 0.77× | 0.86× | 0.93× |

- ext4-guest-kernel: mmap-seq-read-faults-per-mib 6.30 median (6.30–6.30)
- ext4-guest-kernel: mtime-ahead-of-guest-ms -4.13 median (-4.67–-3.10)
- ext4-guest-kernel: sqlite-mmap-scan-faults-per-mib 13.6 median (13.5–13.6)
- ext4-guest-kernel: sqlite-mmap-scan-warm-faults-per-mib 11.9 median (11.9–11.9)
- ext4-guest-kernel: sqlite-pread-scan-faults-per-mib 2.80 median (2.80–2.80)
- ext4-guest-kernel: sqlite-pread-scan-warm-faults-per-mib 2.60 median (2.60–2.60)
- ext4-dax-pmem: mmap-seq-read-faults-per-mib 5.60 median (5.60–5.60)
- ext4-dax-pmem: mtime-ahead-of-guest-ms -4.28 median (-4.42–-1.73)
- ext4-dax-pmem: sqlite-mmap-scan-faults-per-mib 3.80 median (3.80–129)
- ext4-dax-pmem: sqlite-mmap-scan-warm-faults-per-mib 3.60 median (3.60–129)
- ext4-dax-pmem: sqlite-pread-scan-faults-per-mib 2.80 median (2.80–2.80)
- ext4-dax-pmem: sqlite-pread-scan-warm-faults-per-mib 2.60 median (2.60–2.60)
- fxfs-guest: mmap-seq-read-faults-per-mib 13.7 median (13.7–13.8)
- fxfs-guest: mtime-ahead-of-guest-ms -4.48 median (-6.03–-3.49)
- fxfs-guest: sqlite-mmap-scan-faults-per-mib 9.70 median (9.70–9.70)
- fxfs-guest: sqlite-mmap-scan-warm-faults-per-mib 9.30 median (9.30–9.30)
- fxfs-guest: sqlite-pread-scan-faults-per-mib 2.80 median (2.70–2.80)
- fxfs-guest: sqlite-pread-scan-warm-faults-per-mib 2.60 median (2.60–2.60)
- fxfs-guest-dax: mmap-seq-read-faults-per-mib 5.50 median (5.50–5.60)
- fxfs-guest-dax: mtime-ahead-of-guest-ms -4.06 median (-4.52–-3.01)
- fxfs-guest-dax: sqlite-mmap-scan-faults-per-mib 1.80 median (1.80–1.80)
- fxfs-guest-dax: sqlite-mmap-scan-warm-faults-per-mib 1.70 median (1.60–1.70)
- fxfs-guest-dax: sqlite-pread-scan-faults-per-mib 2.80 median (2.80–2.80)
- fxfs-guest-dax: sqlite-pread-scan-warm-faults-per-mib 2.60 median (2.60–2.60)
- fxfs-guest-dax-unsafe: mmap-seq-read-faults-per-mib 5.50 median (5.50–5.50)
- fxfs-guest-dax-unsafe: mtime-ahead-of-guest-ms -2.44 median (-2.87–-2.30)
- fxfs-guest-dax-unsafe: sqlite-mmap-scan-faults-per-mib 1.80 median (1.80–1.80)
- fxfs-guest-dax-unsafe: sqlite-mmap-scan-warm-faults-per-mib 1.70 median (1.60–1.70)
- fxfs-guest-dax-unsafe: sqlite-pread-scan-faults-per-mib 2.80 median (2.70–2.80)
- fxfs-guest-dax-unsafe: sqlite-pread-scan-warm-faults-per-mib 2.60 median (2.60–2.60)

Object storage: the extent daemon's requests to its bucket, for the cases that kept their
cells there, at GCS Standard single-region prices (class A $0.005 per 1,000, class B
$0.0004 per 1,000, deletes free).

| Request | Count | Class | Cost |
|---|---|---|---|
| get | 103 | B | $0.000041 |
| get-range | 3 | B | $0.000001 |
| head | 1 | B | $0.000000 |
| list | 0 | A | $0.000000 |
| create | 156 | A | $0.000780 |
| update | 91 | A | $0.000455 |
| overwrite | 0 | A | $0.000000 |
| delete | 1 | free | $0.000000 |
| all | 355 | | $0.001278 |

Uploaded 5.3 GiB (segments 5.3 GiB, extents 0 B, checkpoints 5.8 KiB, other 22.0 KiB); downloaded 224.9 KiB.

Extent daemon: the VM processes' requests, as it served them, from their dispatch to their
reply.

| Request | Count | Each |
|---|---|---|
| open | 3 | 8,812.7 µs |
| read | 60,914 | 251.0 µs |
| write | 110,899 | 325.9 µs |
| free | 446 | 1,438.4 µs |
| map | 1,566 | 72.3 µs |
| unmap | 1,566 | 19.3 µs |
| commit | 165 | 31,486.3 µs |
| root | 3 | 11.7 µs |
| info | 9 | 25.6 µs |
| window | 2 | 61.0 µs |
| fault | 27,270 | 397.3 µs |
| transfer | 3 | 44.6 µs |

Commits: 20,570, 301.5 µs each: committing the extent store 42.2 µs, waiting for object storage 259.2 µs.

Segments closed and framed: 144, 39.0 ms each.

Over 2× slower than ext4:
- rand-read-cold on fxfs-guest: 10.63× ext4-dax-pmem's
- rand-read-cold on fxfs-guest-dax: 6.88× ext4-dax-pmem's
- seq-read-cold on ext4-dax-pmem: 3.13× ext4-guest-kernel's
- seq-read-cold on fxfs-guest: 3.82× ext4-dax-pmem's
- rand-write on ext4-dax-pmem: 7.70× ext4-guest-kernel's
- rand-write on fxfs-guest-dax: 2.16× ext4-dax-pmem's
- mmap-rand-read on ext4-dax-pmem: 66.31× ext4-guest-kernel's
- mmap-rand-write on ext4-dax-pmem: 4.05× ext4-guest-kernel's
- fsync-append on ext4-dax-pmem: 5.79× ext4-guest-kernel's
- sqlite-pread-scan on ext4-dax-pmem: 4.71× ext4-guest-kernel's
- sqlite-mmap-scan on fxfs-guest: 6.38× ext4-dax-pmem's
- create on ext4-dax-pmem: 3.05× ext4-guest-kernel's
- stat-cold on fxfs-guest-dax: 2.14× ext4-dax-pmem's
- readdir on fxfs-guest-dax: 2.15× ext4-dax-pmem's
- read-small-cold on fxfs-guest: 2.78× ext4-dax-pmem's
- setxattr on fxfs-guest: 2.35× ext4-dax-pmem's
- setxattr on fxfs-guest-dax: 2.55× ext4-dax-pmem's
- create-fsync on ext4-dax-pmem: 4.54× ext4-guest-kernel's
- mkdir-tree on ext4-dax-pmem: 4.41× ext4-guest-kernel's
- find-cold on fxfs-guest-dax: 2.52× ext4-dax-pmem's
- stat-parallel on fxfs-guest-dax: 2.22× ext4-dax-pmem's
