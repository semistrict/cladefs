// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The extent daemon's control API (TASK-43.3): what the host's VM manager, which launches the
// VMMs, asks cladefsd to do with the VMs' disks, the extent stores it serves them. It is
// served over a Unix socket (`CLADEFSD_GRPC`), the daemon's user's alone, mode 0600 whatever
// its umask (TASK-159), as every caller may do everything. VM processes do not use it: they keep
// the extent protocol on the daemon's VM sockets, which carries their memory as file descriptors.
//
// Tenants (Grant, Revoke) are defence in depth, should a guest escape into its VMM: a tenant's
// VM processes connect to the tenant's own socket, through which they name cells in the
// tenant's directory alone, and hold the tenant's cached pages alone, with public bases' pages
// read-only. Guest isolation is the security boundary, which nothing relies on tenants for: every
// VM may as well be of one tenant, or of none, on the daemon's own VM socket. Grants are not kept
// across restarts of the daemon: the caller grants again.
//
// A version is a named state of a cell, made by its writer as one local commit (MakeVersion) and
// uploaded behind. Forks are made of versions, on any number of hosts at once, each making only
// its child's record, so that a restore on one host changes nothing another sees.
//
// A VM snapshot makes a version of each of the VM's cells, which its file names by tenant, cell
// and version, and restores on any number of hosts at once (TASK-154): each restore forks the
// versions into new cells of the restoring VM's tenant's, the daemon opening each version once on
// its host for every restore of it there. A version of the restoring tenant's own cells forks; one
// of another tenant's only while shared (ShareVersion; PERMISSION_DENIED, EACCES, otherwise); one
// dropped since (DropVersions) fails with NOT_FOUND (ENOENT), and what was restored from it lives
// on. A restore on another host than the snapshot's waits for nothing: the version is in the
// bucket once SyncCell of the VM's cell returns.
//
// A cell with a writer forks into cells of other tenants' only if it is the default tenant's, as
// the daemon's own cells and public bases are (TASK-395.9): a ForkCell of another tenant's into
// another tenant's directory fails with PERMISSION_DENIED (EACCES), making nothing. A fork of a
// cell with a writer is a commit, which takes its writer: such a cell forks on one
// host at a time. A base forked on many hosts is given a version first, and its version forked on
// each (ForkCell's `version`).
//
// Cells are named by `(tenant, cell)` alone (hotbed question 547): the daemon owns its managed
// JBOD disks (`[[disks]]` in TOML config) and chooses where each cell directory lives on the host.
// Callers never choose or pass host directories for cells. A cell a VM process has open is its
// connection's: what would change it under the guest's feet fails with FAILED_PRECONDITION and
// errno EBUSY, unless the call says otherwise.
//
// Errors are gRPC statuses whose code follows the errno the daemon failed with, which the
// `cladefs-errno` metadata entry carries, as a decimal number (Linux's): ENOENT is NOT_FOUND,
// EEXIST ALREADY_EXISTS, EACCES and EPERM PERMISSION_DENIED, EINVAL INVALID_ARGUMENT, ENOSPC,
// ENOMEM and EMSGSIZE RESOURCE_EXHAUSTED, ETIMEDOUT DEADLINE_EXCEEDED, EBUSY, ESTALE, ENOTSUP and
// EROFS FAILED_PRECONDITION, EIO and others INTERNAL.
//
// Calls block for as long as their work takes: a sync until the cell's commits are in object
// storage, an import for minutes. A caller bounds them with deadlines; one that gives up does not
// stop the work, which completes, or fails, on its own.
//
// No answer is larger than 64 MiB (board 8581): one that would be, as GetStats's and ListCells's
// grow with a host's VMs and cells, fails with RESOURCE_EXHAUSTED (EMSGSIZE), saying its size, and
// GetStats says what of it to leave out. A caller takes answers of that size: gRPC's own default,
// 4 MiB, which the Rust client (cladefs::cladefsd::control) and the Go client (go/cladefsd/v1)
// raise to it, failed GetStats at some hundreds of VMs. Requests are 4 MiB at most.
//
// Compatibility (TASK-50.5, doc-28): v1 only grows, checked against the last build's by
// go/compat.sh (board 10056). Calls and fields are added; none is removed, renamed, renumbered or
// given another meaning or type, and no field number is used twice (a field that goes is
// `reserved`). A field a daemon doesn't know of is zero, empty or false, which each says it means;
// a caller ignores fields it doesn't know of. A call an older daemon lacks fails with
// UNIMPLEMENTED. Errnos keep their codes. A change that breaks a caller is v2, served beside v1
// until callers move.
//
// Cells (TASK-364, doc-20): a tenant's cell has one kind, kept under the cell's prefix in the
// bucket, and a host takes a cell whole, its head with it. A cell is named by its tenant and its
// ID: `tenant` and `cell` beside one another.
// Tenants and cells are IDs alone, which the caller keeps the meaning of: every call names a
// tenant by its ID in the bucket, `tenant`, and no tenant has a name. A granted tenant's ID is the
// one Grant named; the daemon's own socket's VM processes make cells of the default tenant, whose
// ID is 0.

// Code generated by protoc-gen-go-grpc. DO NOT EDIT.
// versions:
// - protoc-gen-go-grpc v1.6.2
// - protoc             (unknown)
// source: cladefs/cladefsd/v1/cladefsd.proto

package cladefsdv1

import (
	context "context"
	grpc "google.golang.org/grpc"
	codes "google.golang.org/grpc/codes"
	status "google.golang.org/grpc/status"
)

// This is a compile-time assertion to ensure that this generated file
// is compatible with the grpc package it is being compiled against.
// Requires gRPC-Go v1.64.0 or later.
const _ = grpc.SupportPackageIsVersion9

const (
	Cladefsd_CreateCell_FullMethodName              = "/cladefs.cladefsd.v1.Cladefsd/CreateCell"
	Cladefsd_ImportImage_FullMethodName             = "/cladefs.cladefsd.v1.Cladefsd/ImportImage"
	Cladefsd_ImportMemory_FullMethodName            = "/cladefs.cladefsd.v1.Cladefsd/ImportMemory"
	Cladefsd_ForkCell_FullMethodName                = "/cladefs.cladefsd.v1.Cladefsd/ForkCell"
	Cladefsd_DeleteCell_FullMethodName              = "/cladefs.cladefsd.v1.Cladefsd/DeleteCell"
	Cladefsd_DropCell_FullMethodName                = "/cladefs.cladefsd.v1.Cladefsd/DropCell"
	Cladefsd_ResizeCell_FullMethodName              = "/cladefs.cladefsd.v1.Cladefsd/ResizeCell"
	Cladefsd_PlaceCell_FullMethodName               = "/cladefs.cladefsd.v1.Cladefsd/PlaceCell"
	Cladefsd_MakePublic_FullMethodName              = "/cladefs.cladefsd.v1.Cladefsd/MakePublic"
	Cladefsd_MakeVersion_FullMethodName             = "/cladefs.cladefsd.v1.Cladefsd/MakeVersion"
	Cladefsd_ListVersions_FullMethodName            = "/cladefs.cladefsd.v1.Cladefsd/ListVersions"
	Cladefsd_DropVersions_FullMethodName            = "/cladefs.cladefsd.v1.Cladefsd/DropVersions"
	Cladefsd_ShareVersion_FullMethodName            = "/cladefs.cladefsd.v1.Cladefsd/ShareVersion"
	Cladefsd_AttachBase_FullMethodName              = "/cladefs.cladefsd.v1.Cladefsd/AttachBase"
	Cladefsd_SyncCell_FullMethodName                = "/cladefs.cladefsd.v1.Cladefsd/SyncCell"
	Cladefsd_StatCell_FullMethodName                = "/cladefs.cladefsd.v1.Cladefsd/StatCell"
	Cladefsd_ListCells_FullMethodName               = "/cladefs.cladefsd.v1.Cladefsd/ListCells"
	Cladefsd_GetCounters_FullMethodName             = "/cladefs.cladefsd.v1.Cladefsd/GetCounters"
	Cladefsd_GetStats_FullMethodName                = "/cladefs.cladefsd.v1.Cladefsd/GetStats"
	Cladefsd_Grant_FullMethodName                   = "/cladefs.cladefsd.v1.Cladefsd/Grant"
	Cladefsd_Revoke_FullMethodName                  = "/cladefs.cladefsd.v1.Cladefsd/Revoke"
	Cladefsd_SetMemoryLimit_FullMethodName          = "/cladefs.cladefsd.v1.Cladefsd/SetMemoryLimit"
	Cladefsd_SetMemoryWeight_FullMethodName         = "/cladefs.cladefsd.v1.Cladefsd/SetMemoryWeight"
	Cladefsd_SetTenantInFlight_FullMethodName       = "/cladefs.cladefsd.v1.Cladefsd/SetTenantInFlight"
	Cladefsd_ShrinkMemory_FullMethodName            = "/cladefs.cladefsd.v1.Cladefsd/ShrinkMemory"
	Cladefsd_SetDiskLimit_FullMethodName            = "/cladefs.cladefsd.v1.Cladefsd/SetDiskLimit"
	Cladefsd_SetDiskWeight_FullMethodName           = "/cladefs.cladefsd.v1.Cladefsd/SetDiskWeight"
	Cladefsd_GetTenantDisk_FullMethodName           = "/cladefs.cladefsd.v1.Cladefsd/GetTenantDisk"
	Cladefsd_SetPeers_FullMethodName                = "/cladefs.cladefsd.v1.Cladefsd/SetPeers"
	Cladefsd_ShredCell_FullMethodName               = "/cladefs.cladefsd.v1.Cladefsd/ShredCell"
	Cladefsd_ForgetTenantKeys_FullMethodName        = "/cladefs.cladefsd.v1.Cladefsd/ForgetTenantKeys"
	Cladefsd_ShredTenant_FullMethodName             = "/cladefs.cladefsd.v1.Cladefsd/ShredTenant"
	Cladefsd_ReloadKms_FullMethodName               = "/cladefs.cladefsd.v1.Cladefsd/ReloadKms"
	Cladefsd_DecommissionHost_FullMethodName        = "/cladefs.cladefsd.v1.Cladefsd/DecommissionHost"
	Cladefsd_ListEphemeralHosts_FullMethodName      = "/cladefs.cladefsd.v1.Cladefsd/ListEphemeralHosts"
	Cladefsd_MigrateEphemeralCell_FullMethodName    = "/cladefs.cladefsd.v1.Cladefsd/MigrateEphemeralCell"
	Cladefsd_HandOverEphemeralCell_FullMethodName   = "/cladefs.cladefsd.v1.Cladefsd/HandOverEphemeralCell"
	Cladefsd_AbortEphemeralMigration_FullMethodName = "/cladefs.cladefsd.v1.Cladefsd/AbortEphemeralMigration"
	Cladefsd_ListDisks_FullMethodName               = "/cladefs.cladefsd.v1.Cladefsd/ListDisks"
	Cladefsd_SetDiskGovernance_FullMethodName       = "/cladefs.cladefsd.v1.Cladefsd/SetDiskGovernance"
	Cladefsd_WaitEphemeralHandOver_FullMethodName   = "/cladefs.cladefsd.v1.Cladefsd/WaitEphemeralHandOver"
)

