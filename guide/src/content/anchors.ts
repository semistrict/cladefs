// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Every place the guide's content versions into the code: a file and an exact snippet of it,
// unique in that file (tools/check.ts fails the build otherwise). Content names anchors by id,
// never by line: lines drift with every edit, a snippet only when its code changes.

export interface Anchor {
  path: string;
  snippet: string;
  /** What the reader finds there, in a line. */
  note: string;
}

export const anchors = {
  vfs_fsync: { path: "guest/linux/fs/cladefs/super.c", snippet: "int cladefs_vfs_fsync(struct file *file, loff_t start, loff_t end, int datasync)", note: "Linux's fsync of a guest Fxfs file: write back, then cladefs_fsync." },
  cladefs_fsync: { path: "guest/rust/cladefs-linux/src/lib.rs", snippet: "pub unsafe extern \"C\" fn cladefs_fsync(fs: *const Fxfs) -> c_int {", note: "Where the guest kernel's C calls into Fxfs's Rust." },
  posix_fsync: { path: "crates/fxfs-posix/src/lib.rs", snippet: "pub async fn fsync(&self) -> Result<(), Error> {", note: "Group commit: an fsync a sync already covers needs none of its own." },
  platform: { path: "crates/fxfs-posix/src/lib.rs", snippet: "pub trait Platform: Send + Sync + Sized + 'static", note: "What differs between the guest kernel and the host's tools, behind one POSIX layer." },
  journal_commit: { path: "crates/fxfs/src/object_store/journal.rs", snippet: "pub async fn commit(&self, transaction: &mut Transaction<'_>) -> Result<u64, Error>", note: "Fuchsia's journal: a transaction's mutations committed as a group." },
  commit_root_barrier: { path: "crates/fxfs/src/object_store/journal.rs", snippet: "device.commit_root(&root).await.map(|()| true)", note: "cladefs's change to Fuchsia's journal: its flush commits the root on the extent device." },
  transaction: { path: "crates/fxfs/src/object_store/transaction.rs", snippet: "pub struct Transaction<'a>", note: "Fuchsia's Transaction: mutation records committed together." },
  lsm: { path: "crates/fxfs/src/lsm_tree.rs", snippet: "pub struct LSMTree<K, V>", note: "Fuchsia's LSM tree, Fxfs's key/value store of layers." },
  object_store: { path: "crates/fxfs/src/object_store.rs", snippet: "pub struct ObjectStore", note: "Fuchsia's ObjectStore: files as objects." },
  device_trait: { path: "crates/storage_device/src/lib.rs", snippet: "pub trait Device: Send + Sync", note: "Fuchsia's Device trait, which cladefs extended with extents." },
  extent_id: { path: "crates/storage_device/src/extent.rs", snippet: "pub struct ExtentId {", note: "cladefs's own: an extent is named by (cell, sequence)." },
  extent_device: { path: "guest/rust/cladefs-linux/src/device.rs", snippet: "pub struct ExtentDevice", note: "The guest's extent-store device, which Fxfs runs on." },
  dev_commit_root: { path: "guest/rust/cladefs-linux/src/device.rs", snippet: "async fn commit_root(&self, root: &[u8]) -> Result<(), Error>", note: "Ordered like the device's other commits; uploaded only while a sync wants it." },
  device_submit: { path: "guest/linux/fs/cladefs/device.c", snippet: "int cladefs_device_submit(struct cladefs_device *dev, const struct cladefs_device_op *op,", note: "The guest C side of a device request." },
  virtio_submit: { path: "guest/linux/drivers/virtio_extent/driver.c", snippet: "int virtio_extent_submit(struct virtio_extent *vext, struct vext_request *req)", note: "Onto a virtqueue, one per CPU." },
  vmm_handle: { path: "crates/virtio_extent/src/requests.rs", snippet: "pub async fn handle<S: Backend>(", note: "The device serving one guest request through the daemon: virtio_extent, which every VMM hosting the device shares." },
  vmm_window: { path: "crates/virtio_extent/src/window.rs", snippet: "pub(crate) struct WindowMemory", note: "The DAX window in the VM process: guest physical addresses and the host memory KVM maps there." },
  cell_client: { path: "crates/cladefs/src/cladefsd/client.rs", snippet: "pub struct CellClient", note: "The VM process's handle on its cell, which the extent daemon serves." },
  daemon_commit: { path: "crates/cladefs/src/cladefsd/client.rs", snippet: "pub fn commit(&self, root: Option<&[u8]>) -> Result<()>", note: "COMMIT with COMMIT_UPLOADED, over the daemon socket." },
  server_serve: { path: "crates/cladefs/src/cladefsd/server.rs", snippet: "fn serve(&self, request: &Request<'_>, answer: Answer)", note: "cladefsd serving one request of a VM process." },
  flush_behind: { path: "crates/cladefs/src/cladefsd/server.rs", snippet: "async fn flush_behind(open_cell: Arc<OpenCell>, mut stopped: tokio::sync::oneshot::Receiver<()>)", note: "Every 50 ms: the commits ordered so far made local." },
  server_fault: { path: "crates/cladefs/src/cladefsd/server.rs", snippet: "async fn fault(self: Arc<Self>, fault: Fault, answer: Answer)", note: "A guest fault on the window, which userfaultfd reported, resolved by the daemon." },
  open_cell: { path: "crates/cladefs/src/open_cell.rs", snippet: "pub struct OpenCell", note: "A cell, open, as a VM's extent store device serves it." },
  es_write: { path: "crates/cladefs/src/open_cell.rs", snippet: "pub async fn write(", note: "WRITE: bytes into an open extent." },
  es_commit: { path: "crates/cladefs/src/open_cell.rs", snippet: "pub async fn commit(&self, root: Option<&[u8]>) -> Result<()>", note: "COMMIT: seals the extents written before it; nothing in a window changes." },
  es_uploaded: { path: "crates/cladefs/src/open_cell.rs", snippet: "fn make_uploaded(&self) -> Result<()>", note: "Uploaded: local, synced to the host's disk, then acknowledged by the bucket if the store has one." },
  es_map: { path: "crates/cladefs/src/open_cell.rs", snippet: "pub async fn map(", note: "MAP: a sealed extent's pages into the window, read-only." },
  es_fork: { path: "crates/cladefs/src/open_cell.rs", snippet: "pub async fn fork(&self, path: impl AsRef<Path>, tenant: u64) -> Result<CellId, Error>", note: "A fork: a new cell, as of the last commit, made uploaded first, its ID the bucket's to give." },
  local_cell: { path: "crates/local_cell/src/cell.rs", snippet: "pub struct LocalCell<D: Dir>", note: "A cell on the host's disk: the bucket's write-ahead log and cache." },
  local_commit: { path: "crates/local_cell/src/cell.rs", snippet: "pub async fn commit(&self, root: Option<&[u8]>) -> Result<(), Error> {", note: "The commit of a cell on the host." },
  fork_head: { path: "crates/local_cell/src/upload.rs", snippet: "pub async fn forked(&self, child: Child, inheriting: keys::Inheriting) -> Result<(), Failure> {", note: "The fork of a version: the child's head, made in the bucket." },
  upload: { path: "crates/bucket_cell/src/writer.rs", snippet: "pub async fn upload(&self, segment: u64) -> Result<(), Error>", note: "A segment to the bucket." },
  acknowledge: { path: "crates/bucket_cell/src/writer.rs", snippet: "pub async fn acknowledge(&self) -> Result<Option<u64>, Error>", note: "Acknowledged by swapping the cell's head, only while it is still the writer's." },
  head_swap: { path: "crates/bucket_cell/src/head.rs", snippet: "pub async fn swap(", note: "The cell's head, swapped on the condition that it is the version its holder wrote." },
  bucket: { path: "crates/bucket_cell/src/bucket.rs", snippet: "pub trait Bucket: Send + Sync + 'static", note: "Anything with conditional writes: S3, GCS, a directory." },
  origin: { path: "crates/bucket_cell/src/layout.rs", snippet: "pub struct Origin {", note: "A child's origin: its parent and the segment its version closed. It inherits its parent's extents without copying." },
  fetcher: { path: "crates/bucket_cell/src/fetch.rs", snippet: "pub struct Fetcher<B: Bucket>", note: "Reads extents from a bucket by parts, under a budget." },
  dax_window: { path: "crates/cladefs/src/dax.rs", snippet: "pub struct DaxWindow(Arc<Window>);", note: "A DAX window of an extent-store device." },
  dax_fault: { path: "crates/cladefs/src/dax.rs", snippet: "pub async fn fault(&self, offset: u64, access: Access) -> Result<()>", note: "The guest's fault on the window: map the page." },
  cow: { path: "crates/cladefs/src/dax/uffd.rs", snippet: "pub(crate) struct PrivateMemory {", note: "How a window copies on write: the VM process's private memory, laid out as the window is, holding private_max pages at most." },
  second_stage: { path: "crates/cladefs/src/dax.rs", snippet: "pub trait SecondStage: Send + Sync", note: "A window's translation outside the daemon: the VM process's memory." },
  uffd_serve: { path: "crates/cladefs/src/dax/uffd.rs", snippet: "pub fn serve(self: &Arc<Self>, window: Arc<dyn Faults>) -> Result<FaultThread, Error> {", note: "The VM process's thread taking KVM's faults by userfaultfd." },
  cached_extent: { path: "crates/cladefs/src/cache.rs", snippet: "pub(crate) struct CachedExtent", note: "One sealed extent in the cache: a table of its pages." },
  evict: { path: "crates/cladefs/src/cache.rs", snippet: "pub(crate) fn evict_least_used(arena: Option<ArenaId>, enough: impl FnMut(u64) -> bool) -> u64 {", note: "Eviction, least used first, windows' mappings last." },
  relieve: { path: "crates/cladefs/src/slabs/relief.rs", snippet: "pub fn relieve_with(", note: "Relief of arenas starved at their caps." },
  arena: { path: "crates/cladefs/src/pages.rs", snippet: "pub struct Arena(ArenaId);", note: "An arena of physical memory: a tenant's." },
  guest_fault: { path: "guest/linux/fs/cladefs/dax.c", snippet: "static vm_fault_t cladefs_dax_do_fault(struct vm_fault *vmf, unsigned int order, bool write)", note: "The guest's DAX fault." },
  guest_writeback: { path: "guest/linux/fs/cladefs/dax.c", snippet: "int cladefs_dax_write_back(struct inode *inode)", note: "The guest's DAX writeback." },
  snapshot_volume: { path: "crates/fxfs/src/object_store/volume/snapshot.rs", snippet: "pub async fn snapshot_volume(", note: "cladefs's addition to Fuchsia's Fxfs: a volume snapshot sharing layers and extents." },
  es_commit_ordered: { path: "crates/cladefs/src/open_cell.rs", snippet: "pub async fn commit_ordered(&self, root: Option<&[u8]>) -> Result<()>", note: "A barrier: ordered, not yet local." },
  guest_dirty: { path: "guest/linux/fs/cladefs/dax.c", snippet: "static int cladefs_dax_dirty(struct inode *inode, struct cladefs_dax_range *range, unsigned long first,", note: "Before a page is dirty, the host makes it private (PRIVATE): dirty implies private." },
  guest_rebind_range: { path: "guest/linux/fs/cladefs/dax.c", snippet: "static long cladefs_dax_rebind_range(struct cladefs_dax *dax, struct cladefs *fs,", note: "Rebinding: clean private pages MAPped anew to their sealed extents, the semaphore held for writing." },
  guest_swap: { path: "guest/linux/fs/cladefs/dax.c", snippet: "pages += cladefs_dax_range_swap(range, batch[nr].pages);", note: "Writeback swaps the dirty set out for an empty one." },
  guest_write_back_keeping: { path: "guest/linux/fs/cladefs/dax.c", snippet: "static int cladefs_dax_write_back_keeping(struct inode *inode, bool keep)", note: "Writeback of a DAX file's dirty pages." },
  pages_swap: { path: "guest/linux/fs/cladefs/pages.c", snippet: "unsigned long cladefs_dax_pages_swap(struct cladefs_dax_pages *pages, unsigned long *set)", note: "The swap of a range's dirty set for its writeback set." },
  guest_device_private: { path: "guest/linux/fs/cladefs/device.c", snippet: "int cladefs_device_private(struct cladefs_device *dev, u64 window_offset, u64 len)", note: "PRIVATE: ask the host for a private copy of window pages." },
  window_make_private: { path: "crates/cladefs/src/dax/uffd.rs", snippet: "pub(crate) async fn make_private(", note: "The host side of PRIVATE, in the VM process: each page copied from its arena, at the same offset." },
  server_make_private: { path: "crates/cladefs/src/cladefsd/client.rs", snippet: "pub async fn make_private_async(&self, window_offset: u64, len: u64) -> Result<()> {", note: "The VM process serving a PRIVATE itself, no round trip to cladefsd (doc-26)." },
  vmm_private_max: { path: "crates/virtio_extent/src/config.rs", snippet: "pub fn private_max(len: u64, configured: Option<u64>) -> u64 {", note: "private_max: 1 GiB by default, whatever the window's size." },
  cache_cached: { path: "crates/cladefs/src/cache.rs", snippet: "pub(crate) async fn cached(", note: "A sealed extent in the cache, read in from a cell that holds it if it isn't." },
  uffd_copy_on_write: { path: "crates/cladefs/src/dax/uffd.rs", snippet: "pub(crate) fn copy_on_write(&self, most: u64) -> Result<(), Error> {", note: "The VM process's second stage made to copy on write, bounding its private pages." },
  extent_cache: { path: "crates/cladefs/src/cache.rs", snippet: "struct ExtentCache", note: "The process's one cache of sealed extents, shared by every open cell and window." },
  sim_swap_during_write: { path: "guest/sim/tests/scenarios.c", snippet: "static void cow_swap_during_write(void)", note: "guest/sim's scenario that kills the swap-shared mutant." },
} as const satisfies Record<string, Anchor>;

export type AnchorId = keyof typeof anchors;