// CladefsdClient is the client API for Cladefsd service.
//
// For semantics around ctx use and closing/ending streaming RPCs, please refer to https://pkg.go.dev/google.golang.org/grpc/?tab=doc#ClientConn.NewStream.
//
// The extent daemon's control API.
type CladefsdClient interface {
	// Makes an empty cell: a new one; or one kept in the daemon's bucket, which the daemon
	// takes once it opens it (a VM on another host than the cell's). RESOURCE_EXHAUSTED (ENOMEM)
	// while the host's memory is full: the VMs here need what is left (board 5662).
	CreateCell(ctx context.Context, in *CreateCellRequest, opts ...grpc.CallOption) (*CreateCellResponse, error)
	// Makes a cell holding an Fxfs filesystem of a container image: a base for VMs to fork.
	ImportImage(ctx context.Context, in *ImportImageRequest, opts ...grpc.CallOption) (*ImportImageResponse, error)
	// Makes a memory cell holding a VM snapshot's memory file (board 11222): its pages, those of
	// zeros discarded as holes, committed at once, and optionally a version made of it.
	ImportMemory(ctx context.Context, in *ImportMemoryRequest, opts ...grpc.CallOption) (*ImportMemoryResponse, error)
	// Forks a cell to a new cell sharing its data: live, if a VM has it open, at a
	// moment nothing is open on it (as with its guest's filesystem frozen), FAILED_PRECONDITION
	// (EBUSY) otherwise. A fork of a cell with a writer is a commit of it, made where that
	// writer is, one host at a time; a fork of a version (`version`) is its child's record alone,
	// made on any number of hosts at once. RESOURCE_EXHAUSTED (ENOMEM) while the host's memory is
	// full, as CreateCell.
	ForkCell(ctx context.Context, in *ForkCellRequest, opts ...grpc.CallOption) (*ForkCellResponse, error)
	// Deletes a cell, or a fork, and its branch in the daemon's bucket: its data goes but for what
	// other cells share of it, which stays until the last of them goes. What was forked from it
	// lives on.
	DeleteCell(ctx context.Context, in *DeleteCellRequest, opts ...grpc.CallOption) (*DeleteCellResponse, error)
	// Deletes this host's copy of a cell, or a fork, as DeleteCell does, but for its branch in the
	// daemon's bucket, which stays, with what else holds it. A cell whose
	// forks live here keeps its directory, which no longer opens, and takes no CreateCell until the
	// last of them goes. What its commits hold that the bucket doesn't goes with it: SyncCell first,
	// to keep it. FAILED_PRECONDITION (EBUSY) while a VM process has it open.
	DropCell(ctx context.Context, in *DropCellRequest, opts ...grpc.CallOption) (*DropCellResponse, error)
	// Sets a cell's capacity, the size of the disk its guest sees: a cell a VM has open grows at
	// once, the guest seeing it mounted, and shrinks if its guest's filesystem agrees, no lower than
	// what it holds (RESOURCE_EXHAUSTED otherwise); one none has open only grows.
	ResizeCell(ctx context.Context, in *ResizeCellRequest, opts ...grpc.CallOption) (*ResizeCellResponse, error)
	// Places a cell at a placement whose fencing token, its boot ID, is `token`: its writer takes
	// its branch in object storage with that token from then on, which a later placement's exceeds,
	// so that a placement cut off takes no branch back from the one after it.
	PlaceCell(ctx context.Context, in *PlaceCellRequest, opts ...grpc.CallOption) (*PlaceCellResponse, error)
	// Makes a cell a public base, and the branches it inherited from: their pages are cached once,
	// in memory every tenant's VM processes map read-only, and shared by every tenant's forks.
	MakePublic(ctx context.Context, in *MakePublicRequest, opts ...grpc.CallOption) (*MakePublicResponse, error)
	// Makes a version of a cell: a name for its state as of its last commit, with nothing
	// open on it (FAILED_PRECONDITION, EBUSY, otherwise), as with its guest's filesystem frozen. It
	// is one commit on this host, durable once this returns, and asks nothing of the daemon's
	// bucket: the cell's head names it once its commit is uploaded, behind. Forks are made of a
	// version on any host (ForkCell's `version`, a VM snapshot's restores).
	MakeVersion(ctx context.Context, in *MakeVersionRequest, opts ...grpc.CallOption) (*MakeVersionResponse, error)
	// Lists the versions of a cell: those its head names, then those made here not yet
	// uploaded.
	ListVersions(ctx context.Context, in *ListVersionsRequest, opts ...grpc.CallOption) (*ListVersionsResponse, error)
	// Drops versions of a cell. What forks of one inherited stays until they go.
	DropVersions(ctx context.Context, in *DropVersionsRequest, opts ...grpc.CallOption) (*DropVersionsResponse, error)
	// Shares an uploaded version, or no longer: while shared, it forks into cells of other tenants'
	// than its own, as a base image many tenants fork; unshared, such forks fail
	// (PERMISSION_DENIED, EACCES), and those made live on.
	ShareVersion(ctx context.Context, in *ShareVersionRequest, opts ...grpc.CallOption) (*ShareVersionResponse, error)
	// Attaches a fork of a cell to a cell, a VM's or stopped, and so to its forks and the VMs
	// restored from them: reads and maps of the fork's extents the cell doesn't hold read through
	// to it, read-only. Everything the fork holds is readable to the guest, which names extents, so
	// that a cell that isn't public attaches only to a cell of its tenant (PERMISSION_DENIED
	// otherwise). The fork stays until the caller deletes it: a VM with it open keeps it from going
	// (FAILED_PRECONDITION), but a stopped cell doesn't. No guest can ask for this.
	AttachBase(ctx context.Context, in *AttachBaseRequest, opts ...grpc.CallOption) (*AttachBaseResponse, error)
	// Returns once every commit of a cell, as of the call, is uploaded: in the daemon's bucket,
	// as before a clean stop.
	SyncCell(ctx context.Context, in *SyncCellRequest, opts ...grpc.CallOption) (*SyncCellResponse, error)
	// What a cell is, and holds. Of a cell a VM process has open, as it has it, at no cost however
	// many extents it holds; of one none has, the daemon opens it to say, as a VM's would, taking its
	// branch's head, and closes it again: time, and memory till then, as its extents (TASK-454).
	StatCell(ctx context.Context, in *StatCellRequest, opts ...grpc.CallOption) (*StatCellResponse, error)
	// Lists the cells the daemon serves: those VM processes write, and those open for others, such
	// as bases attached, with what StatCell says of each (TASK-50.5).
	ListCells(ctx context.Context, in *ListCellsRequest, opts ...grpc.CallOption) (*ListCellsResponse, error)
	// What the daemon did since it started: counters, which only grow.
	GetCounters(ctx context.Context, in *GetCountersRequest, opts ...grpc.CallOption) (*GetCountersResponse, error)
	// What the daemon, its VM processes and their guests counted, as of one moment, each with the
	// commit it was built of (TASK-111.4): the daemon asks every VM process that opened a cell, each
	// its guest, waits for their answers, a while at most, and reads its own counters then. Each VM
	// process is some 2 KiB of the answer with today's guests (some 50 counters and 12 histograms),
	// some 40 KiB with a guest saying near the most it may, 64 KiB (tests/control.rs measures both,
	// 1,960 and 40,568 bytes): 4 MiB, gRPC's default, is some hundreds of VMs at the most. Past
	// the most an answer is (64 MiB, above), RESOURCE_EXHAUSTED (EMSGSIZE), and the request's
	// `leave_out_*` choose what to leave out.
	GetStats(ctx context.Context, in *GetStatsRequest, opts ...grpc.CallOption) (*GetStatsResponse, error)
	// Grants a tenant, by its ID, a VM socket, making the tenant if it is new: the path of a socket
	// for the caller to bind-mount into the tenant's jails, through which VM processes open cells in
	// the tenant's directory alone. Granting a tenant again returns its socket, given to the uid and
	// gid named; with another directory, ALREADY_EXISTS.
	Grant(ctx context.Context, in *GrantRequest, opts ...grpc.CallOption) (*GrantResponse, error)
	// Revokes a tenant's grant: its socket goes, and takes no more connections. Its VM processes
	// connected keep their cells until they go; then the tenant goes, and its cached pages.
	Revoke(ctx context.Context, in *RevokeRequest, opts ...grpc.CallOption) (*RevokeResponse, error)
	// Limits the memory the daemon holds extents' pages in, at once (TASK-50.3): a tenant's, or
	// every tenant's together. Past a tenant's limit, the daemon evicts the clean pages of its
	// extents asked for longest ago; past the total, any tenant's; with none left to evict, what
	// would take more fails, with ENOMEM, which the VM's guest sees as an I/O error, and the daemon
	// goes on. A limit under what is held holds as pages are freed: ShrinkMemory frees them now.
	SetMemoryLimit(ctx context.Context, in *SetMemoryLimitRequest, opts ...grpc.CallOption) (*SetMemoryLimitResponse, error)
	// Weighs a tenant's memory against the others' (TASK-358). The daemon's memory is a page cache:
	// as much of the host's as is free, less a headroom; as the host's free memory falls, the daemon
	// gives it back, evicting each tenant first down to its share by weight of what it may still
	// hold, a tenant within its share kept, then any, least recently used first. Every tenant weighs
	// 1 until set, such as to the memory its sandboxes are given.
	SetMemoryWeight(ctx context.Context, in *SetMemoryWeightRequest, opts ...grpc.CallOption) (*SetMemoryWeightResponse, error)
	// Sets the bytes in flight a tenant's VM processes' connections share (TASK-149.35): what of
	// their requests the daemon holds at once, their data in the stream and what it queued for them,
	// which each connection's own credit bounds too. A request past it waits until enough leaves. By
	// default 8 GiB a tenant, which a tenant's VMs reach only writing large extents through the
	// stream, back to back.
	SetTenantInFlight(ctx context.Context, in *SetTenantInFlightRequest, opts ...grpc.CallOption) (*SetTenantInFlightResponse, error)
	// Evicts clean pages, of extents asked for longest ago first, until the memory a tenant, or the
	// daemon, holds is at most a target, or none is left to evict: as the host's memory runs low.
	ShrinkMemory(ctx context.Context, in *ShrinkMemoryRequest, opts ...grpc.CallOption) (*ShrinkMemoryResponse, error)
	// Limits the host's disk a tenant takes, or every tenant together (TASK-94): what is in its
	// directory, its cells' files, each file's length rounded up to 4 KiB, once however many names
	// it has, counted as written (doc-22), the host's part cache no tenant's. A write is admitted
	// only if what is taken, what writes admitted hold, and what the cells' own work may still add
	// for what was written (their commits copying, logging and framing it, which nothing refuses)
	// stay within it. Past a tenant's limit, its VMs' writes, and the cells they make, fail with
	// ENOSPC, which the VM's guest sees as a full disk, and the daemon and other tenants' writes go
	// on; past the total, any tenant's do. Commits go on, and what leaves the disk (uploaded, freed,
	// a cell deleted) lets the tenant write again. A limit under what is taken holds as data leaves.
	SetDiskLimit(ctx context.Context, in *SetDiskLimitRequest, opts ...grpc.CallOption) (*SetDiskLimitResponse, error)
	// Weighs a tenant's disk against the others' (doc-22): as a disk the daemon keeps cells on
	// comes near what the disk governor allows them, each tenant's writes are refused past its share
	// by weight of what is left, a tenant within its share writing on. Every tenant weighs 1 until
	// set.
	SetDiskWeight(ctx context.Context, in *SetDiskWeightRequest, opts ...grpc.CallOption) (*SetDiskWeightResponse, error)
	// What the host's disk a tenant takes, or every tenant together, and its limit.
	GetTenantDisk(ctx context.Context, in *GetTenantDiskRequest, opts ...grpc.CallOption) (*GetTenantDiskResponse, error)
	// Has the hosts sharing the daemon's bucket be these (TASK-406.2): its cells' fetchers ask the
	// host that owns a part, by rendezvous hashing of the object's key and the part's number over
	// them, for it before the bucket, and it serves them the parts it owns. Those it had are kept,
	// down or not. FAILED_PRECONDITION (ENOTSUP) if the daemon serves no peers (it was started
	// without CLADEFSD_PEER_ADDRESS); INVALID_ARGUMENT (EINVAL) for an address that is no
	// HOST:PORT.
	SetPeers(ctx context.Context, in *SetPeersRequest, opts ...grpc.CallOption) (*SetPeersResponse, error)
	// Shreds a cell (TASK-411.7, doc-16 Shredding): takes every wrapped KEK out of its object in the
	// daemon's bucket, by a conditional update, so that its keys unwrap no more and no host
	// takes it again; then forgets what this host holds of its keys, as ForgetTenantKeys does for a
	// tenant. Call it on every host that may hold the cell or serve it: the first call
	// shreds it, the others find it shredded and forget. Only as strong as the bucket keeps no older
	// version of the cell's object: with versioning on, a lifecycle rule must expire non-current
	// versions. A cell with no object, NOT_FOUND (ENOENT).
	ShredCell(ctx context.Context, in *ShredCellRequest, opts ...grpc.CallOption) (*ShredCellResponse, error)
	// Has the daemon forget what it holds of a tenant's keys (TASK-411.7, doc-16 Shredding), once
	// the tenant's KEK is destroyed (the tenant shredded), or revoked, in the KMS: its cells open
	// here are halted, each request of their VM processes failing with EACCES, as their openings do;
	// their extents' pages leave the cache, but those other cells hold too (a shared version's fork,
	// a public base); its fetchers forget their keys and what they kept of them; its KMS forgets the
	// tenant's cell KEKs it unwrapped and its pool of new ones, refusing them until the KMS answers
	// for the tenant again. Call it on every host. Halted cells keep their own keys in memory until
	// their VM processes close them.
	ForgetTenantKeys(ctx context.Context, in *ForgetTenantKeysRequest, opts ...grpc.CallOption) (*ForgetTenantKeysResponse, error)
	// Shreds a tenant under the bucket KMS (TASK-411.27, doc-16 The bucket KMS): replaces its KEK
	// object in the daemon's bucket (tenants/<tenant>/kek) by a tombstone, so that no key its KEK
	// wrapped unwraps on any host and none is made it again; then forgets what this host holds of its
	// keys, as ForgetTenantKeys does. Call it on every host: the first call shreds it, the others
	// find it shredded and forget; hosts not told refuse the tenant once they read its KEK object
	// again, within a minute. Only as strong as the bucket keeps no older version of the KEK object:
	// with versioning on, a lifecycle rule must expire non-current versions. A daemon whose KMS is
	// not the bucket KMS (CLADEFS_KMS names one) shreds nothing: FAILED_PRECONDITION (ENOTSUP); destroy
	// the tenant's KEK in that KMS, then ForgetTenantKeys.
	ShredTenant(ctx context.Context, in *ShredTenantRequest, opts ...grpc.CallOption) (*ShredTenantResponse, error)
	// Reloads what of the daemon's KMS configuration reloads live (board 8487, doc-16 What
	// reloads): the key overrides file of each KMS named by a URL (its keys=FILE), and the master
	// keys of each bucket KMS (CLADEFS_MASTER_KEY_FILE's, or CLADEFSD_BUCKET_<N>_'s for bucket N's),
	// each read again as at the start, so that a tenant onboarded with a key of its own (BYOK), or a
	// master key added, takes effect without a restart. Applied whole or not at all, a refusal
	// changing nothing: a file not to be read, or holding a line that is not one, INVALID_ARGUMENT
	// (EINVAL), naming the file and the line by its number; a change that would lose what is in use,
	// FAILED_PRECONDITION (EBUSY): a tenant's key renamed (an override added, changed or removed)
	// while it has cells in a bucket under that KMS, which would open no more, or a master key
	// removed while the registry of buckets records it as its bucket's or a tenant's KEK object is
	// under it; a bucket to check out of reach, INTERNAL (EIO); a daemon with no bucket of its own
	// (CLADEFSD_BUCKET) to check against, FAILED_PRECONDITION (ENOTSUP). Each KMS's URL, credentials,
	// and which KMS each bucket is under, stay the daemon's start's. Call it on every host.
	ReloadKms(ctx context.Context, in *ReloadKmsRequest, opts ...grpc.CallOption) (*ReloadKmsResponse, error)
	// Decommissions a host gone for good, of the incarnation given (backlog doc-25): its tombstone
	// written in every bucket the registry records, so that no host of it refers to a version from
	// then on (a host of it refuses to fork, or to start, once it reads it); then the references
	// of its ephemeral cells, which no host holds any more, retired, `most` of them at most, in
	// whichever bucket each is (boards 8517, 8519, 8523). Returns how many it retired, and how many
	// it found and left: a call cut off by its deadline, or that left some, keeps what it retired,
	// and a call again continues, retiring what is left; what it retires it retires in batches,
	// references first, then their index entries. For an incarnation no host has: a host of it
	// still holding its cells would read what is collected. The daemon's own incarnation, or one no
	// host makes (an even number, 0 among them), is refused (INVALID_ARGUMENT, EINVAL); a bucket
	// out of reach fails it (INTERNAL, EIO), never answering that it retired none. Incarnations
	// are written in base 32 everywhere they are printed (cladefsctl, the daemon's log).
	DecommissionHost(ctx context.Context, in *DecommissionHostRequest, opts ...grpc.CallOption) (*DecommissionHostResponse, error)
	// Lists every incarnation of a host whose ephemeral cells have references in the buckets the
	// registry records, or that was decommissioned there, with its count of references (board
	// 8521): for an operator to find a lost host's incarnation without its disk or its log.
	ListEphemeralHosts(ctx context.Context, in *ListEphemeralHostsRequest, opts ...grpc.CallOption) (*ListEphemeralHostsResponse, error)
	// Begins the live migration of an ephemeral cell to the daemon whose peer address is `peer`
	// (backlog doc-25, TASK-698.4): that daemon refers to the cell's origin first, then takes its
	// kept segments as they come while its guest runs. Returns the migration's ID.
	// HandOverEphemeralCell ends it, or AbortEphemeralMigration. EBUSY while another migration of
	// the cell is under way. Serves ephemeral cells only; bucket-backed cells are refused
	// (FAILED_PRECONDITION, ENOTSUP) and move through the bucket instead (SyncCell on source,
	// CreateCell + PlaceCell on target, PUT /snapshot/load, then DropCell on source).
	MigrateEphemeralCell(ctx context.Context, in *MigrateEphemeralCellRequest, opts ...grpc.CallOption) (*MigrateEphemeralCellResponse, error)
	// Hands an ephemeral cell over to the daemon its migration names, once nothing has it open
	// (EBUSY otherwise: its guest paused, its VM process gone or cell released). Accepted, the cell
	// directory here goes and its reference is retired; refused, it serves the cell again (EAGAIN).
	// Asked again after a failure or a restart, it asks the other again, which answers as it did.
	// Serves ephemeral cells only; bucket-backed cells are refused (FAILED_PRECONDITION, ENOTSUP) and
	// move through the bucket instead.
	HandOverEphemeralCell(ctx context.Context, in *HandOverEphemeralCellRequest, opts ...grpc.CallOption) (*HandOverEphemeralCellResponse, error)
	// Ends the live migration of an ephemeral cell (board 8577). Serves ephemeral cells only;
	// bucket-backed cells are refused (FAILED_PRECONDITION, ENOTSUP).
	AbortEphemeralMigration(ctx context.Context, in *AbortEphemeralMigrationRequest, opts ...grpc.CallOption) (*AbortEphemeralMigrationResponse, error)
	// Lists all managed host disks and their live per-disk governance state (caps, headroom,
	// allowance, usage, spare, pressure, health, placement; hotbed question 547).
	ListDisks(ctx context.Context, in *ListDisksRequest, opts ...grpc.CallOption) (*ListDisksResponse, error)
	// Updates per-disk governance (hard cap, part-cache cap, headroom, dirty budget, placement mode)
	// live at runtime for one managed disk identified by its configured `id` (hotbed question 547).
	SetDiskGovernance(ctx context.Context, in *SetDiskGovernanceRequest, opts ...grpc.CallOption) (*SetDiskGovernanceResponse, error)
	// Waits on the target daemon until an incoming ephemeral live migration hand-over for
	// `(tenant, cell)` is accepted and ready to open, or fails (refused, aborted, idle-reaped, or
	// superseded). Returns at once if `(tenant, cell)` is already present locally as an ephemeral
	// cell with no migration for it under way, or if the named migration has already reached a
	// terminal outcome. Respects caller cancellation and deadlines, and returns UNAVAILABLE if the
	// daemon begins draining. Serves ephemeral cells only; bucket-backed cells are refused
	// (FAILED_PRECONDITION, ENOTSUP).
	WaitEphemeralHandOver(ctx context.Context, in *WaitEphemeralHandOverRequest, opts ...grpc.CallOption) (*WaitEphemeralHandOverResponse, error)
}

type cladefsdClient struct {
	cc grpc.ClientConnInterface
}

func NewCladefsdClient(cc grpc.ClientConnInterface) CladefsdClient {
	return &cladefsdClient{cc}
}

func (c *cladefsdClient) CreateCell(ctx context.Context, in *CreateCellRequest, opts ...grpc.CallOption) (*CreateCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(CreateCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_CreateCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ImportImage(ctx context.Context, in *ImportImageRequest, opts ...grpc.CallOption) (*ImportImageResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ImportImageResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ImportImage_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ImportMemory(ctx context.Context, in *ImportMemoryRequest, opts ...grpc.CallOption) (*ImportMemoryResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ImportMemoryResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ImportMemory_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ForkCell(ctx context.Context, in *ForkCellRequest, opts ...grpc.CallOption) (*ForkCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ForkCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ForkCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) DeleteCell(ctx context.Context, in *DeleteCellRequest, opts ...grpc.CallOption) (*DeleteCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(DeleteCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_DeleteCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) DropCell(ctx context.Context, in *DropCellRequest, opts ...grpc.CallOption) (*DropCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(DropCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_DropCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ResizeCell(ctx context.Context, in *ResizeCellRequest, opts ...grpc.CallOption) (*ResizeCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ResizeCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ResizeCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) PlaceCell(ctx context.Context, in *PlaceCellRequest, opts ...grpc.CallOption) (*PlaceCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(PlaceCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_PlaceCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) MakePublic(ctx context.Context, in *MakePublicRequest, opts ...grpc.CallOption) (*MakePublicResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(MakePublicResponse)
	err := c.cc.Invoke(ctx, Cladefsd_MakePublic_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) MakeVersion(ctx context.Context, in *MakeVersionRequest, opts ...grpc.CallOption) (*MakeVersionResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(MakeVersionResponse)
	err := c.cc.Invoke(ctx, Cladefsd_MakeVersion_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ListVersions(ctx context.Context, in *ListVersionsRequest, opts ...grpc.CallOption) (*ListVersionsResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ListVersionsResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ListVersions_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) DropVersions(ctx context.Context, in *DropVersionsRequest, opts ...grpc.CallOption) (*DropVersionsResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(DropVersionsResponse)
	err := c.cc.Invoke(ctx, Cladefsd_DropVersions_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ShareVersion(ctx context.Context, in *ShareVersionRequest, opts ...grpc.CallOption) (*ShareVersionResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ShareVersionResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ShareVersion_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) AttachBase(ctx context.Context, in *AttachBaseRequest, opts ...grpc.CallOption) (*AttachBaseResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(AttachBaseResponse)
	err := c.cc.Invoke(ctx, Cladefsd_AttachBase_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SyncCell(ctx context.Context, in *SyncCellRequest, opts ...grpc.CallOption) (*SyncCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SyncCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SyncCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) StatCell(ctx context.Context, in *StatCellRequest, opts ...grpc.CallOption) (*StatCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(StatCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_StatCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ListCells(ctx context.Context, in *ListCellsRequest, opts ...grpc.CallOption) (*ListCellsResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ListCellsResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ListCells_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) GetCounters(ctx context.Context, in *GetCountersRequest, opts ...grpc.CallOption) (*GetCountersResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(GetCountersResponse)
	err := c.cc.Invoke(ctx, Cladefsd_GetCounters_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) GetStats(ctx context.Context, in *GetStatsRequest, opts ...grpc.CallOption) (*GetStatsResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(GetStatsResponse)
	err := c.cc.Invoke(ctx, Cladefsd_GetStats_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) Grant(ctx context.Context, in *GrantRequest, opts ...grpc.CallOption) (*GrantResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(GrantResponse)
	err := c.cc.Invoke(ctx, Cladefsd_Grant_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) Revoke(ctx context.Context, in *RevokeRequest, opts ...grpc.CallOption) (*RevokeResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(RevokeResponse)
	err := c.cc.Invoke(ctx, Cladefsd_Revoke_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SetMemoryLimit(ctx context.Context, in *SetMemoryLimitRequest, opts ...grpc.CallOption) (*SetMemoryLimitResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SetMemoryLimitResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SetMemoryLimit_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SetMemoryWeight(ctx context.Context, in *SetMemoryWeightRequest, opts ...grpc.CallOption) (*SetMemoryWeightResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SetMemoryWeightResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SetMemoryWeight_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SetTenantInFlight(ctx context.Context, in *SetTenantInFlightRequest, opts ...grpc.CallOption) (*SetTenantInFlightResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SetTenantInFlightResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SetTenantInFlight_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ShrinkMemory(ctx context.Context, in *ShrinkMemoryRequest, opts ...grpc.CallOption) (*ShrinkMemoryResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ShrinkMemoryResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ShrinkMemory_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SetDiskLimit(ctx context.Context, in *SetDiskLimitRequest, opts ...grpc.CallOption) (*SetDiskLimitResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SetDiskLimitResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SetDiskLimit_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SetDiskWeight(ctx context.Context, in *SetDiskWeightRequest, opts ...grpc.CallOption) (*SetDiskWeightResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SetDiskWeightResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SetDiskWeight_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) GetTenantDisk(ctx context.Context, in *GetTenantDiskRequest, opts ...grpc.CallOption) (*GetTenantDiskResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(GetTenantDiskResponse)
	err := c.cc.Invoke(ctx, Cladefsd_GetTenantDisk_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SetPeers(ctx context.Context, in *SetPeersRequest, opts ...grpc.CallOption) (*SetPeersResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SetPeersResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SetPeers_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ShredCell(ctx context.Context, in *ShredCellRequest, opts ...grpc.CallOption) (*ShredCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ShredCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ShredCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ForgetTenantKeys(ctx context.Context, in *ForgetTenantKeysRequest, opts ...grpc.CallOption) (*ForgetTenantKeysResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ForgetTenantKeysResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ForgetTenantKeys_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ShredTenant(ctx context.Context, in *ShredTenantRequest, opts ...grpc.CallOption) (*ShredTenantResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ShredTenantResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ShredTenant_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ReloadKms(ctx context.Context, in *ReloadKmsRequest, opts ...grpc.CallOption) (*ReloadKmsResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ReloadKmsResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ReloadKms_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) DecommissionHost(ctx context.Context, in *DecommissionHostRequest, opts ...grpc.CallOption) (*DecommissionHostResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(DecommissionHostResponse)
	err := c.cc.Invoke(ctx, Cladefsd_DecommissionHost_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ListEphemeralHosts(ctx context.Context, in *ListEphemeralHostsRequest, opts ...grpc.CallOption) (*ListEphemeralHostsResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ListEphemeralHostsResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ListEphemeralHosts_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) MigrateEphemeralCell(ctx context.Context, in *MigrateEphemeralCellRequest, opts ...grpc.CallOption) (*MigrateEphemeralCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(MigrateEphemeralCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_MigrateEphemeralCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) HandOverEphemeralCell(ctx context.Context, in *HandOverEphemeralCellRequest, opts ...grpc.CallOption) (*HandOverEphemeralCellResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(HandOverEphemeralCellResponse)
	err := c.cc.Invoke(ctx, Cladefsd_HandOverEphemeralCell_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) AbortEphemeralMigration(ctx context.Context, in *AbortEphemeralMigrationRequest, opts ...grpc.CallOption) (*AbortEphemeralMigrationResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(AbortEphemeralMigrationResponse)
	err := c.cc.Invoke(ctx, Cladefsd_AbortEphemeralMigration_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) ListDisks(ctx context.Context, in *ListDisksRequest, opts ...grpc.CallOption) (*ListDisksResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(ListDisksResponse)
	err := c.cc.Invoke(ctx, Cladefsd_ListDisks_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) SetDiskGovernance(ctx context.Context, in *SetDiskGovernanceRequest, opts ...grpc.CallOption) (*SetDiskGovernanceResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(SetDiskGovernanceResponse)
	err := c.cc.Invoke(ctx, Cladefsd_SetDiskGovernance_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

func (c *cladefsdClient) WaitEphemeralHandOver(ctx context.Context, in *WaitEphemeralHandOverRequest, opts ...grpc.CallOption) (*WaitEphemeralHandOverResponse, error) {
	cOpts := append([]grpc.CallOption{grpc.StaticMethod()}, opts...)
	out := new(WaitEphemeralHandOverResponse)
	err := c.cc.Invoke(ctx, Cladefsd_WaitEphemeralHandOver_FullMethodName, in, out, cOpts...)
	if err != nil {
		return nil, err
	}
	return out, nil
}

// CladefsdServer is the server API for Cladefsd service.
// All implementations must embed UnimplementedCladefsdServer
// for forward compatibility.
//
// The extent daemon's control API.
type CladefsdServer interface {
	// Makes an empty cell: a new one; or one kept in the daemon's bucket, which the daemon
	// takes once it opens it (a VM on another host than the cell's). RESOURCE_EXHAUSTED (ENOMEM)
	// while the host's memory is full: the VMs here need what is left (board 5662).
	CreateCell(context.Context, *CreateCellRequest) (*CreateCellResponse, error)
	// Makes a cell holding an Fxfs filesystem of a container image: a base for VMs to fork.
	ImportImage(context.Context, *ImportImageRequest) (*ImportImageResponse, error)
	// Makes a memory cell holding a VM snapshot's memory file (board 11222): its pages, those of
	// zeros discarded as holes, committed at once, and optionally a version made of it.
	ImportMemory(context.Context, *ImportMemoryRequest) (*ImportMemoryResponse, error)
	// Forks a cell to a new cell sharing its data: live, if a VM has it open, at a
	// moment nothing is open on it (as with its guest's filesystem frozen), FAILED_PRECONDITION
	// (EBUSY) otherwise. A fork of a cell with a writer is a commit of it, made where that
	// writer is, one host at a time; a fork of a version (`version`) is its child's record alone,
	// made on any number of hosts at once. RESOURCE_EXHAUSTED (ENOMEM) while the host's memory is
	// full, as CreateCell.
	ForkCell(context.Context, *ForkCellRequest) (*ForkCellResponse, error)
	// Deletes a cell, or a fork, and its branch in the daemon's bucket: its data goes but for what
	// other cells share of it, which stays until the last of them goes. What was forked from it
	// lives on.
	DeleteCell(context.Context, *DeleteCellRequest) (*DeleteCellResponse, error)
	// Deletes this host's copy of a cell, or a fork, as DeleteCell does, but for its branch in the
	// daemon's bucket, which stays, with what else holds it. A cell whose
	// forks live here keeps its directory, which no longer opens, and takes no CreateCell until the
	// last of them goes. What its commits hold that the bucket doesn't goes with it: SyncCell first,
	// to keep it. FAILED_PRECONDITION (EBUSY) while a VM process has it open.
	DropCell(context.Context, *DropCellRequest) (*DropCellResponse, error)
	// Sets a cell's capacity, the size of the disk its guest sees: a cell a VM has open grows at
	// once, the guest seeing it mounted, and shrinks if its guest's filesystem agrees, no lower than
	// what it holds (RESOURCE_EXHAUSTED otherwise); one none has open only grows.
	ResizeCell(context.Context, *ResizeCellRequest) (*ResizeCellResponse, error)
	// Places a cell at a placement whose fencing token, its boot ID, is `token`: its writer takes
	// its branch in object storage with that token from then on, which a later placement's exceeds,
	// so that a placement cut off takes no branch back from the one after it.
	PlaceCell(context.Context, *PlaceCellRequest) (*PlaceCellResponse, error)
	// Makes a cell a public base, and the branches it inherited from: their pages are cached once,
	// in memory every tenant's VM processes map read-only, and shared by every tenant's forks.
	MakePublic(context.Context, *MakePublicRequest) (*MakePublicResponse, error)
	// Makes a version of a cell: a name for its state as of its last commit, with nothing
	// open on it (FAILED_PRECONDITION, EBUSY, otherwise), as with its guest's filesystem frozen. It
	// is one commit on this host, durable once this returns, and asks nothing of the daemon's
	// bucket: the cell's head names it once its commit is uploaded, behind. Forks are made of a
	// version on any host (ForkCell's `version`, a VM snapshot's restores).
	MakeVersion(context.Context, *MakeVersionRequest) (*MakeVersionResponse, error)
	// Lists the versions of a cell: those its head names, then those made here not yet
	// uploaded.
	ListVersions(context.Context, *ListVersionsRequest) (*ListVersionsResponse, error)
	// Drops versions of a cell. What forks of one inherited stays until they go.
	DropVersions(context.Context, *DropVersionsRequest) (*DropVersionsResponse, error)
	// Shares an uploaded version, or no longer: while shared, it forks into cells of other tenants'
	// than its own, as a base image many tenants fork; unshared, such forks fail
	// (PERMISSION_DENIED, EACCES), and those made live on.
	ShareVersion(context.Context, *ShareVersionRequest) (*ShareVersionResponse, error)
	// Attaches a fork of a cell to a cell, a VM's or stopped, and so to its forks and the VMs
	// restored from them: reads and maps of the fork's extents the cell doesn't hold read through
	// to it, read-only. Everything the fork holds is readable to the guest, which names extents, so
	// that a cell that isn't public attaches only to a cell of its tenant (PERMISSION_DENIED
	// otherwise). The fork stays until the caller deletes it: a VM with it open keeps it from going
	// (FAILED_PRECONDITION), but a stopped cell doesn't. No guest can ask for this.
	AttachBase(context.Context, *AttachBaseRequest) (*AttachBaseResponse, error)
	// Returns once every commit of a cell, as of the call, is uploaded: in the daemon's bucket,
	// as before a clean stop.
	SyncCell(context.Context, *SyncCellRequest) (*SyncCellResponse, error)
	// What a cell is, and holds. Of a cell a VM process has open, as it has it, at no cost however
	// many extents it holds; of one none has, the daemon opens it to say, as a VM's would, taking its
	// branch's head, and closes it again: time, and memory till then, as its extents (TASK-454).
	StatCell(context.Context, *StatCellRequest) (*StatCellResponse, error)
	// Lists the cells the daemon serves: those VM processes write, and those open for others, such
	// as bases attached, with what StatCell says of each (TASK-50.5).
	ListCells(context.Context, *ListCellsRequest) (*ListCellsResponse, error)
	// What the daemon did since it started: counters, which only grow.
	GetCounters(context.Context, *GetCountersRequest) (*GetCountersResponse, error)
	// What the daemon, its VM processes and their guests counted, as of one moment, each with the
	// commit it was built of (TASK-111.4): the daemon asks every VM process that opened a cell, each
	// its guest, waits for their answers, a while at most, and reads its own counters then. Each VM
	// process is some 2 KiB of the answer with today's guests (some 50 counters and 12 histograms),
	// some 40 KiB with a guest saying near the most it may, 64 KiB (tests/control.rs measures both,
	// 1,960 and 40,568 bytes): 4 MiB, gRPC's default, is some hundreds of VMs at the most. Past
	// the most an answer is (64 MiB, above), RESOURCE_EXHAUSTED (EMSGSIZE), and the request's
	// `leave_out_*` choose what to leave out.
	GetStats(context.Context, *GetStatsRequest) (*GetStatsResponse, error)
	// Grants a tenant, by its ID, a VM socket, making the tenant if it is new: the path of a socket
	// for the caller to bind-mount into the tenant's jails, through which VM processes open cells in
	// the tenant's directory alone. Granting a tenant again returns its socket, given to the uid and
	// gid named; with another directory, ALREADY_EXISTS.
	Grant(context.Context, *GrantRequest) (*GrantResponse, error)
	// Revokes a tenant's grant: its socket goes, and takes no more connections. Its VM processes
	// connected keep their cells until they go; then the tenant goes, and its cached pages.
	Revoke(context.Context, *RevokeRequest) (*RevokeResponse, error)
	// Limits the memory the daemon holds extents' pages in, at once (TASK-50.3): a tenant's, or
	// every tenant's together. Past a tenant's limit, the daemon evicts the clean pages of its
	// extents asked for longest ago; past the total, any tenant's; with none left to evict, what
	// would take more fails, with ENOMEM, which the VM's guest sees as an I/O error, and the daemon
	// goes on. A limit under what is held holds as pages are freed: ShrinkMemory frees them now.
	SetMemoryLimit(context.Context, *SetMemoryLimitRequest) (*SetMemoryLimitResponse, error)
	// Weighs a tenant's memory against the others' (TASK-358). The daemon's memory is a page cache:
	// as much of the host's as is free, less a headroom; as the host's free memory falls, the daemon
	// gives it back, evicting each tenant first down to its share by weight of what it may still
	// hold, a tenant within its share kept, then any, least recently used first. Every tenant weighs
	// 1 until set, such as to the memory its sandboxes are given.
	SetMemoryWeight(context.Context, *SetMemoryWeightRequest) (*SetMemoryWeightResponse, error)
	// Sets the bytes in flight a tenant's VM processes' connections share (TASK-149.35): what of
	// their requests the daemon holds at once, their data in the stream and what it queued for them,
	// which each connection's own credit bounds too. A request past it waits until enough leaves. By
	// default 8 GiB a tenant, which a tenant's VMs reach only writing large extents through the
	// stream, back to back.
	SetTenantInFlight(context.Context, *SetTenantInFlightRequest) (*SetTenantInFlightResponse, error)
	// Evicts clean pages, of extents asked for longest ago first, until the memory a tenant, or the
	// daemon, holds is at most a target, or none is left to evict: as the host's memory runs low.
	ShrinkMemory(context.Context, *ShrinkMemoryRequest) (*ShrinkMemoryResponse, error)
	// Limits the host's disk a tenant takes, or every tenant together (TASK-94): what is in its
	// directory, its cells' files, each file's length rounded up to 4 KiB, once however many names
	// it has, counted as written (doc-22), the host's part cache no tenant's. A write is admitted
	// only if what is taken, what writes admitted hold, and what the cells' own work may still add
	// for what was written (their commits copying, logging and framing it, which nothing refuses)
	// stay within it. Past a tenant's limit, its VMs' writes, and the cells they make, fail with
	// ENOSPC, which the VM's guest sees as a full disk, and the daemon and other tenants' writes go
	// on; past the total, any tenant's do. Commits go on, and what leaves the disk (uploaded, freed,
	// a cell deleted) lets the tenant write again. A limit under what is taken holds as data leaves.
	SetDiskLimit(context.Context, *SetDiskLimitRequest) (*SetDiskLimitResponse, error)
	// Weighs a tenant's disk against the others' (doc-22): as a disk the daemon keeps cells on
	// comes near what the disk governor allows them, each tenant's writes are refused past its share
	// by weight of what is left, a tenant within its share writing on. Every tenant weighs 1 until
	// set.
	SetDiskWeight(context.Context, *SetDiskWeightRequest) (*SetDiskWeightResponse, error)
	// What the host's disk a tenant takes, or every tenant together, and its limit.
	GetTenantDisk(context.Context, *GetTenantDiskRequest) (*GetTenantDiskResponse, error)
	// Has the hosts sharing the daemon's bucket be these (TASK-406.2): its cells' fetchers ask the
	// host that owns a part, by rendezvous hashing of the object's key and the part's number over
	// them, for it before the bucket, and it serves them the parts it owns. Those it had are kept,
	// down or not. FAILED_PRECONDITION (ENOTSUP) if the daemon serves no peers (it was started
	// without CLADEFSD_PEER_ADDRESS); INVALID_ARGUMENT (EINVAL) for an address that is no
	// HOST:PORT.
	SetPeers(context.Context, *SetPeersRequest) (*SetPeersResponse, error)
	// Shreds a cell (TASK-411.7, doc-16 Shredding): takes every wrapped KEK out of its object in the
	// daemon's bucket, by a conditional update, so that its keys unwrap no more and no host
	// takes it again; then forgets what this host holds of its keys, as ForgetTenantKeys does for a
	// tenant. Call it on every host that may hold the cell or serve it: the first call
	// shreds it, the others find it shredded and forget. Only as strong as the bucket keeps no older
	// version of the cell's object: with versioning on, a lifecycle rule must expire non-current
	// versions. A cell with no object, NOT_FOUND (ENOENT).
	ShredCell(context.Context, *ShredCellRequest) (*ShredCellResponse, error)
	// Has the daemon forget what it holds of a tenant's keys (TASK-411.7, doc-16 Shredding), once
	// the tenant's KEK is destroyed (the tenant shredded), or revoked, in the KMS: its cells open
	// here are halted, each request of their VM processes failing with EACCES, as their openings do;
	// their extents' pages leave the cache, but those other cells hold too (a shared version's fork,
	// a public base); its fetchers forget their keys and what they kept of them; its KMS forgets the
	// tenant's cell KEKs it unwrapped and its pool of new ones, refusing them until the KMS answers
	// for the tenant again. Call it on every host. Halted cells keep their own keys in memory until
	// their VM processes close them.
	ForgetTenantKeys(context.Context, *ForgetTenantKeysRequest) (*ForgetTenantKeysResponse, error)
	// Shreds a tenant under the bucket KMS (TASK-411.27, doc-16 The bucket KMS): replaces its KEK
	// object in the daemon's bucket (tenants/<tenant>/kek) by a tombstone, so that no key its KEK
	// wrapped unwraps on any host and none is made it again; then forgets what this host holds of its
	// keys, as ForgetTenantKeys does. Call it on every host: the first call shreds it, the others
	// find it shredded and forget; hosts not told refuse the tenant once they read its KEK object
	// again, within a minute. Only as strong as the bucket keeps no older version of the KEK object:
	// with versioning on, a lifecycle rule must expire non-current versions. A daemon whose KMS is
	// not the bucket KMS (CLADEFS_KMS names one) shreds nothing: FAILED_PRECONDITION (ENOTSUP); destroy
	// the tenant's KEK in that KMS, then ForgetTenantKeys.
	ShredTenant(context.Context, *ShredTenantRequest) (*ShredTenantResponse, error)
	// Reloads what of the daemon's KMS configuration reloads live (board 8487, doc-16 What
	// reloads): the key overrides file of each KMS named by a URL (its keys=FILE), and the master
	// keys of each bucket KMS (CLADEFS_MASTER_KEY_FILE's, or CLADEFSD_BUCKET_<N>_'s for bucket N's),
	// each read again as at the start, so that a tenant onboarded with a key of its own (BYOK), or a
	// master key added, takes effect without a restart. Applied whole or not at all, a refusal
	// changing nothing: a file not to be read, or holding a line that is not one, INVALID_ARGUMENT
	// (EINVAL), naming the file and the line by its number; a change that would lose what is in use,
	// FAILED_PRECONDITION (EBUSY): a tenant's key renamed (an override added, changed or removed)
	// while it has cells in a bucket under that KMS, which would open no more, or a master key
	// removed while the registry of buckets records it as its bucket's or a tenant's KEK object is
	// under it; a bucket to check out of reach, INTERNAL (EIO); a daemon with no bucket of its own
	// (CLADEFSD_BUCKET) to check against, FAILED_PRECONDITION (ENOTSUP). Each KMS's URL, credentials,
	// and which KMS each bucket is under, stay the daemon's start's. Call it on every host.
	ReloadKms(context.Context, *ReloadKmsRequest) (*ReloadKmsResponse, error)
	// Decommissions a host gone for good, of the incarnation given (backlog doc-25): its tombstone
	// written in every bucket the registry records, so that no host of it refers to a version from
	// then on (a host of it refuses to fork, or to start, once it reads it); then the references
	// of its ephemeral cells, which no host holds any more, retired, `most` of them at most, in
	// whichever bucket each is (boards 8517, 8519, 8523). Returns how many it retired, and how many
	// it found and left: a call cut off by its deadline, or that left some, keeps what it retired,
	// and a call again continues, retiring what is left; what it retires it retires in batches,
	// references first, then their index entries. For an incarnation no host has: a host of it
	// still holding its cells would read what is collected. The daemon's own incarnation, or one no
	// host makes (an even number, 0 among them), is refused (INVALID_ARGUMENT, EINVAL); a bucket
	// out of reach fails it (INTERNAL, EIO), never answering that it retired none. Incarnations
	// are written in base 32 everywhere they are printed (cladefsctl, the daemon's log).
	DecommissionHost(context.Context, *DecommissionHostRequest) (*DecommissionHostResponse, error)
	// Lists every incarnation of a host whose ephemeral cells have references in the buckets the
	// registry records, or that was decommissioned there, with its count of references (board
	// 8521): for an operator to find a lost host's incarnation without its disk or its log.
	ListEphemeralHosts(context.Context, *ListEphemeralHostsRequest) (*ListEphemeralHostsResponse, error)
	// Begins the live migration of an ephemeral cell to the daemon whose peer address is `peer`
	// (backlog doc-25, TASK-698.4): that daemon refers to the cell's origin first, then takes its
	// kept segments as they come while its guest runs. Returns the migration's ID.
	// HandOverEphemeralCell ends it, or AbortEphemeralMigration. EBUSY while another migration of
	// the cell is under way. Serves ephemeral cells only; bucket-backed cells are refused
	// (FAILED_PRECONDITION, ENOTSUP) and move through the bucket instead (SyncCell on source,
	// CreateCell + PlaceCell on target, PUT /snapshot/load, then DropCell on source).
	MigrateEphemeralCell(context.Context, *MigrateEphemeralCellRequest) (*MigrateEphemeralCellResponse, error)
	// Hands an ephemeral cell over to the daemon its migration names, once nothing has it open
	// (EBUSY otherwise: its guest paused, its VM process gone or cell released). Accepted, the cell
	// directory here goes and its reference is retired; refused, it serves the cell again (EAGAIN).
	// Asked again after a failure or a restart, it asks the other again, which answers as it did.
	// Serves ephemeral cells only; bucket-backed cells are refused (FAILED_PRECONDITION, ENOTSUP) and
	// move through the bucket instead.
	HandOverEphemeralCell(context.Context, *HandOverEphemeralCellRequest) (*HandOverEphemeralCellResponse, error)
	// Ends the live migration of an ephemeral cell (board 8577). Serves ephemeral cells only;
	// bucket-backed cells are refused (FAILED_PRECONDITION, ENOTSUP).
	AbortEphemeralMigration(context.Context, *AbortEphemeralMigrationRequest) (*AbortEphemeralMigrationResponse, error)
	// Lists all managed host disks and their live per-disk governance state (caps, headroom,
	// allowance, usage, spare, pressure, health, placement; hotbed question 547).
	ListDisks(context.Context, *ListDisksRequest) (*ListDisksResponse, error)
	// Updates per-disk governance (hard cap, part-cache cap, headroom, dirty budget, placement mode)
	// live at runtime for one managed disk identified by its configured `id` (hotbed question 547).
	SetDiskGovernance(context.Context, *SetDiskGovernanceRequest) (*SetDiskGovernanceResponse, error)
	// Waits on the target daemon until an incoming ephemeral live migration hand-over for
	// `(tenant, cell)` is accepted and ready to open, or fails (refused, aborted, idle-reaped, or
	// superseded). Returns at once if `(tenant, cell)` is already present locally as an ephemeral
	// cell with no migration for it under way, or if the named migration has already reached a
	// terminal outcome. Respects caller cancellation and deadlines, and returns UNAVAILABLE if the
	// daemon begins draining. Serves ephemeral cells only; bucket-backed cells are refused
	// (FAILED_PRECONDITION, ENOTSUP).
	WaitEphemeralHandOver(context.Context, *WaitEphemeralHandOverRequest) (*WaitEphemeralHandOverResponse, error)
	mustEmbedUnimplementedCladefsdServer()
}

// UnimplementedCladefsdServer must be embedded to have
// forward compatible implementations.
//
// NOTE: this should be embedded by value instead of pointer to avoid a nil
// pointer dereference when methods are called.
type UnimplementedCladefsdServer struct{}

func (UnimplementedCladefsdServer) CreateCell(context.Context, *CreateCellRequest) (*CreateCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method CreateCell not implemented")
}
func (UnimplementedCladefsdServer) ImportImage(context.Context, *ImportImageRequest) (*ImportImageResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ImportImage not implemented")
}
func (UnimplementedCladefsdServer) ImportMemory(context.Context, *ImportMemoryRequest) (*ImportMemoryResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ImportMemory not implemented")
}
func (UnimplementedCladefsdServer) ForkCell(context.Context, *ForkCellRequest) (*ForkCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ForkCell not implemented")
}
func (UnimplementedCladefsdServer) DeleteCell(context.Context, *DeleteCellRequest) (*DeleteCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method DeleteCell not implemented")
}
func (UnimplementedCladefsdServer) DropCell(context.Context, *DropCellRequest) (*DropCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method DropCell not implemented")
}
func (UnimplementedCladefsdServer) ResizeCell(context.Context, *ResizeCellRequest) (*ResizeCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ResizeCell not implemented")
}
func (UnimplementedCladefsdServer) PlaceCell(context.Context, *PlaceCellRequest) (*PlaceCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method PlaceCell not implemented")
}
func (UnimplementedCladefsdServer) MakePublic(context.Context, *MakePublicRequest) (*MakePublicResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method MakePublic not implemented")
}
func (UnimplementedCladefsdServer) MakeVersion(context.Context, *MakeVersionRequest) (*MakeVersionResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method MakeVersion not implemented")
}
func (UnimplementedCladefsdServer) ListVersions(context.Context, *ListVersionsRequest) (*ListVersionsResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ListVersions not implemented")
}
func (UnimplementedCladefsdServer) DropVersions(context.Context, *DropVersionsRequest) (*DropVersionsResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method DropVersions not implemented")
}
func (UnimplementedCladefsdServer) ShareVersion(context.Context, *ShareVersionRequest) (*ShareVersionResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ShareVersion not implemented")
}
func (UnimplementedCladefsdServer) AttachBase(context.Context, *AttachBaseRequest) (*AttachBaseResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method AttachBase not implemented")
}
func (UnimplementedCladefsdServer) SyncCell(context.Context, *SyncCellRequest) (*SyncCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SyncCell not implemented")
}
func (UnimplementedCladefsdServer) StatCell(context.Context, *StatCellRequest) (*StatCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method StatCell not implemented")
}
func (UnimplementedCladefsdServer) ListCells(context.Context, *ListCellsRequest) (*ListCellsResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ListCells not implemented")
}
func (UnimplementedCladefsdServer) GetCounters(context.Context, *GetCountersRequest) (*GetCountersResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method GetCounters not implemented")
}
func (UnimplementedCladefsdServer) GetStats(context.Context, *GetStatsRequest) (*GetStatsResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method GetStats not implemented")
}
func (UnimplementedCladefsdServer) Grant(context.Context, *GrantRequest) (*GrantResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method Grant not implemented")
}
func (UnimplementedCladefsdServer) Revoke(context.Context, *RevokeRequest) (*RevokeResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method Revoke not implemented")
}
func (UnimplementedCladefsdServer) SetMemoryLimit(context.Context, *SetMemoryLimitRequest) (*SetMemoryLimitResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SetMemoryLimit not implemented")
}
func (UnimplementedCladefsdServer) SetMemoryWeight(context.Context, *SetMemoryWeightRequest) (*SetMemoryWeightResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SetMemoryWeight not implemented")
}
func (UnimplementedCladefsdServer) SetTenantInFlight(context.Context, *SetTenantInFlightRequest) (*SetTenantInFlightResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SetTenantInFlight not implemented")
}
func (UnimplementedCladefsdServer) ShrinkMemory(context.Context, *ShrinkMemoryRequest) (*ShrinkMemoryResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ShrinkMemory not implemented")
}
func (UnimplementedCladefsdServer) SetDiskLimit(context.Context, *SetDiskLimitRequest) (*SetDiskLimitResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SetDiskLimit not implemented")
}
func (UnimplementedCladefsdServer) SetDiskWeight(context.Context, *SetDiskWeightRequest) (*SetDiskWeightResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SetDiskWeight not implemented")
}
func (UnimplementedCladefsdServer) GetTenantDisk(context.Context, *GetTenantDiskRequest) (*GetTenantDiskResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method GetTenantDisk not implemented")
}
func (UnimplementedCladefsdServer) SetPeers(context.Context, *SetPeersRequest) (*SetPeersResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SetPeers not implemented")
}
func (UnimplementedCladefsdServer) ShredCell(context.Context, *ShredCellRequest) (*ShredCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ShredCell not implemented")
}
func (UnimplementedCladefsdServer) ForgetTenantKeys(context.Context, *ForgetTenantKeysRequest) (*ForgetTenantKeysResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ForgetTenantKeys not implemented")
}
func (UnimplementedCladefsdServer) ShredTenant(context.Context, *ShredTenantRequest) (*ShredTenantResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ShredTenant not implemented")
}
func (UnimplementedCladefsdServer) ReloadKms(context.Context, *ReloadKmsRequest) (*ReloadKmsResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ReloadKms not implemented")
}
func (UnimplementedCladefsdServer) DecommissionHost(context.Context, *DecommissionHostRequest) (*DecommissionHostResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method DecommissionHost not implemented")
}
func (UnimplementedCladefsdServer) ListEphemeralHosts(context.Context, *ListEphemeralHostsRequest) (*ListEphemeralHostsResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ListEphemeralHosts not implemented")
}
func (UnimplementedCladefsdServer) MigrateEphemeralCell(context.Context, *MigrateEphemeralCellRequest) (*MigrateEphemeralCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method MigrateEphemeralCell not implemented")
}
func (UnimplementedCladefsdServer) HandOverEphemeralCell(context.Context, *HandOverEphemeralCellRequest) (*HandOverEphemeralCellResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method HandOverEphemeralCell not implemented")
}
func (UnimplementedCladefsdServer) AbortEphemeralMigration(context.Context, *AbortEphemeralMigrationRequest) (*AbortEphemeralMigrationResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method AbortEphemeralMigration not implemented")
}
func (UnimplementedCladefsdServer) ListDisks(context.Context, *ListDisksRequest) (*ListDisksResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method ListDisks not implemented")
}
func (UnimplementedCladefsdServer) SetDiskGovernance(context.Context, *SetDiskGovernanceRequest) (*SetDiskGovernanceResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method SetDiskGovernance not implemented")
}
func (UnimplementedCladefsdServer) WaitEphemeralHandOver(context.Context, *WaitEphemeralHandOverRequest) (*WaitEphemeralHandOverResponse, error) {
	return nil, status.Error(codes.Unimplemented, "method WaitEphemeralHandOver not implemented")
}
func (UnimplementedCladefsdServer) mustEmbedUnimplementedCladefsdServer() {}
func (UnimplementedCladefsdServer) testEmbeddedByValue()                  {}

// UnsafeCladefsdServer may be embedded to opt out of forward compatibility for this service.
// Use of this interface is not recommended, as added methods to CladefsdServer will
// result in compilation errors.
type UnsafeCladefsdServer interface {
	mustEmbedUnimplementedCladefsdServer()
}

func RegisterCladefsdServer(s grpc.ServiceRegistrar, srv CladefsdServer) {
	// If the following call panics, it indicates UnimplementedCladefsdServer was
	// embedded by pointer and is nil.  This will cause panics if an
	// unimplemented method is ever invoked, so we test this at initialization
	// time to prevent it from happening at runtime later due to I/O.
	if t, ok := srv.(interface{ testEmbeddedByValue() }); ok {
		t.testEmbeddedByValue()
	}
	s.RegisterService(&Cladefsd_ServiceDesc, srv)
}

func _Cladefsd_CreateCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(CreateCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).CreateCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_CreateCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).CreateCell(ctx, req.(*CreateCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ImportImage_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ImportImageRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ImportImage(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ImportImage_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ImportImage(ctx, req.(*ImportImageRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ImportMemory_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ImportMemoryRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ImportMemory(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ImportMemory_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ImportMemory(ctx, req.(*ImportMemoryRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ForkCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ForkCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ForkCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ForkCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ForkCell(ctx, req.(*ForkCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_DeleteCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(DeleteCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).DeleteCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_DeleteCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).DeleteCell(ctx, req.(*DeleteCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_DropCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(DropCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).DropCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_DropCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).DropCell(ctx, req.(*DropCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ResizeCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ResizeCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ResizeCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ResizeCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ResizeCell(ctx, req.(*ResizeCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_PlaceCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(PlaceCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).PlaceCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_PlaceCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).PlaceCell(ctx, req.(*PlaceCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_MakePublic_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(MakePublicRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).MakePublic(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_MakePublic_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).MakePublic(ctx, req.(*MakePublicRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_MakeVersion_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(MakeVersionRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).MakeVersion(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_MakeVersion_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).MakeVersion(ctx, req.(*MakeVersionRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ListVersions_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ListVersionsRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ListVersions(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ListVersions_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ListVersions(ctx, req.(*ListVersionsRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_DropVersions_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(DropVersionsRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).DropVersions(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_DropVersions_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).DropVersions(ctx, req.(*DropVersionsRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ShareVersion_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ShareVersionRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ShareVersion(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ShareVersion_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ShareVersion(ctx, req.(*ShareVersionRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_AttachBase_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(AttachBaseRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).AttachBase(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_AttachBase_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).AttachBase(ctx, req.(*AttachBaseRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SyncCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SyncCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SyncCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SyncCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SyncCell(ctx, req.(*SyncCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_StatCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(StatCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).StatCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_StatCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).StatCell(ctx, req.(*StatCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ListCells_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ListCellsRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ListCells(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ListCells_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ListCells(ctx, req.(*ListCellsRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_GetCounters_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(GetCountersRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).GetCounters(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_GetCounters_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).GetCounters(ctx, req.(*GetCountersRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_GetStats_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(GetStatsRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).GetStats(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_GetStats_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).GetStats(ctx, req.(*GetStatsRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_Grant_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(GrantRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).Grant(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_Grant_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).Grant(ctx, req.(*GrantRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_Revoke_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(RevokeRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).Revoke(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_Revoke_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).Revoke(ctx, req.(*RevokeRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SetMemoryLimit_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SetMemoryLimitRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SetMemoryLimit(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SetMemoryLimit_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SetMemoryLimit(ctx, req.(*SetMemoryLimitRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SetMemoryWeight_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SetMemoryWeightRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SetMemoryWeight(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SetMemoryWeight_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SetMemoryWeight(ctx, req.(*SetMemoryWeightRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SetTenantInFlight_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SetTenantInFlightRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SetTenantInFlight(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SetTenantInFlight_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SetTenantInFlight(ctx, req.(*SetTenantInFlightRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ShrinkMemory_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ShrinkMemoryRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ShrinkMemory(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ShrinkMemory_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ShrinkMemory(ctx, req.(*ShrinkMemoryRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SetDiskLimit_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SetDiskLimitRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SetDiskLimit(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SetDiskLimit_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SetDiskLimit(ctx, req.(*SetDiskLimitRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SetDiskWeight_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SetDiskWeightRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SetDiskWeight(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SetDiskWeight_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SetDiskWeight(ctx, req.(*SetDiskWeightRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_GetTenantDisk_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(GetTenantDiskRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).GetTenantDisk(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_GetTenantDisk_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).GetTenantDisk(ctx, req.(*GetTenantDiskRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SetPeers_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SetPeersRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SetPeers(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SetPeers_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SetPeers(ctx, req.(*SetPeersRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ShredCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ShredCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ShredCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ShredCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ShredCell(ctx, req.(*ShredCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ForgetTenantKeys_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ForgetTenantKeysRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ForgetTenantKeys(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ForgetTenantKeys_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ForgetTenantKeys(ctx, req.(*ForgetTenantKeysRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ShredTenant_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ShredTenantRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ShredTenant(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ShredTenant_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ShredTenant(ctx, req.(*ShredTenantRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ReloadKms_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ReloadKmsRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ReloadKms(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ReloadKms_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ReloadKms(ctx, req.(*ReloadKmsRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_DecommissionHost_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(DecommissionHostRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).DecommissionHost(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_DecommissionHost_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).DecommissionHost(ctx, req.(*DecommissionHostRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ListEphemeralHosts_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ListEphemeralHostsRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ListEphemeralHosts(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ListEphemeralHosts_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ListEphemeralHosts(ctx, req.(*ListEphemeralHostsRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_MigrateEphemeralCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(MigrateEphemeralCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).MigrateEphemeralCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_MigrateEphemeralCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).MigrateEphemeralCell(ctx, req.(*MigrateEphemeralCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_HandOverEphemeralCell_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(HandOverEphemeralCellRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).HandOverEphemeralCell(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_HandOverEphemeralCell_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).HandOverEphemeralCell(ctx, req.(*HandOverEphemeralCellRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_AbortEphemeralMigration_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(AbortEphemeralMigrationRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).AbortEphemeralMigration(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_AbortEphemeralMigration_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).AbortEphemeralMigration(ctx, req.(*AbortEphemeralMigrationRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_ListDisks_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(ListDisksRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).ListDisks(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_ListDisks_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).ListDisks(ctx, req.(*ListDisksRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_SetDiskGovernance_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(SetDiskGovernanceRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).SetDiskGovernance(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_SetDiskGovernance_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).SetDiskGovernance(ctx, req.(*SetDiskGovernanceRequest))
	}
	return interceptor(ctx, in, info, handler)
}

func _Cladefsd_WaitEphemeralHandOver_Handler(srv interface{}, ctx context.Context, dec func(interface{}) error, interceptor grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(WaitEphemeralHandOverRequest)
	if err := dec(in); err != nil {
		return nil, err
	}
	if interceptor == nil {
		return srv.(CladefsdServer).WaitEphemeralHandOver(ctx, in)
	}
	info := &grpc.UnaryServerInfo{
		Server:     srv,
		FullMethod: Cladefsd_WaitEphemeralHandOver_FullMethodName,
	}
	handler := func(ctx context.Context, req interface{}) (interface{}, error) {
		return srv.(CladefsdServer).WaitEphemeralHandOver(ctx, req.(*WaitEphemeralHandOverRequest))
	}
	return interceptor(ctx, in, info, handler)
}

// Cladefsd_ServiceDesc is the grpc.ServiceDesc for Cladefsd service.
// It's only intended for direct use with grpc.RegisterService,
// and not to be introspected or modified (even as a copy)
var Cladefsd_ServiceDesc = grpc.ServiceDesc{
	ServiceName: "cladefs.cladefsd.v1.Cladefsd",
	HandlerType: (*CladefsdServer)(nil),
	Methods: []grpc.MethodDesc{
		{
			MethodName: "CreateCell",
			Handler:    _Cladefsd_CreateCell_Handler,
		},
		{
			MethodName: "ImportImage",
			Handler:    _Cladefsd_ImportImage_Handler,
		},
		{
			MethodName: "ImportMemory",
			Handler:    _Cladefsd_ImportMemory_Handler,
		},
		{
			MethodName: "ForkCell",
			Handler:    _Cladefsd_ForkCell_Handler,
		},
		{
			MethodName: "DeleteCell",
			Handler:    _Cladefsd_DeleteCell_Handler,
		},
		{
			MethodName: "DropCell",
			Handler:    _Cladefsd_DropCell_Handler,
		},
		{
			MethodName: "ResizeCell",
			Handler:    _Cladefsd_ResizeCell_Handler,
		},
		{
			MethodName: "PlaceCell",
			Handler:    _Cladefsd_PlaceCell_Handler,
		},
		{
			MethodName: "MakePublic",
			Handler:    _Cladefsd_MakePublic_Handler,
		},
		{
			MethodName: "MakeVersion",
			Handler:    _Cladefsd_MakeVersion_Handler,
		},
		{
			MethodName: "ListVersions",
			Handler:    _Cladefsd_ListVersions_Handler,
		},
		{
			MethodName: "DropVersions",
			Handler:    _Cladefsd_DropVersions_Handler,
		},
		{
			MethodName: "ShareVersion",
			Handler:    _Cladefsd_ShareVersion_Handler,
		},
		{
			MethodName: "AttachBase",
			Handler:    _Cladefsd_AttachBase_Handler,
		},
		{
			MethodName: "SyncCell",
			Handler:    _Cladefsd_SyncCell_Handler,
		},
		{
			MethodName: "StatCell",
			Handler:    _Cladefsd_StatCell_Handler,
		},
		{
			MethodName: "ListCells",
			Handler:    _Cladefsd_ListCells_Handler,
		},
		{
			MethodName: "GetCounters",
			Handler:    _Cladefsd_GetCounters_Handler,
		},
		{
			MethodName: "GetStats",
			Handler:    _Cladefsd_GetStats_Handler,
		},
		{
			MethodName: "Grant",
			Handler:    _Cladefsd_Grant_Handler,
		},
		{
			MethodName: "Revoke",
			Handler:    _Cladefsd_Revoke_Handler,
		},
		{
			MethodName: "SetMemoryLimit",
			Handler:    _Cladefsd_SetMemoryLimit_Handler,
		},
		{
			MethodName: "SetMemoryWeight",
			Handler:    _Cladefsd_SetMemoryWeight_Handler,
		},
		{
			MethodName: "SetTenantInFlight",
			Handler:    _Cladefsd_SetTenantInFlight_Handler,
		},
		{
			MethodName: "ShrinkMemory",
			Handler:    _Cladefsd_ShrinkMemory_Handler,
		},
		{
			MethodName: "SetDiskLimit",
			Handler:    _Cladefsd_SetDiskLimit_Handler,
		},
		{
			MethodName: "SetDiskWeight",
			Handler:    _Cladefsd_SetDiskWeight_Handler,
		},
		{
			MethodName: "GetTenantDisk",
			Handler:    _Cladefsd_GetTenantDisk_Handler,
		},
		{
			MethodName: "SetPeers",
			Handler:    _Cladefsd_SetPeers_Handler,
		},
		{
			MethodName: "ShredCell",
			Handler:    _Cladefsd_ShredCell_Handler,
		},
		{
			MethodName: "ForgetTenantKeys",
			Handler:    _Cladefsd_ForgetTenantKeys_Handler,
		},
		{
			MethodName: "ShredTenant",
			Handler:    _Cladefsd_ShredTenant_Handler,
		},
		{
			MethodName: "ReloadKms",
			Handler:    _Cladefsd_ReloadKms_Handler,
		},
		{
			MethodName: "DecommissionHost",
			Handler:    _Cladefsd_DecommissionHost_Handler,
		},
		{
			MethodName: "ListEphemeralHosts",
			Handler:    _Cladefsd_ListEphemeralHosts_Handler,
		},
		{
			MethodName: "MigrateEphemeralCell",
			Handler:    _Cladefsd_MigrateEphemeralCell_Handler,
		},
		{
			MethodName: "HandOverEphemeralCell",
			Handler:    _Cladefsd_HandOverEphemeralCell_Handler,
		},
		{
			MethodName: "AbortEphemeralMigration",
			Handler:    _Cladefsd_AbortEphemeralMigration_Handler,
		},
		{
			MethodName: "ListDisks",
			Handler:    _Cladefsd_ListDisks_Handler,
		},
		{
			MethodName: "SetDiskGovernance",
			Handler:    _Cladefsd_SetDiskGovernance_Handler,
		},
		{
			MethodName: "WaitEphemeralHandOver",
			Handler:    _Cladefsd_WaitEphemeralHandOver_Handler,
		},
	},
	Streams:  []grpc.StreamDesc{},
	Metadata: "cladefs/cladefsd/v1/cladefsd.proto",
}
