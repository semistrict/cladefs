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

// Code generated by protoc-gen-go. DO NOT EDIT.
// versions:
// 	protoc-gen-go v1.36.12
// 	protoc        (unknown)
// source: cladefs/cladefsd/v1/cladefsd.proto

package cladefsdv1

import (
	protoreflect "google.golang.org/protobuf/reflect/protoreflect"
	protoimpl "google.golang.org/protobuf/runtime/protoimpl"
	reflect "reflect"
	sync "sync"
	unsafe "unsafe"
)

const (
	// Verify that this generated code is sufficiently up-to-date.
	_ = protoimpl.EnforceVersion(20 - protoimpl.MinVersion)
	// Verify that runtime/protoimpl is sufficiently up-to-date.
	_ = protoimpl.EnforceVersion(protoimpl.MaxVersion - 20)
)

// Administrative placement/write policy for a managed disk.
type DiskPlacementMode int32

const (
	// Normal operation: accepts new cells, writes, and part-cache fills.
	DiskPlacementMode_DISK_PLACEMENT_READ_WRITE DiskPlacementMode = 0
	// Draining: no new cells placed here; existing cells keep running.
	DiskPlacementMode_DISK_PLACEMENT_DRAIN DiskPlacementMode = 1
	// Read-only: refuses new cells, cell writes, and part-cache fills.
	DiskPlacementMode_DISK_PLACEMENT_READ_ONLY DiskPlacementMode = 2
)

// Enum value maps for DiskPlacementMode.
var (
	DiskPlacementMode_name = map[int32]string{
		0: "DISK_PLACEMENT_READ_WRITE",
		1: "DISK_PLACEMENT_DRAIN",
		2: "DISK_PLACEMENT_READ_ONLY",
	}
	DiskPlacementMode_value = map[string]int32{
		"DISK_PLACEMENT_READ_WRITE": 0,
		"DISK_PLACEMENT_DRAIN":      1,
		"DISK_PLACEMENT_READ_ONLY":  2,
	}
)

func (x DiskPlacementMode) Enum() *DiskPlacementMode {
	p := new(DiskPlacementMode)
	*p = x
	return p
}

func (x DiskPlacementMode) String() string {
	return protoimpl.X.EnumStringOf(x.Descriptor(), protoreflect.EnumNumber(x))
}

func (DiskPlacementMode) Descriptor() protoreflect.EnumDescriptor {
	return file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[0].Descriptor()
}

func (DiskPlacementMode) Type() protoreflect.EnumType {
	return &file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[0]
}

func (x DiskPlacementMode) Number() protoreflect.EnumNumber {
	return protoreflect.EnumNumber(x)
}

// Deprecated: Use DiskPlacementMode.Descriptor instead.
func (DiskPlacementMode) EnumDescriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{0}
}

// Runtime health state of a managed disk.
type DiskHealthMode int32

const (
	// Healthy: serving reads, writes, and new cell placements.
	DiskHealthMode_DISK_HEALTH_ONLINE DiskHealthMode = 0
	// Experiencing consecutive I/O or statvfs errors; excluded from new placements.
	DiskHealthMode_DISK_HEALTH_SUSPECT DiskHealthMode = 1
	// Unreachable or failed past threshold; quarantined from writes and placements.
	DiskHealthMode_DISK_HEALTH_OFFLINE DiskHealthMode = 2
	// Recovering from offline after a healthy probe; awaiting consecutive successes.
	DiskHealthMode_DISK_HEALTH_RETURNING DiskHealthMode = 3
)

// Enum value maps for DiskHealthMode.
var (
	DiskHealthMode_name = map[int32]string{
		0: "DISK_HEALTH_ONLINE",
		1: "DISK_HEALTH_SUSPECT",
		2: "DISK_HEALTH_OFFLINE",
		3: "DISK_HEALTH_RETURNING",
	}
	DiskHealthMode_value = map[string]int32{
		"DISK_HEALTH_ONLINE":    0,
		"DISK_HEALTH_SUSPECT":   1,
		"DISK_HEALTH_OFFLINE":   2,
		"DISK_HEALTH_RETURNING": 3,
	}
)

func (x DiskHealthMode) Enum() *DiskHealthMode {
	p := new(DiskHealthMode)
	*p = x
	return p
}

func (x DiskHealthMode) String() string {
	return protoimpl.X.EnumStringOf(x.Descriptor(), protoreflect.EnumNumber(x))
}

func (DiskHealthMode) Descriptor() protoreflect.EnumDescriptor {
	return file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[1].Descriptor()
}

func (DiskHealthMode) Type() protoreflect.EnumType {
	return &file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[1]
}

func (x DiskHealthMode) Number() protoreflect.EnumNumber {
	return protoreflect.EnumNumber(x)
}

// Deprecated: Use DiskHealthMode.Descriptor instead.
func (DiskHealthMode) EnumDescriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{1}
}

// What a guest's fsync, fdatasync, msync and O_SYNC wait for in a cell (doc-12, doc-21, doc-29).
// Its sync, syncfs and unmount wait for its commits replicated in a replicated cell, durable in
// the fast WAL in a fast-WAL cell, and uploaded in any other.
type FsyncLevel int32

const (
	// In the daemon's memory, in order: survives what leaves the daemon alive.
	FsyncLevel_FSYNC_LEVEL_ORDERED FsyncLevel = 0
	// Deprecated: mapped to FSYNC_LEVEL_ORDERED with a warning.
	//
	// Deprecated: Marked as deprecated in cladefs/cladefsd/v1/cladefsd.proto.
	FsyncLevel_FSYNC_LEVEL_LOCAL FsyncLevel = 1
	// On f+1 hosts' disks, or uploaded: survives the writer's host.
	FsyncLevel_FSYNC_LEVEL_REPLICATED FsyncLevel = 2
	// Appended to the cell's fast WAL in appendable zonal object storage, or uploaded to the cold
	// regional bucket: survives losing the writer's host with zero local disk state (doc-29).
	FsyncLevel_FSYNC_LEVEL_FAST_WAL FsyncLevel = 3
)

// Enum value maps for FsyncLevel.
var (
	FsyncLevel_name = map[int32]string{
		0: "FSYNC_LEVEL_ORDERED",
		1: "FSYNC_LEVEL_LOCAL",
		2: "FSYNC_LEVEL_REPLICATED",
		3: "FSYNC_LEVEL_FAST_WAL",
	}
	FsyncLevel_value = map[string]int32{
		"FSYNC_LEVEL_ORDERED":    0,
		"FSYNC_LEVEL_LOCAL":      1,
		"FSYNC_LEVEL_REPLICATED": 2,
		"FSYNC_LEVEL_FAST_WAL":   3,
	}
)

func (x FsyncLevel) Enum() *FsyncLevel {
	p := new(FsyncLevel)
	*p = x
	return p
}

func (x FsyncLevel) String() string {
	return protoimpl.X.EnumStringOf(x.Descriptor(), protoreflect.EnumNumber(x))
}

func (FsyncLevel) Descriptor() protoreflect.EnumDescriptor {
	return file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[2].Descriptor()
}

func (FsyncLevel) Type() protoreflect.EnumType {
	return &file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[2]
}

func (x FsyncLevel) Number() protoreflect.EnumNumber {
	return protoreflect.EnumNumber(x)
}

// Deprecated: Use FsyncLevel.Descriptor instead.
func (FsyncLevel) EnumDescriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{2}
}

// What a cell holds, which says what opens it (the type's name is renamed with the rest,
// TASK-508.4).
type CellKind int32

const (
	// An Fxfs filesystem: a VM's disk, or a host's mount's.
	CellKind_CELL_KIND_FXFS CellKind = 0
	// A database of sqlite_cladefs, which its VFS alone opens.
	CellKind_CELL_KIND_SQLITE CellKind = 1
	// A VM's memory: a block_device whose contents are its image (TASK-456).
	CellKind_CELL_KIND_MEMORY CellKind = 2
)

// Enum value maps for CellKind.
var (
	CellKind_name = map[int32]string{
		0: "CELL_KIND_FXFS",
		1: "CELL_KIND_SQLITE",
		2: "CELL_KIND_MEMORY",
	}
	CellKind_value = map[string]int32{
		"CELL_KIND_FXFS":   0,
		"CELL_KIND_SQLITE": 1,
		"CELL_KIND_MEMORY": 2,
	}
)

func (x CellKind) Enum() *CellKind {
	p := new(CellKind)
	*p = x
	return p
}

func (x CellKind) String() string {
	return protoimpl.X.EnumStringOf(x.Descriptor(), protoreflect.EnumNumber(x))
}

func (CellKind) Descriptor() protoreflect.EnumDescriptor {
	return file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[3].Descriptor()
}

func (CellKind) Type() protoreflect.EnumType {
	return &file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[3]
}

func (x CellKind) Number() protoreflect.EnumNumber {
	return protoreflect.EnumNumber(x)
}

// Deprecated: Use CellKind.Descriptor instead.
func (CellKind) EnumDescriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{3}
}

// A live migration of an ephemeral cell to begin. The target daemon chooses its own managed JBOD
// disk.
type MigrateCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The peer address of the daemon it goes to.
	Peer string `protobuf:"bytes,2,opt,name=peer,proto3" json:"peer,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,4,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,5,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MigrateCellRequest) Reset() {
	*x = MigrateCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[0]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MigrateCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MigrateCellRequest) ProtoMessage() {}

func (x *MigrateCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[0]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use MigrateCellRequest.ProtoReflect.Descriptor instead.
func (*MigrateCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{0}
}

func (x *MigrateCellRequest) GetPeer() string {
	if x != nil {
		return x.Peer
	}
	return ""
}

func (x *MigrateCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *MigrateCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// A migration begun.
type MigrateCellResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its ID.
	Migration     uint64 `protobuf:"varint,1,opt,name=migration,proto3" json:"migration,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MigrateCellResponse) Reset() {
	*x = MigrateCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[1]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MigrateCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MigrateCellResponse) ProtoMessage() {}

func (x *MigrateCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[1]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use MigrateCellResponse.ProtoReflect.Descriptor instead.
func (*MigrateCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{1}
}

func (x *MigrateCellResponse) GetMigration() uint64 {
	if x != nil {
		return x.Migration
	}
	return 0
}

// A migration's hand-over.
type HandOverCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Deprecated: HandOverCell serves ephemeral cells only and takes no fencing token. Setting this
	// field is refused (INVALID_ARGUMENT, EINVAL).
	//
	// Deprecated: Marked as deprecated in cladefs/cladefsd/v1/cladefsd.proto.
	Token *uint64 `protobuf:"varint,2,opt,name=token,proto3,oneof" json:"token,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,3,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,4,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *HandOverCellRequest) Reset() {
	*x = HandOverCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[2]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *HandOverCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*HandOverCellRequest) ProtoMessage() {}

func (x *HandOverCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[2]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use HandOverCellRequest.ProtoReflect.Descriptor instead.
func (*HandOverCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{2}
}

// Deprecated: Marked as deprecated in cladefs/cladefsd/v1/cladefsd.proto.
func (x *HandOverCellRequest) GetToken() uint64 {
	if x != nil && x.Token != nil {
		return *x.Token
	}
	return 0
}

func (x *HandOverCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *HandOverCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// A migration handed over.
type HandOverCellResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *HandOverCellResponse) Reset() {
	*x = HandOverCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[3]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *HandOverCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*HandOverCellResponse) ProtoMessage() {}

func (x *HandOverCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[3]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use HandOverCellResponse.ProtoReflect.Descriptor instead.
func (*HandOverCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{3}
}

// A migration to end.
type AbortMigrationRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *AbortMigrationRequest) Reset() {
	*x = AbortMigrationRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[4]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *AbortMigrationRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*AbortMigrationRequest) ProtoMessage() {}

func (x *AbortMigrationRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[4]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use AbortMigrationRequest.ProtoReflect.Descriptor instead.
func (*AbortMigrationRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{4}
}

func (x *AbortMigrationRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *AbortMigrationRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// A migration ended.
type AbortMigrationResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *AbortMigrationResponse) Reset() {
	*x = AbortMigrationResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[5]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *AbortMigrationResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*AbortMigrationResponse) ProtoMessage() {}

func (x *AbortMigrationResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[5]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use AbortMigrationResponse.ProtoReflect.Descriptor instead.
func (*AbortMigrationResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{5}
}

// Asks for all managed disks and their governance state.
type ListDisksRequest struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListDisksRequest) Reset() {
	*x = ListDisksRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[6]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListDisksRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListDisksRequest) ProtoMessage() {}

func (x *ListDisksRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[6]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListDisksRequest.ProtoReflect.Descriptor instead.
func (*ListDisksRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{6}
}

// All managed disks and their live governance state.
type ListDisksResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Configured managed disks in configuration order.
	Disks         []*DiskInfo `protobuf:"bytes,1,rep,name=disks,proto3" json:"disks,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListDisksResponse) Reset() {
	*x = ListDisksResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[7]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListDisksResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListDisksResponse) ProtoMessage() {}

func (x *ListDisksResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[7]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListDisksResponse.ProtoReflect.Descriptor instead.
func (*ListDisksResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{7}
}

func (x *ListDisksResponse) GetDisks() []*DiskInfo {
	if x != nil {
		return x.Disks
	}
	return nil
}

// Live governance and usage state of one managed disk.
type DiskInfo struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Configured disk ID (from TOML `[[disks]].id`).
	Id string `protobuf:"bytes,1,opt,name=id,proto3" json:"id,omitempty"`
	// Kernel device `MAJOR:MINOR`.
	Device string `protobuf:"bytes,2,opt,name=device,proto3" json:"device,omitempty"`
	// Runtime health state.
	State DiskHealthMode `protobuf:"varint,3,opt,name=state,proto3,enum=cladefs.cladefsd.v1.DiskHealthMode" json:"state,omitempty"`
	// Administrative placement mode.
	Placement DiskPlacementMode `protobuf:"varint,4,opt,name=placement,proto3,enum=cladefs.cladefsd.v1.DiskPlacementMode" json:"placement,omitempty"`
	// Filesystem total size in bytes.
	SizeBytes uint64 `protobuf:"varint,5,opt,name=size_bytes,json=sizeBytes,proto3" json:"size_bytes,omitempty"`
	// Filesystem free bytes available to unprivileged writers.
	FreeBytes uint64 `protobuf:"varint,6,opt,name=free_bytes,json=freeBytes,proto3" json:"free_bytes,omitempty"`
	// Configured per-disk hard cap in bytes (0 if uncapped).
	CapBytes uint64 `protobuf:"varint,7,opt,name=cap_bytes,json=capBytes,proto3" json:"cap_bytes,omitempty"`
	// Effective governor headroom in bytes.
	HeadroomBytes uint64 `protobuf:"varint,8,opt,name=headroom_bytes,json=headroomBytes,proto3" json:"headroom_bytes,omitempty"`
	// Current governed allowance in bytes (cells + reserve + pledged + cache).
	AllowanceBytes uint64 `protobuf:"varint,9,opt,name=allowance_bytes,json=allowanceBytes,proto3" json:"allowance_bytes,omitempty"`
	// Bytes taken by cells on this disk.
	CellsBytes uint64 `protobuf:"varint,10,opt,name=cells_bytes,json=cellsBytes,proto3" json:"cells_bytes,omitempty"`
	// Reserved + pledged bytes held against this disk's allowance.
	ReservedBytes uint64 `protobuf:"varint,11,opt,name=reserved_bytes,json=reservedBytes,proto3" json:"reserved_bytes,omitempty"`
	// Bytes held by the part-cache shard on this disk.
	CacheBytes uint64 `protobuf:"varint,12,opt,name=cache_bytes,json=cacheBytes,proto3" json:"cache_bytes,omitempty"`
	// Configured per-disk part-cache hard cap in bytes (0 if uncapped).
	CacheCapBytes uint64 `protobuf:"varint,13,opt,name=cache_cap_bytes,json=cacheCapBytes,proto3" json:"cache_cap_bytes,omitempty"`
	// Spare bytes left by cells under `allowance_bytes`.
	SpareBytes uint64 `protobuf:"varint,14,opt,name=spare_bytes,json=spareBytes,proto3" json:"spare_bytes,omitempty"`
	// Whether this disk is currently under governor pressure.
	UnderPressure bool `protobuf:"varint,15,opt,name=under_pressure,json=underPressure,proto3" json:"under_pressure,omitempty"`
	// Configured per-disk dirty-page hard ceiling in bytes (0 if auto-divided).
	DirtyBudgetBytes uint64 `protobuf:"varint,16,opt,name=dirty_budget_bytes,json=dirtyBudgetBytes,proto3" json:"dirty_budget_bytes,omitempty"`
	unknownFields    protoimpl.UnknownFields
	sizeCache        protoimpl.SizeCache
}

func (x *DiskInfo) Reset() {
	*x = DiskInfo{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[8]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DiskInfo) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DiskInfo) ProtoMessage() {}

func (x *DiskInfo) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[8]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DiskInfo.ProtoReflect.Descriptor instead.
func (*DiskInfo) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{8}
}

func (x *DiskInfo) GetId() string {
	if x != nil {
		return x.Id
	}
	return ""
}

func (x *DiskInfo) GetDevice() string {
	if x != nil {
		return x.Device
	}
	return ""
}

func (x *DiskInfo) GetState() DiskHealthMode {
	if x != nil {
		return x.State
	}
	return DiskHealthMode_DISK_HEALTH_ONLINE
}

func (x *DiskInfo) GetPlacement() DiskPlacementMode {
	if x != nil {
		return x.Placement
	}
	return DiskPlacementMode_DISK_PLACEMENT_READ_WRITE
}

func (x *DiskInfo) GetSizeBytes() uint64 {
	if x != nil {
		return x.SizeBytes
	}
	return 0
}

func (x *DiskInfo) GetFreeBytes() uint64 {
	if x != nil {
		return x.FreeBytes
	}
	return 0
}

func (x *DiskInfo) GetCapBytes() uint64 {
	if x != nil {
		return x.CapBytes
	}
	return 0
}

func (x *DiskInfo) GetHeadroomBytes() uint64 {
	if x != nil {
		return x.HeadroomBytes
	}
	return 0
}

func (x *DiskInfo) GetAllowanceBytes() uint64 {
	if x != nil {
		return x.AllowanceBytes
	}
	return 0
}

func (x *DiskInfo) GetCellsBytes() uint64 {
	if x != nil {
		return x.CellsBytes
	}
	return 0
}

func (x *DiskInfo) GetReservedBytes() uint64 {
	if x != nil {
		return x.ReservedBytes
	}
	return 0
}

func (x *DiskInfo) GetCacheBytes() uint64 {
	if x != nil {
		return x.CacheBytes
	}
	return 0
}

func (x *DiskInfo) GetCacheCapBytes() uint64 {
	if x != nil {
		return x.CacheCapBytes
	}
	return 0
}

func (x *DiskInfo) GetSpareBytes() uint64 {
	if x != nil {
		return x.SpareBytes
	}
	return 0
}

func (x *DiskInfo) GetUnderPressure() bool {
	if x != nil {
		return x.UnderPressure
	}
	return false
}

func (x *DiskInfo) GetDirtyBudgetBytes() uint64 {
	if x != nil {
		return x.DirtyBudgetBytes
	}
	return 0
}

// Live runtime governance update for one managed disk.
type SetDiskGovernanceRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Configured disk ID (`DiskInfo.id`).
	Id string `protobuf:"bytes,1,opt,name=id,proto3" json:"id,omitempty"`
	// New per-disk hard cap in bytes (must be > 0).
	CapBytes *uint64 `protobuf:"varint,2,opt,name=cap_bytes,json=capBytes,proto3,oneof" json:"cap_bytes,omitempty"`
	// Clears the per-disk hard cap when true.
	ClearCap bool `protobuf:"varint,3,opt,name=clear_cap,json=clearCap,proto3" json:"clear_cap,omitempty"`
	// New per-disk part-cache hard cap in bytes (must be > 0).
	CacheCapBytes *uint64 `protobuf:"varint,4,opt,name=cache_cap_bytes,json=cacheCapBytes,proto3,oneof" json:"cache_cap_bytes,omitempty"`
	// Clears the per-disk part-cache cap when true.
	ClearCacheCap bool `protobuf:"varint,5,opt,name=clear_cache_cap,json=clearCacheCap,proto3" json:"clear_cache_cap,omitempty"`
	// New per-disk headroom in bytes (mutually exclusive with `headroom_percent`).
	HeadroomBytes *uint64 `protobuf:"varint,6,opt,name=headroom_bytes,json=headroomBytes,proto3,oneof" json:"headroom_bytes,omitempty"`
	// New per-disk headroom percentage `0..100` (mutually exclusive with `headroom_bytes`).
	HeadroomPercent *uint32 `protobuf:"varint,7,opt,name=headroom_percent,json=headroomPercent,proto3,oneof" json:"headroom_percent,omitempty"`
	// Clears the per-disk headroom override when true.
	ClearHeadroom bool `protobuf:"varint,8,opt,name=clear_headroom,json=clearHeadroom,proto3" json:"clear_headroom,omitempty"`
	// New per-disk dirty-page hard ceiling in bytes (must be > 0).
	DirtyBudgetBytes *uint64 `protobuf:"varint,9,opt,name=dirty_budget_bytes,json=dirtyBudgetBytes,proto3,oneof" json:"dirty_budget_bytes,omitempty"`
	// Clears the per-disk dirty-page ceiling override when true.
	ClearDirtyBudget bool `protobuf:"varint,10,opt,name=clear_dirty_budget,json=clearDirtyBudget,proto3" json:"clear_dirty_budget,omitempty"`
	// New placement mode, if set.
	Placement     *DiskPlacementMode `protobuf:"varint,11,opt,name=placement,proto3,enum=cladefs.cladefsd.v1.DiskPlacementMode,oneof" json:"placement,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetDiskGovernanceRequest) Reset() {
	*x = SetDiskGovernanceRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[9]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskGovernanceRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskGovernanceRequest) ProtoMessage() {}

func (x *SetDiskGovernanceRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[9]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetDiskGovernanceRequest.ProtoReflect.Descriptor instead.
func (*SetDiskGovernanceRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{9}
}

func (x *SetDiskGovernanceRequest) GetId() string {
	if x != nil {
		return x.Id
	}
	return ""
}

func (x *SetDiskGovernanceRequest) GetCapBytes() uint64 {
	if x != nil && x.CapBytes != nil {
		return *x.CapBytes
	}
	return 0
}

func (x *SetDiskGovernanceRequest) GetClearCap() bool {
	if x != nil {
		return x.ClearCap
	}
	return false
}

func (x *SetDiskGovernanceRequest) GetCacheCapBytes() uint64 {
	if x != nil && x.CacheCapBytes != nil {
		return *x.CacheCapBytes
	}
	return 0
}

func (x *SetDiskGovernanceRequest) GetClearCacheCap() bool {
	if x != nil {
		return x.ClearCacheCap
	}
	return false
}

func (x *SetDiskGovernanceRequest) GetHeadroomBytes() uint64 {
	if x != nil && x.HeadroomBytes != nil {
		return *x.HeadroomBytes
	}
	return 0
}

func (x *SetDiskGovernanceRequest) GetHeadroomPercent() uint32 {
	if x != nil && x.HeadroomPercent != nil {
		return *x.HeadroomPercent
	}
	return 0
}

func (x *SetDiskGovernanceRequest) GetClearHeadroom() bool {
	if x != nil {
		return x.ClearHeadroom
	}
	return false
}

func (x *SetDiskGovernanceRequest) GetDirtyBudgetBytes() uint64 {
	if x != nil && x.DirtyBudgetBytes != nil {
		return *x.DirtyBudgetBytes
	}
	return 0
}

func (x *SetDiskGovernanceRequest) GetClearDirtyBudget() bool {
	if x != nil {
		return x.ClearDirtyBudget
	}
	return false
}

func (x *SetDiskGovernanceRequest) GetPlacement() DiskPlacementMode {
	if x != nil && x.Placement != nil {
		return *x.Placement
	}
	return DiskPlacementMode_DISK_PLACEMENT_READ_WRITE
}

// Per-disk governance updated.
type SetDiskGovernanceResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetDiskGovernanceResponse) Reset() {
	*x = SetDiskGovernanceResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[10]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskGovernanceResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskGovernanceResponse) ProtoMessage() {}

func (x *SetDiskGovernanceResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[10]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetDiskGovernanceResponse.ProtoReflect.Descriptor instead.
func (*SetDiskGovernanceResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{10}
}

// Waits for an incoming migration hand-over of `(tenant, cell)` on this target daemon.
type WaitHandOverRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The migrating cell's tenant ID.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The migrating cell's ID.
	Cell uint64 `protobuf:"varint,2,opt,name=cell,proto3" json:"cell,omitempty"`
	// Optional migration ID (from MigrateCellResponse.migration). When 0, waits for any incoming
	// migration of `(tenant, cell)` (or returns immediately if the cell is already present locally
	// with no active migration). When non-zero, waits specifically for that migration of the cell.
	Migration     uint64 `protobuf:"varint,3,opt,name=migration,proto3" json:"migration,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *WaitHandOverRequest) Reset() {
	*x = WaitHandOverRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[11]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *WaitHandOverRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*WaitHandOverRequest) ProtoMessage() {}

func (x *WaitHandOverRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[11]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use WaitHandOverRequest.ProtoReflect.Descriptor instead.
func (*WaitHandOverRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{11}
}

func (x *WaitHandOverRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *WaitHandOverRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *WaitHandOverRequest) GetMigration() uint64 {
	if x != nil {
		return x.Migration
	}
	return 0
}

// An incoming migration hand-over accepted on this target daemon.
type WaitHandOverResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The migration ID that completed, or 0 if the cell was already present before this daemon
	// observed a migration for it.
	Migration uint64 `protobuf:"varint,1,opt,name=migration,proto3" json:"migration,omitempty"`
	// The cell's tenant ID.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *WaitHandOverResponse) Reset() {
	*x = WaitHandOverResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[12]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *WaitHandOverResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*WaitHandOverResponse) ProtoMessage() {}

func (x *WaitHandOverResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[12]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use WaitHandOverResponse.ProtoReflect.Descriptor instead.
func (*WaitHandOverResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{12}
}

func (x *WaitHandOverResponse) GetMigration() uint64 {
	if x != nil {
		return x.Migration
	}
	return 0
}

func (x *WaitHandOverResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *WaitHandOverResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// A host's incarnation to decommission.
type DecommissionHostRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The incarnation of the host's ephemeral cells' directory (CLADEFSD_EPHEMERAL).
	Incarnation uint64 `protobuf:"varint,1,opt,name=incarnation,proto3" json:"incarnation,omitempty"`
	// The most references to retire in this call: 0 for the daemon's default, 10,000.
	Most          uint64 `protobuf:"varint,2,opt,name=most,proto3" json:"most,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DecommissionHostRequest) Reset() {
	*x = DecommissionHostRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[13]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DecommissionHostRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DecommissionHostRequest) ProtoMessage() {}

func (x *DecommissionHostRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[13]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DecommissionHostRequest.ProtoReflect.Descriptor instead.
func (*DecommissionHostRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{13}
}

func (x *DecommissionHostRequest) GetIncarnation() uint64 {
	if x != nil {
		return x.Incarnation
	}
	return 0
}

func (x *DecommissionHostRequest) GetMost() uint64 {
	if x != nil {
		return x.Most
	}
	return 0
}

// What was retired.
type DecommissionHostResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// How many references of its ephemeral cells.
	Retired uint64 `protobuf:"varint,1,opt,name=retired,proto3" json:"retired,omitempty"`
	// How many it found and left, past `most`: a call again retires them.
	Left          uint64 `protobuf:"varint,2,opt,name=left,proto3" json:"left,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DecommissionHostResponse) Reset() {
	*x = DecommissionHostResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[14]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DecommissionHostResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DecommissionHostResponse) ProtoMessage() {}

func (x *DecommissionHostResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[14]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DecommissionHostResponse.ProtoReflect.Descriptor instead.
func (*DecommissionHostResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{14}
}

func (x *DecommissionHostResponse) GetRetired() uint64 {
	if x != nil {
		return x.Retired
	}
	return 0
}

func (x *DecommissionHostResponse) GetLeft() uint64 {
	if x != nil {
		return x.Left
	}
	return 0
}

// Asks for the incarnations of hosts with ephemeral cells' references in the buckets.
type ListEphemeralHostsRequest struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListEphemeralHostsRequest) Reset() {
	*x = ListEphemeralHostsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[15]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListEphemeralHostsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListEphemeralHostsRequest) ProtoMessage() {}

func (x *ListEphemeralHostsRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[15]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListEphemeralHostsRequest.ProtoReflect.Descriptor instead.
func (*ListEphemeralHostsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{15}
}

// The incarnations of hosts with ephemeral cells' references in the buckets, in order.
type ListEphemeralHostsResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Each incarnation, by its number.
	Hosts         []*EphemeralHost `protobuf:"bytes,1,rep,name=hosts,proto3" json:"hosts,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListEphemeralHostsResponse) Reset() {
	*x = ListEphemeralHostsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[16]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListEphemeralHostsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListEphemeralHostsResponse) ProtoMessage() {}

func (x *ListEphemeralHostsResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[16]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListEphemeralHostsResponse.ProtoReflect.Descriptor instead.
func (*ListEphemeralHostsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{16}
}

func (x *ListEphemeralHostsResponse) GetHosts() []*EphemeralHost {
	if x != nil {
		return x.Hosts
	}
	return nil
}

// A host's incarnation, as the buckets hold it.
type EphemeralHost struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its incarnation (DecommissionHostRequest's).
	Incarnation uint64 `protobuf:"varint,1,opt,name=incarnation,proto3" json:"incarnation,omitempty"`
	// How many references of its ephemeral cells the buckets' index names.
	References uint64 `protobuf:"varint,2,opt,name=references,proto3" json:"references,omitempty"`
	// Whether it was decommissioned (DecommissionHost).
	Decommissioned bool `protobuf:"varint,3,opt,name=decommissioned,proto3" json:"decommissioned,omitempty"`
	unknownFields  protoimpl.UnknownFields
	sizeCache      protoimpl.SizeCache
}

func (x *EphemeralHost) Reset() {
	*x = EphemeralHost{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[17]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *EphemeralHost) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*EphemeralHost) ProtoMessage() {}

func (x *EphemeralHost) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[17]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use EphemeralHost.ProtoReflect.Descriptor instead.
func (*EphemeralHost) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{17}
}

func (x *EphemeralHost) GetIncarnation() uint64 {
	if x != nil {
		return x.Incarnation
	}
	return 0
}

func (x *EphemeralHost) GetReferences() uint64 {
	if x != nil {
		return x.References
	}
	return 0
}

func (x *EphemeralHost) GetDecommissioned() bool {
	if x != nil {
		return x.Decommissioned
	}
	return false
}

// Makes an empty cell. The daemon chooses the managed JBOD disk automatically.
type CreateCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The most bytes it holds.
	Capacity uint64 `protobuf:"varint,2,opt,name=capacity,proto3" json:"capacity,omitempty"`
	// The fencing token to place it at (PlaceCell), if any: of a placement no earlier than the
	// last writer's of the cell to take, or the take fails (FAILED_PRECONDITION, ESTALE).
	Token *uint64 `protobuf:"varint,4,opt,name=token,proto3,oneof" json:"token,omitempty"`
	// The cell's tenant (0 for the default tenant).
	Tenant uint64 `protobuf:"varint,5,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell kept in the daemon's bucket to take (NOT_FOUND, ENOENT, if it has none), or 0 for a
	// new one, its ID, unique in the daemon's bucket, the bucket's to give (TASK-364), which the
	// response says.
	Cell uint64 `protobuf:"varint,6,opt,name=cell,proto3" json:"cell,omitempty"`
	// The cell's kind, which says what opens it.
	Kind CellKind `protobuf:"varint,7,opt,name=kind,proto3,enum=cladefs.cladefsd.v1.CellKind" json:"kind,omitempty"`
	// Whether the cell is replicated (doc-21, TASK-537): its f, its commits on f+1 of 2f+1 hosts'
	// disks before the bucket, the hosts the daemon's peers (SetPeers); 0 for a cell durable off its
	// host once uploaded alone. At most 3. Unset keeps what the cell's head says, of a cell taken,
	// and makes a new one not replicated; set, it is the cell's from this taking on, on any host.
	// Of 1 or more, refused by a daemon with no peers or no replica directory (ENOTSUP).
	Replicas *uint32 `protobuf:"varint,9,opt,name=replicas,proto3,oneof" json:"replicas,omitempty"`
	// What its guest's fsync waits for (doc-12, doc-21), given with `replicas` alone (INVALID_ARGUMENT
	// otherwise): FSYNC_LEVEL_REPLICATED needs replicas of 1 or more.
	Fsync FsyncLevel `protobuf:"varint,10,opt,name=fsync,proto3,enum=cladefs.cladefsd.v1.FsyncLevel" json:"fsync,omitempty"`
	// Whether its taking of a replicated cell is forced (doc-21, TASK-681), as PlaceCell's `force`
	// places a cell: it goes on with the members of the configuration the cell's head names that
	// answer, fewer than a take needs, as when f+1 of them are gone for good; commits told replicated
	// that only the others held are lost, and StatCell's forced_after says past which commit. With
	// `token`, the placement it is placed at is forced from then on.
	Force bool `protobuf:"varint,11,opt,name=force,proto3" json:"force,omitempty"`
	// Optional per-cell uploader interval in milliseconds (hotbed question 534). Unset keeps the
	// daemon default; 0 or values past 24 hours are refused (INVALID_ARGUMENT).
	UploadIntervalMs *uint64 `protobuf:"varint,12,opt,name=upload_interval_ms,json=uploadIntervalMs,proto3,oneof" json:"upload_interval_ms,omitempty"`
	// Whether the cell uses Fast WAL commits backed by appendable zonal object storage (doc-29):
	// compute nodes stay stateless cache-only with zero local disk fsyncs. Unset keeps what the
	// cell's head says (and makes a new cell without Fast WAL); true enables Fast WAL (refused with
	// ENOTSUP if CLADEFSD_FAST_WAL_BUCKET is unset, and EINVAL if combined with replicas >= 1);
	// false disables Fast WAL, flushing any active WAL on take.
	FastWal *bool `protobuf:"varint,13,opt,name=fast_wal,json=fastWal,proto3,oneof" json:"fast_wal,omitempty"`
	// Whether to create a standalone (parentless) host-local ephemeral cell (doc-25): kept on host
	// SSD alone, encrypted under the tenant's KMS key, never uploaded to the bucket, with no
	// versions and no bucket reference. Must be a new cell (`cell == 0`) with `token`, `replicas`,
	// `force`, `upload_interval_ms`, and `fast_wal` unset and `fsync == FSYNC_LEVEL_ORDERED`
	// (INVALID_ARGUMENT otherwise).
	Ephemeral     bool `protobuf:"varint,14,opt,name=ephemeral,proto3" json:"ephemeral,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *CreateCellRequest) Reset() {
	*x = CreateCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[18]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *CreateCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*CreateCellRequest) ProtoMessage() {}

func (x *CreateCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[18]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use CreateCellRequest.ProtoReflect.Descriptor instead.
func (*CreateCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{18}
}

func (x *CreateCellRequest) GetCapacity() uint64 {
	if x != nil {
		return x.Capacity
	}
	return 0
}

func (x *CreateCellRequest) GetToken() uint64 {
	if x != nil && x.Token != nil {
		return *x.Token
	}
	return 0
}

func (x *CreateCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *CreateCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *CreateCellRequest) GetKind() CellKind {
	if x != nil {
		return x.Kind
	}
	return CellKind_CELL_KIND_FXFS
}

func (x *CreateCellRequest) GetReplicas() uint32 {
	if x != nil && x.Replicas != nil {
		return *x.Replicas
	}
	return 0
}

func (x *CreateCellRequest) GetFsync() FsyncLevel {
	if x != nil {
		return x.Fsync
	}
	return FsyncLevel_FSYNC_LEVEL_ORDERED
}

func (x *CreateCellRequest) GetForce() bool {
	if x != nil {
		return x.Force
	}
	return false
}

func (x *CreateCellRequest) GetUploadIntervalMs() uint64 {
	if x != nil && x.UploadIntervalMs != nil {
		return *x.UploadIntervalMs
	}
	return 0
}

func (x *CreateCellRequest) GetFastWal() bool {
	if x != nil && x.FastWal != nil {
		return *x.FastWal
	}
	return false
}

func (x *CreateCellRequest) GetEphemeral() bool {
	if x != nil {
		return x.Ephemeral
	}
	return false
}

// The cell made.
type CreateCellResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell: the one named, or the new one.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *CreateCellResponse) Reset() {
	*x = CreateCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[19]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *CreateCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*CreateCellResponse) ProtoMessage() {}

func (x *CreateCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[19]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use CreateCellResponse.ProtoReflect.Descriptor instead.
func (*CreateCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{19}
}

func (x *CreateCellResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *CreateCellResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// Makes a cell of a container image. The daemon chooses the managed JBOD disk automatically.
type ImportImageRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The most bytes it holds.
	Capacity uint64 `protobuf:"varint,2,opt,name=capacity,proto3" json:"capacity,omitempty"`
	// The image, on the daemon's host: an OCI image layout (as `skopeo copy` makes, or
	// `docker save`'s tarball extracted), or a tar archive of one layer, gzipped or not (as
	// `docker export` makes).
	Image string `protobuf:"bytes,3,opt,name=image,proto3" json:"image,omitempty"`
	// The image in the layout to import, by its reference; empty for its only image.
	Reference string `protobuf:"bytes,4,opt,name=reference,proto3" json:"reference,omitempty"`
	// The tenant of the new cell (0 for the default tenant).
	Tenant        uint64 `protobuf:"varint,5,opt,name=tenant,proto3" json:"tenant,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ImportImageRequest) Reset() {
	*x = ImportImageRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[20]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ImportImageRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ImportImageRequest) ProtoMessage() {}

func (x *ImportImageRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[20]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ImportImageRequest.ProtoReflect.Descriptor instead.
func (*ImportImageRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{20}
}

func (x *ImportImageRequest) GetCapacity() uint64 {
	if x != nil {
		return x.Capacity
	}
	return 0
}

func (x *ImportImageRequest) GetImage() string {
	if x != nil {
		return x.Image
	}
	return ""
}

func (x *ImportImageRequest) GetReference() string {
	if x != nil {
		return x.Reference
	}
	return ""
}

func (x *ImportImageRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

// The cell made.
type ImportImageResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell, a new one.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ImportImageResponse) Reset() {
	*x = ImportImageResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[21]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ImportImageResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ImportImageResponse) ProtoMessage() {}

func (x *ImportImageResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[21]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ImportImageResponse.ProtoReflect.Descriptor instead.
func (*ImportImageResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{21}
}

func (x *ImportImageResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ImportImageResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// Makes a memory cell of a snapshot's memory file (board 11222). The daemon chooses the managed
// JBOD disk automatically.
type ImportMemoryRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The memory file, on the daemon's host: non-empty, a whole number of 4 KiB pages.
	File string `protobuf:"bytes,2,opt,name=file,proto3" json:"file,omitempty"`
	// The tenant of the new cell (0 for the default tenant).
	Tenant uint64 `protobuf:"varint,3,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Whether to make an initial version of the memory cell as part of the import.
	MakeVersion bool `protobuf:"varint,4,opt,name=make_version,json=makeVersion,proto3" json:"make_version,omitempty"`
	// Whether to import into a standalone host-local ephemeral memory cell (doc-25): kept on host
	// SSD alone with zero bucket uploads. Cannot be combined with `make_version` (INVALID_ARGUMENT).
	Ephemeral     bool `protobuf:"varint,5,opt,name=ephemeral,proto3" json:"ephemeral,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ImportMemoryRequest) Reset() {
	*x = ImportMemoryRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[22]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ImportMemoryRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ImportMemoryRequest) ProtoMessage() {}

func (x *ImportMemoryRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[22]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ImportMemoryRequest.ProtoReflect.Descriptor instead.
func (*ImportMemoryRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{22}
}

func (x *ImportMemoryRequest) GetFile() string {
	if x != nil {
		return x.File
	}
	return ""
}

func (x *ImportMemoryRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ImportMemoryRequest) GetMakeVersion() bool {
	if x != nil {
		return x.MakeVersion
	}
	return false
}

func (x *ImportMemoryRequest) GetEphemeral() bool {
	if x != nil {
		return x.Ephemeral
	}
	return false
}

// The memory cell made.
type ImportMemoryResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its tenant.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell, a new one.
	Cell uint64 `protobuf:"varint,2,opt,name=cell,proto3" json:"cell,omitempty"`
	// Its initial version, if `make_version` was set; 0 otherwise.
	Version       uint64 `protobuf:"varint,3,opt,name=version,proto3" json:"version,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ImportMemoryResponse) Reset() {
	*x = ImportMemoryResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[23]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ImportMemoryResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ImportMemoryResponse) ProtoMessage() {}

func (x *ImportMemoryResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[23]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ImportMemoryResponse.ProtoReflect.Descriptor instead.
func (*ImportMemoryResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{23}
}

func (x *ImportMemoryResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ImportMemoryResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *ImportMemoryResponse) GetVersion() uint64 {
	if x != nil {
		return x.Version
	}
	return 0
}

// Forks a cell (or a version of a cell). The daemon chooses the managed JBOD disk automatically.
type ForkCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// If not 0, the version to fork of `(tenant, cell)` on this host or any other; if 0, forks the
	// local cell `(tenant, cell)` directly.
	Version uint64 `protobuf:"varint,6,opt,name=version,proto3" json:"version,omitempty"`
	// The source cell's tenant.
	Tenant uint64 `protobuf:"varint,7,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The source cell's ID.
	Cell uint64 `protobuf:"varint,8,opt,name=cell,proto3" json:"cell,omitempty"`
	// Deprecated: Marked as deprecated in cladefs/cladefsd/v1/cladefsd.proto.
	Ephemeral bool `protobuf:"varint,9,opt,name=ephemeral,proto3" json:"ephemeral,omitempty"`
	// Optional destination tenant for the forked cell; unset or 0 inherits the source cell's tenant.
	// Cross-tenant forks of a non-default-tenant cell require a shared version (PERMISSION_DENIED).
	TargetTenant *uint64 `protobuf:"varint,10,opt,name=target_tenant,json=targetTenant,proto3,oneof" json:"target_tenant,omitempty"`
	// Whether the fork is a persistent (bucket-backed) cell. Defaults to true when unset; set to
	// false for an ephemeral cell (backlog doc-25): kept on the daemon's host alone, never uploaded,
	// lost with the host's disk, with no versions of its own. Of `version` alone
	// (INVALID_ARGUMENT otherwise); refused by a daemon keeping no ephemeral cells (ENOTSUP).
	Persistent    *bool `protobuf:"varint,11,opt,name=persistent,proto3,oneof" json:"persistent,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ForkCellRequest) Reset() {
	*x = ForkCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[24]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForkCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForkCellRequest) ProtoMessage() {}

func (x *ForkCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[24]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ForkCellRequest.ProtoReflect.Descriptor instead.
func (*ForkCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{24}
}

func (x *ForkCellRequest) GetVersion() uint64 {
	if x != nil {
		return x.Version
	}
	return 0
}

func (x *ForkCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ForkCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// Deprecated: Marked as deprecated in cladefs/cladefsd/v1/cladefsd.proto.
func (x *ForkCellRequest) GetEphemeral() bool {
	if x != nil {
		return x.Ephemeral
	}
	return false
}

func (x *ForkCellRequest) GetTargetTenant() uint64 {
	if x != nil && x.TargetTenant != nil {
		return *x.TargetTenant
	}
	return 0
}

func (x *ForkCellRequest) GetPersistent() bool {
	if x != nil && x.Persistent != nil {
		return *x.Persistent
	}
	return false
}

// The fork made.
type ForkCellResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell, a new one.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ForkCellResponse) Reset() {
	*x = ForkCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[25]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForkCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForkCellResponse) ProtoMessage() {}

func (x *ForkCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[25]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ForkCellResponse.ProtoReflect.Descriptor instead.
func (*ForkCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{25}
}

func (x *ForkCellResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ForkCellResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// Deletes a cell.
type DeleteCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DeleteCellRequest) Reset() {
	*x = DeleteCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[26]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DeleteCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DeleteCellRequest) ProtoMessage() {}

func (x *DeleteCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[26]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DeleteCellRequest.ProtoReflect.Descriptor instead.
func (*DeleteCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{26}
}

func (x *DeleteCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *DeleteCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The cell deleted.
type DeleteCellResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DeleteCellResponse) Reset() {
	*x = DeleteCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[27]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DeleteCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DeleteCellResponse) ProtoMessage() {}

func (x *DeleteCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[27]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DeleteCellResponse.ProtoReflect.Descriptor instead.
func (*DeleteCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{27}
}

// Deletes this host's copy of a cell.
type DropCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropCellRequest) Reset() {
	*x = DropCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[28]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropCellRequest) ProtoMessage() {}

func (x *DropCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[28]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DropCellRequest.ProtoReflect.Descriptor instead.
func (*DropCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{28}
}

func (x *DropCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *DropCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// This host's copy deleted.
type DropCellResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropCellResponse) Reset() {
	*x = DropCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[29]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropCellResponse) ProtoMessage() {}

func (x *DropCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[29]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DropCellResponse.ProtoReflect.Descriptor instead.
func (*DropCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{29}
}

// Sets a cell's capacity.
type ResizeCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its new capacity, in bytes.
	Capacity uint64 `protobuf:"varint,2,opt,name=capacity,proto3" json:"capacity,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,3,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,4,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ResizeCellRequest) Reset() {
	*x = ResizeCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[30]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ResizeCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ResizeCellRequest) ProtoMessage() {}

func (x *ResizeCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[30]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ResizeCellRequest.ProtoReflect.Descriptor instead.
func (*ResizeCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{30}
}

func (x *ResizeCellRequest) GetCapacity() uint64 {
	if x != nil {
		return x.Capacity
	}
	return 0
}

func (x *ResizeCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ResizeCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The cell resized.
type ResizeCellResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ResizeCellResponse) Reset() {
	*x = ResizeCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[31]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ResizeCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ResizeCellResponse) ProtoMessage() {}

func (x *ResizeCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[31]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ResizeCellResponse.ProtoReflect.Descriptor instead.
func (*ResizeCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{31}
}

// Places a cell.
type PlaceCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The placement's fencing token, its boot ID: greater than every earlier placement's.
	Token uint64 `protobuf:"varint,2,opt,name=token,proto3" json:"token,omitempty"`
	// Whether this placement's takings of a replicated cell are forced (doc-21, TASK-681).
	Force bool `protobuf:"varint,3,opt,name=force,proto3" json:"force,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,4,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,5,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *PlaceCellRequest) Reset() {
	*x = PlaceCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[32]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *PlaceCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*PlaceCellRequest) ProtoMessage() {}

func (x *PlaceCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[32]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use PlaceCellRequest.ProtoReflect.Descriptor instead.
func (*PlaceCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{32}
}

func (x *PlaceCellRequest) GetToken() uint64 {
	if x != nil {
		return x.Token
	}
	return 0
}

func (x *PlaceCellRequest) GetForce() bool {
	if x != nil {
		return x.Force
	}
	return false
}

func (x *PlaceCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *PlaceCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The cell placed.
type PlaceCellResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *PlaceCellResponse) Reset() {
	*x = PlaceCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[33]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *PlaceCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*PlaceCellResponse) ProtoMessage() {}

func (x *PlaceCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[33]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use PlaceCellResponse.ProtoReflect.Descriptor instead.
func (*PlaceCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{33}
}

// Makes a version of a cell.
type MakeVersionRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// If not 0, how many versions of the cell to keep, this one the newest: the rest are dropped
	// once the head names it.
	KeepLast uint32 `protobuf:"varint,2,opt,name=keep_last,json=keepLast,proto3" json:"keep_last,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,4,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,5,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MakeVersionRequest) Reset() {
	*x = MakeVersionRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[34]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakeVersionRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakeVersionRequest) ProtoMessage() {}

func (x *MakeVersionRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[34]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use MakeVersionRequest.ProtoReflect.Descriptor instead.
func (*MakeVersionRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{34}
}

func (x *MakeVersionRequest) GetKeepLast() uint32 {
	if x != nil {
		return x.KeepLast
	}
	return 0
}

func (x *MakeVersionRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *MakeVersionRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The version made.
type MakeVersionResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its ID, unique in its cell.
	Version       uint64 `protobuf:"varint,1,opt,name=version,proto3" json:"version,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MakeVersionResponse) Reset() {
	*x = MakeVersionResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[35]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakeVersionResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakeVersionResponse) ProtoMessage() {}

func (x *MakeVersionResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[35]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use MakeVersionResponse.ProtoReflect.Descriptor instead.
func (*MakeVersionResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{35}
}

func (x *MakeVersionResponse) GetVersion() uint64 {
	if x != nil {
		return x.Version
	}
	return 0
}

// Lists the versions of a cell.
type ListVersionsRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListVersionsRequest) Reset() {
	*x = ListVersionsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[36]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListVersionsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListVersionsRequest) ProtoMessage() {}

func (x *ListVersionsRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[36]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListVersionsRequest.ProtoReflect.Descriptor instead.
func (*ListVersionsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{36}
}

func (x *ListVersionsRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ListVersionsRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// A version of a cell.
type Version struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its ID.
	Id uint64 `protobuf:"varint,1,opt,name=id,proto3" json:"id,omitempty"`
	// Whether the cell's head names it: forks of it are made on any host then.
	Uploaded bool `protobuf:"varint,2,opt,name=uploaded,proto3" json:"uploaded,omitempty"`
	// Whether forks of it into other tenants' cells are made (ShareVersion).
	Shared        bool `protobuf:"varint,3,opt,name=shared,proto3" json:"shared,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *Version) Reset() {
	*x = Version{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[37]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *Version) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*Version) ProtoMessage() {}

func (x *Version) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[37]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use Version.ProtoReflect.Descriptor instead.
func (*Version) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{37}
}

func (x *Version) GetId() uint64 {
	if x != nil {
		return x.Id
	}
	return 0
}

func (x *Version) GetUploaded() bool {
	if x != nil {
		return x.Uploaded
	}
	return false
}

func (x *Version) GetShared() bool {
	if x != nil {
		return x.Shared
	}
	return false
}

// The versions of the cell.
type ListVersionsResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Oldest first: those its head names, then those made here not yet uploaded.
	Versions      []*Version `protobuf:"bytes,1,rep,name=versions,proto3" json:"versions,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListVersionsResponse) Reset() {
	*x = ListVersionsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[38]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListVersionsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListVersionsResponse) ProtoMessage() {}

func (x *ListVersionsResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[38]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListVersionsResponse.ProtoReflect.Descriptor instead.
func (*ListVersionsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{38}
}

func (x *ListVersionsResponse) GetVersions() []*Version {
	if x != nil {
		return x.Versions
	}
	return nil
}

// Drops versions of a cell.
type DropVersionsRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The versions to drop.
	Versions []uint64 `protobuf:"varint,2,rep,packed,name=versions,proto3" json:"versions,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,3,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,4,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropVersionsRequest) Reset() {
	*x = DropVersionsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[39]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropVersionsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropVersionsRequest) ProtoMessage() {}

func (x *DropVersionsRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[39]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DropVersionsRequest.ProtoReflect.Descriptor instead.
func (*DropVersionsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{39}
}

func (x *DropVersionsRequest) GetVersions() []uint64 {
	if x != nil {
		return x.Versions
	}
	return nil
}

func (x *DropVersionsRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *DropVersionsRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The versions dropped.
type DropVersionsResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropVersionsResponse) Reset() {
	*x = DropVersionsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[40]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropVersionsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropVersionsResponse) ProtoMessage() {}

func (x *DropVersionsResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[40]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use DropVersionsResponse.ProtoReflect.Descriptor instead.
func (*DropVersionsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{40}
}

// Shares a version, or no longer.
type ShareVersionRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The version, uploaded.
	Version uint64 `protobuf:"varint,2,opt,name=version,proto3" json:"version,omitempty"`
	// Whether other tenants' forks of it are made from now on.
	Shared bool `protobuf:"varint,3,opt,name=shared,proto3" json:"shared,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,4,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,5,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShareVersionRequest) Reset() {
	*x = ShareVersionRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[41]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShareVersionRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShareVersionRequest) ProtoMessage() {}

func (x *ShareVersionRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[41]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShareVersionRequest.ProtoReflect.Descriptor instead.
func (*ShareVersionRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{41}
}

func (x *ShareVersionRequest) GetVersion() uint64 {
	if x != nil {
		return x.Version
	}
	return 0
}

func (x *ShareVersionRequest) GetShared() bool {
	if x != nil {
		return x.Shared
	}
	return false
}

func (x *ShareVersionRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ShareVersionRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The version shared, or no longer.
type ShareVersionResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShareVersionResponse) Reset() {
	*x = ShareVersionResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[42]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShareVersionResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShareVersionResponse) ProtoMessage() {}

func (x *ShareVersionResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[42]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShareVersionResponse.ProtoReflect.Descriptor instead.
func (*ShareVersionResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{42}
}

// Makes a cell a public base.
type MakePublicRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its branch alone, not those it inherited: a fork pinning a state of a cell that goes on being
	// written, to attach to cells (AttachBase), not to fork.
	AttachOnly bool `protobuf:"varint,2,opt,name=attach_only,json=attachOnly,proto3" json:"attach_only,omitempty"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,3,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,4,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MakePublicRequest) Reset() {
	*x = MakePublicRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[43]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakePublicRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakePublicRequest) ProtoMessage() {}

func (x *MakePublicRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[43]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use MakePublicRequest.ProtoReflect.Descriptor instead.
func (*MakePublicRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{43}
}

func (x *MakePublicRequest) GetAttachOnly() bool {
	if x != nil {
		return x.AttachOnly
	}
	return false
}

func (x *MakePublicRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *MakePublicRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The cell made public.
type MakePublicResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MakePublicResponse) Reset() {
	*x = MakePublicResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[44]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakePublicResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakePublicResponse) ProtoMessage() {}

func (x *MakePublicResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[44]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use MakePublicResponse.ProtoReflect.Descriptor instead.
func (*MakePublicResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{44}
}

// Attaches a fork to a cell.
type AttachBaseRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,3,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell uint64 `protobuf:"varint,4,opt,name=cell,proto3" json:"cell,omitempty"`
	// The attached base fork's tenant.
	BaseTenant uint64 `protobuf:"varint,5,opt,name=base_tenant,json=baseTenant,proto3" json:"base_tenant,omitempty"`
	// The attached base fork's cell ID.
	BaseCell      uint64 `protobuf:"varint,6,opt,name=base_cell,json=baseCell,proto3" json:"base_cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *AttachBaseRequest) Reset() {
	*x = AttachBaseRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[45]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *AttachBaseRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*AttachBaseRequest) ProtoMessage() {}

func (x *AttachBaseRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[45]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use AttachBaseRequest.ProtoReflect.Descriptor instead.
func (*AttachBaseRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{45}
}

func (x *AttachBaseRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *AttachBaseRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *AttachBaseRequest) GetBaseTenant() uint64 {
	if x != nil {
		return x.BaseTenant
	}
	return 0
}

func (x *AttachBaseRequest) GetBaseCell() uint64 {
	if x != nil {
		return x.BaseCell
	}
	return 0
}

// Attached, durably.
type AttachBaseResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *AttachBaseResponse) Reset() {
	*x = AttachBaseResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[46]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *AttachBaseResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*AttachBaseResponse) ProtoMessage() {}

func (x *AttachBaseResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[46]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use AttachBaseResponse.ProtoReflect.Descriptor instead.
func (*AttachBaseResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{46}
}

// Waits for a cell's commits to be in object storage.
type SyncCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SyncCellRequest) Reset() {
	*x = SyncCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[47]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SyncCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SyncCellRequest) ProtoMessage() {}

func (x *SyncCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[47]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SyncCellRequest.ProtoReflect.Descriptor instead.
func (*SyncCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{47}
}

func (x *SyncCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *SyncCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The cell's commits, as of the call, are in object storage.
type SyncCellResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SyncCellResponse) Reset() {
	*x = SyncCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[48]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SyncCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SyncCellResponse) ProtoMessage() {}

func (x *SyncCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[48]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SyncCellResponse.ProtoReflect.Descriptor instead.
func (*SyncCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{48}
}

// Asks what a cell is, and holds.
type StatCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *StatCellRequest) Reset() {
	*x = StatCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[49]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *StatCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*StatCellRequest) ProtoMessage() {}

func (x *StatCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[49]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use StatCellRequest.ProtoReflect.Descriptor instead.
func (*StatCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{49}
}

func (x *StatCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *StatCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// What a cell is, and holds.
type StatCellResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The most bytes it holds.
	Capacity uint64 `protobuf:"varint,2,opt,name=capacity,proto3" json:"capacity,omitempty"`
	// The bytes its extents hold.
	Used uint64 `protobuf:"varint,3,opt,name=used,proto3" json:"used,omitempty"`
	// The bytes left for new extents.
	Available uint64 `protobuf:"varint,4,opt,name=available,proto3" json:"available,omitempty"`
	// Whether it is a public base (MakePublic).
	Public bool `protobuf:"varint,5,opt,name=public,proto3" json:"public,omitempty"`
	// Whether a VM process has it open.
	Open bool `protobuf:"varint,6,opt,name=open,proto3" json:"open,omitempty"`
	// Of the bytes its extents hold (used), those of the extents its branch wrote: what billing it
	// alone charges it (TASK-49).
	Own uint64 `protobuf:"varint,7,opt,name=own,proto3" json:"own,omitempty"`
	// Those of the extents it inherited from the branches it was forked from, which may still share
	// them: used is own and inherited.
	Inherited uint64 `protobuf:"varint,8,opt,name=inherited,proto3" json:"inherited,omitempty"`
	// The commit number of its latest commit.
	Committed uint64 `protobuf:"varint,9,opt,name=committed,proto3" json:"committed,omitempty"`
	// Whether its commits upload to the daemon's bucket now: while a VM process has it open, as
	// writer. If not, the fields below are zero (TASK-50.5).
	Uploading bool `protobuf:"varint,10,opt,name=uploading,proto3" json:"uploading,omitempty"`
	// The fencing token of its writer's taking of its branch's head: this host's placement's.
	TakenToken uint64 `protobuf:"varint,11,opt,name=taken_token,json=takenToken,proto3" json:"taken_token,omitempty"`
	// The epoch of its writer's taking of its branch's head, one more than the taking before.
	TakenEpoch uint64 `protobuf:"varint,12,opt,name=taken_epoch,json=takenEpoch,proto3" json:"taken_epoch,omitempty"`
	// When its writer's taking of its branch's head was, in milliseconds since the Unix epoch.
	TakenUnixMs uint64 `protobuf:"varint,13,opt,name=taken_unix_ms,json=takenUnixMs,proto3" json:"taken_unix_ms,omitempty"`
	// The commit number of its latest commit in the bucket.
	Uploaded uint64 `protobuf:"varint,14,opt,name=uploaded,proto3" json:"uploaded,omitempty"`
	// The bytes its commits not yet in the bucket wrote.
	PendingUploadBytes uint64 `protobuf:"varint,15,opt,name=pending_upload_bytes,json=pendingUploadBytes,proto3" json:"pending_upload_bytes,omitempty"`
	// How long the oldest commit not yet in the bucket has waited, in milliseconds, to within the
	// uploader's interval (a second): 0 if none waits.
	OldestPendingMs uint64 `protobuf:"varint,16,opt,name=oldest_pending_ms,json=oldestPendingMs,proto3" json:"oldest_pending_ms,omitempty"`
	// Its cell's tenant (TASK-364).
	Tenant uint64 `protobuf:"varint,17,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell, whose extents its guest mints.
	Cell uint64 `protobuf:"varint,18,opt,name=cell,proto3" json:"cell,omitempty"`
	// How many extents it holds that it wrote: those whose bytes are `own` (TASK-454). What
	// the daemon holds in memory for a cell open grows with all it holds, own and inherited: a few
	// hundred bytes an extent (TASK-443.2).
	OwnExtents uint64 `protobuf:"varint,19,opt,name=own_extents,json=ownExtents,proto3" json:"own_extents,omitempty"`
	// How many it holds that it inherited: those whose bytes are `inherited`.
	InheritedExtents uint64 `protobuf:"varint,20,opt,name=inherited_extents,json=inheritedExtents,proto3" json:"inherited_extents,omitempty"`
	// Whether its cell is replicated, as its writer's taking found its head (doc-21): its f, 0 if
	// not; with `uploading` alone.
	Replicas uint32 `protobuf:"varint,22,opt,name=replicas,proto3" json:"replicas,omitempty"`
	// What its guest's fsync waits for, as its writer's taking found it.
	Fsync FsyncLevel `protobuf:"varint,23,opt,name=fsync,proto3,enum=cladefs.cladefsd.v1.FsyncLevel" json:"fsync,omitempty"`
	// The number of its active configuration, 0 if it has none yet.
	Configuration uint64 `protobuf:"varint,24,opt,name=configuration,proto3" json:"configuration,omitempty"`
	// Whether it is degraded: fewer than f members of its active configuration take its writer's
	// commits, or that configuration is of f = 0, its commits waiting for the bucket (doc-21).
	Degraded bool `protobuf:"varint,25,opt,name=degraded,proto3" json:"degraded,omitempty"`
	// How many members of its active configuration, its writer aside, take its writer's commits.
	MembersUp uint32 `protobuf:"varint,26,opt,name=members_up,json=membersUp,proto3" json:"members_up,omitempty"`
	// The f of its active configuration: less than replicas once it dropped to f = 0, degraded.
	ActiveReplicas uint32 `protobuf:"varint,27,opt,name=active_replicas,json=activeReplicas,proto3" json:"active_replicas,omitempty"`
	// If its writer's taking was forced (PlaceCell's or CreateCell's force) and fenced fewer members
	// than a take needs: the commit number of the last commit the bucket held, past which commits told
	// replicated may be lost (TASK-681); 0 if the bucket held none. Unset if not.
	ForcedAfter *uint64 `protobuf:"varint,28,opt,name=forced_after,json=forcedAfter,proto3,oneof" json:"forced_after,omitempty"`
	// Whether the cell uses Fast WAL commits backed by appendable zonal object storage (doc-29).
	FastWal bool `protobuf:"varint,29,opt,name=fast_wal,json=fastWal,proto3" json:"fast_wal,omitempty"`
	// URL of the appendable zonal bucket holding the cell's active fast WAL, empty if not using
	// Fast WAL.
	FastWalBucket string `protobuf:"bytes,30,opt,name=fast_wal_bucket,json=fastWalBucket,proto3" json:"fast_wal_bucket,omitempty"`
	// Whether the cell's fast WAL bucket is degraded and commits are falling back to cold regional
	// bucket uploads.
	FastWalDegraded bool `protobuf:"varint,31,opt,name=fast_wal_degraded,json=fastWalDegraded,proto3" json:"fast_wal_degraded,omitempty"`
	// Whether this cell is a host-local ephemeral cell (doc-25): kept on host SSD alone, never
	// uploaded to the bucket.
	Ephemeral bool `protobuf:"varint,32,opt,name=ephemeral,proto3" json:"ephemeral,omitempty"`
	// The fencing token of this host's placement of the cell, whether a VM process has it open or
	// not (board 13879): the token PlaceCell (or CreateCell's) placed it at, or, never placed, the
	// daemon's own (`CLADEFSD_TOKEN`). A host's copy is the cell's latest placement while no other
	// host's placement has a greater one. taken_token is the same, but only while uploading. 0 for
	// an ephemeral cell, which no placement holds.
	PlacementToken uint64 `protobuf:"varint,33,opt,name=placement_token,json=placementToken,proto3" json:"placement_token,omitempty"`
	unknownFields  protoimpl.UnknownFields
	sizeCache      protoimpl.SizeCache
}

func (x *StatCellResponse) Reset() {
	*x = StatCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[50]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *StatCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*StatCellResponse) ProtoMessage() {}

func (x *StatCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[50]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use StatCellResponse.ProtoReflect.Descriptor instead.
func (*StatCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{50}
}

func (x *StatCellResponse) GetCapacity() uint64 {
	if x != nil {
		return x.Capacity
	}
	return 0
}

func (x *StatCellResponse) GetUsed() uint64 {
	if x != nil {
		return x.Used
	}
	return 0
}

func (x *StatCellResponse) GetAvailable() uint64 {
	if x != nil {
		return x.Available
	}
	return 0
}

func (x *StatCellResponse) GetPublic() bool {
	if x != nil {
		return x.Public
	}
	return false
}

func (x *StatCellResponse) GetOpen() bool {
	if x != nil {
		return x.Open
	}
	return false
}

func (x *StatCellResponse) GetOwn() uint64 {
	if x != nil {
		return x.Own
	}
	return 0
}

func (x *StatCellResponse) GetInherited() uint64 {
	if x != nil {
		return x.Inherited
	}
	return 0
}

func (x *StatCellResponse) GetCommitted() uint64 {
	if x != nil {
		return x.Committed
	}
	return 0
}

func (x *StatCellResponse) GetUploading() bool {
	if x != nil {
		return x.Uploading
	}
	return false
}

func (x *StatCellResponse) GetTakenToken() uint64 {
	if x != nil {
		return x.TakenToken
	}
	return 0
}

func (x *StatCellResponse) GetTakenEpoch() uint64 {
	if x != nil {
		return x.TakenEpoch
	}
	return 0
}

func (x *StatCellResponse) GetTakenUnixMs() uint64 {
	if x != nil {
		return x.TakenUnixMs
	}
	return 0
}

func (x *StatCellResponse) GetUploaded() uint64 {
	if x != nil {
		return x.Uploaded
	}
	return 0
}

func (x *StatCellResponse) GetPendingUploadBytes() uint64 {
	if x != nil {
		return x.PendingUploadBytes
	}
	return 0
}

func (x *StatCellResponse) GetOldestPendingMs() uint64 {
	if x != nil {
		return x.OldestPendingMs
	}
	return 0
}

func (x *StatCellResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *StatCellResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *StatCellResponse) GetOwnExtents() uint64 {
	if x != nil {
		return x.OwnExtents
	}
	return 0
}

func (x *StatCellResponse) GetInheritedExtents() uint64 {
	if x != nil {
		return x.InheritedExtents
	}
	return 0
}

func (x *StatCellResponse) GetReplicas() uint32 {
	if x != nil {
		return x.Replicas
	}
	return 0
}

func (x *StatCellResponse) GetFsync() FsyncLevel {
	if x != nil {
		return x.Fsync
	}
	return FsyncLevel_FSYNC_LEVEL_ORDERED
}

func (x *StatCellResponse) GetConfiguration() uint64 {
	if x != nil {
		return x.Configuration
	}
	return 0
}

func (x *StatCellResponse) GetDegraded() bool {
	if x != nil {
		return x.Degraded
	}
	return false
}

func (x *StatCellResponse) GetMembersUp() uint32 {
	if x != nil {
		return x.MembersUp
	}
	return 0
}

func (x *StatCellResponse) GetActiveReplicas() uint32 {
	if x != nil {
		return x.ActiveReplicas
	}
	return 0
}

func (x *StatCellResponse) GetForcedAfter() uint64 {
	if x != nil && x.ForcedAfter != nil {
		return *x.ForcedAfter
	}
	return 0
}

func (x *StatCellResponse) GetFastWal() bool {
	if x != nil {
		return x.FastWal
	}
	return false
}

func (x *StatCellResponse) GetFastWalBucket() string {
	if x != nil {
		return x.FastWalBucket
	}
	return ""
}

func (x *StatCellResponse) GetFastWalDegraded() bool {
	if x != nil {
		return x.FastWalDegraded
	}
	return false
}

func (x *StatCellResponse) GetEphemeral() bool {
	if x != nil {
		return x.Ephemeral
	}
	return false
}

func (x *StatCellResponse) GetPlacementToken() uint64 {
	if x != nil {
		return x.PlacementToken
	}
	return 0
}

// Asks for the cells the daemon serves.
type ListCellsRequest struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListCellsRequest) Reset() {
	*x = ListCellsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[51]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListCellsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListCellsRequest) ProtoMessage() {}

func (x *ListCellsRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[51]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListCellsRequest.ProtoReflect.Descriptor instead.
func (*ListCellsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{51}
}

// The cells the daemon serves, in no order.
type ListCellsResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Each cell, and what it is and holds.
	Stores        []*ListedCell `protobuf:"bytes,1,rep,name=stores,proto3" json:"stores,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListCellsResponse) Reset() {
	*x = ListCellsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[52]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListCellsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListCellsResponse) ProtoMessage() {}

func (x *ListCellsResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[52]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListCellsResponse.ProtoReflect.Descriptor instead.
func (*ListCellsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{52}
}

func (x *ListCellsResponse) GetStores() []*ListedCell {
	if x != nil {
		return x.Stores
	}
	return nil
}

// A cell the daemon serves.
type ListedCell struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// What StatCell says of it.
	Stat *StatCellResponse `protobuf:"bytes,2,opt,name=stat,proto3" json:"stat,omitempty"`
	// Its tenant.
	Tenant uint64 `protobuf:"varint,3,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell ID.
	Cell uint64 `protobuf:"varint,4,opt,name=cell,proto3" json:"cell,omitempty"`
	// The managed disk ID (`DiskInfo.id`) holding its local copy on this host.
	DiskId        string `protobuf:"bytes,5,opt,name=disk_id,json=diskId,proto3" json:"disk_id,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListedCell) Reset() {
	*x = ListedCell{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[53]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListedCell) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListedCell) ProtoMessage() {}

func (x *ListedCell) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[53]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ListedCell.ProtoReflect.Descriptor instead.
func (*ListedCell) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{53}
}

func (x *ListedCell) GetStat() *StatCellResponse {
	if x != nil {
		return x.Stat
	}
	return nil
}

func (x *ListedCell) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ListedCell) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *ListedCell) GetDiskId() string {
	if x != nil {
		return x.DiskId
	}
	return ""
}

// Asks what the daemon did.
type GetCountersRequest struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetCountersRequest) Reset() {
	*x = GetCountersRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[54]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetCountersRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetCountersRequest) ProtoMessage() {}

func (x *GetCountersRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[54]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GetCountersRequest.ProtoReflect.Descriptor instead.
func (*GetCountersRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{54}
}

// What the daemon did since it started: counters, which only grow, so that a measurement is the
// difference of two.
type GetCountersResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// MAP requests of VMs' windows.
	Maps uint64 `protobuf:"varint,1,opt,name=maps,proto3" json:"maps,omitempty"`
	// The nanoseconds they took.
	MapNs uint64 `protobuf:"varint,2,opt,name=map_ns,json=mapNs,proto3" json:"map_ns,omitempty"`
	// Guests' faults on their windows resolved.
	Faults uint64 `protobuf:"varint,3,opt,name=faults,proto3" json:"faults,omitempty"`
	// The nanoseconds they took.
	FaultNs uint64 `protobuf:"varint,4,opt,name=fault_ns,json=faultNs,proto3" json:"fault_ns,omitempty"`
	// Reads of sealed extents' data into memory.
	PageIns uint64 `protobuf:"varint,5,opt,name=page_ins,json=pageIns,proto3" json:"page_ins,omitempty"`
	// The bytes they read from local cells.
	PageInBytes uint64 `protobuf:"varint,6,opt,name=page_in_bytes,json=pageInBytes,proto3" json:"page_in_bytes,omitempty"`
	// The bytes they fetched from object storage.
	FetchedBytes uint64 `protobuf:"varint,7,opt,name=fetched_bytes,json=fetchedBytes,proto3" json:"fetched_bytes,omitempty"`
	// READ requests.
	Reads uint64 `protobuf:"varint,8,opt,name=reads,proto3" json:"reads,omitempty"`
	// The bytes they read.
	ReadBytes uint64 `protobuf:"varint,9,opt,name=read_bytes,json=readBytes,proto3" json:"read_bytes,omitempty"`
	// Of those, the bytes read straight from local cells or object storage: of open extents. Sealed
	// extents' are read through the extent cache, whose page-ins count them.
	ReadStoredBytes uint64 `protobuf:"varint,10,opt,name=read_stored_bytes,json=readStoredBytes,proto3" json:"read_stored_bytes,omitempty"`
	// Parts of extents fetched into the host's cache ahead of forks' guests, as their versions' read
	// evidence predicted them, not found kept there.
	Prefetches uint64 `protobuf:"varint,11,opt,name=prefetches,proto3" json:"prefetches,omitempty"`
	// Parts of extents stored compressed, decompressed.
	Unpacks uint64 `protobuf:"varint,12,opt,name=unpacks,proto3" json:"unpacks,omitempty"`
	// What the daemon did with its bucket, which it always has (optional, as it once might not).
	ObjectStorage *ObjectStorageStats `protobuf:"bytes,13,opt,name=object_storage,json=objectStorage,proto3,oneof" json:"object_storage,omitempty"`
	// Commits: COMMIT requests, and WRITEs' barriers and commits.
	Commits uint64 `protobuf:"varint,14,opt,name=commits,proto3" json:"commits,omitempty"`
	// The nanoseconds they took, all of them.
	CommitNs uint64 `protobuf:"varint,15,opt,name=commit_ns,json=commitNs,proto3" json:"commit_ns,omitempty"`
	// Of those nanoseconds, committing the cell.
	CommitStoreNs uint64 `protobuf:"varint,18,opt,name=commit_store_ns,json=commitStoreNs,proto3" json:"commit_store_ns,omitempty"`
	// Of those, waiting for object storage: for the upload of a commit waiting for it, or for the uploads
	// to come within the lag an ordered one allows.
	CommitWaitNs uint64 `protobuf:"varint,20,opt,name=commit_wait_ns,json=commitWaitNs,proto3" json:"commit_wait_ns,omitempty"`
	// Segments cells closed and framed.
	SegmentCloses uint64 `protobuf:"varint,21,opt,name=segment_closes,json=segmentCloses,proto3" json:"segment_closes,omitempty"`
	// The nanoseconds closing them took.
	SegmentCloseNs uint64 `protobuf:"varint,22,opt,name=segment_close_ns,json=segmentCloseNs,proto3" json:"segment_close_ns,omitempty"`
	unknownFields  protoimpl.UnknownFields
	sizeCache      protoimpl.SizeCache
}

func (x *GetCountersResponse) Reset() {
	*x = GetCountersResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[55]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetCountersResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetCountersResponse) ProtoMessage() {}

func (x *GetCountersResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[55]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GetCountersResponse.ProtoReflect.Descriptor instead.
func (*GetCountersResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{55}
}

func (x *GetCountersResponse) GetMaps() uint64 {
	if x != nil {
		return x.Maps
	}
	return 0
}

func (x *GetCountersResponse) GetMapNs() uint64 {
	if x != nil {
		return x.MapNs
	}
	return 0
}

func (x *GetCountersResponse) GetFaults() uint64 {
	if x != nil {
		return x.Faults
	}
	return 0
}

func (x *GetCountersResponse) GetFaultNs() uint64 {
	if x != nil {
		return x.FaultNs
	}
	return 0
}

func (x *GetCountersResponse) GetPageIns() uint64 {
	if x != nil {
		return x.PageIns
	}
	return 0
}

func (x *GetCountersResponse) GetPageInBytes() uint64 {
	if x != nil {
		return x.PageInBytes
	}
	return 0
}

func (x *GetCountersResponse) GetFetchedBytes() uint64 {
	if x != nil {
		return x.FetchedBytes
	}
	return 0
}

func (x *GetCountersResponse) GetReads() uint64 {
	if x != nil {
		return x.Reads
	}
	return 0
}

func (x *GetCountersResponse) GetReadBytes() uint64 {
	if x != nil {
		return x.ReadBytes
	}
	return 0
}

func (x *GetCountersResponse) GetReadStoredBytes() uint64 {
	if x != nil {
		return x.ReadStoredBytes
	}
	return 0
}

func (x *GetCountersResponse) GetPrefetches() uint64 {
	if x != nil {
		return x.Prefetches
	}
	return 0
}

func (x *GetCountersResponse) GetUnpacks() uint64 {
	if x != nil {
		return x.Unpacks
	}
	return 0
}

func (x *GetCountersResponse) GetObjectStorage() *ObjectStorageStats {
	if x != nil {
		return x.ObjectStorage
	}
	return nil
}

func (x *GetCountersResponse) GetCommits() uint64 {
	if x != nil {
		return x.Commits
	}
	return 0
}

func (x *GetCountersResponse) GetCommitNs() uint64 {
	if x != nil {
		return x.CommitNs
	}
	return 0
}

func (x *GetCountersResponse) GetCommitStoreNs() uint64 {
	if x != nil {
		return x.CommitStoreNs
	}
	return 0
}

func (x *GetCountersResponse) GetCommitWaitNs() uint64 {
	if x != nil {
		return x.CommitWaitNs
	}
	return 0
}

func (x *GetCountersResponse) GetSegmentCloses() uint64 {
	if x != nil {
		return x.SegmentCloses
	}
	return 0
}

func (x *GetCountersResponse) GetSegmentCloseNs() uint64 {
	if x != nil {
		return x.SegmentCloseNs
	}
	return 0
}

// Asks for the stats of the daemon, its VM processes and their guests.
type GetStatsRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// How long to wait for the VM processes' answers, in milliseconds: 0 for 2,000; 60,000 at most.
	WaitMs uint32 `protobuf:"varint,1,opt,name=wait_ms,json=waitMs,proto3" json:"wait_ms,omitempty"`
	// Whether to leave out the guests' histograms, every GuestStats's `histograms` empty (board
	// 8581): the larger part of a guest's stats.
	LeaveOutHistograms bool `protobuf:"varint,2,opt,name=leave_out_histograms,json=leaveOutHistograms,proto3" json:"leave_out_histograms,omitempty"`
	// Whether to leave out the guests' stats, every VmStats's `guest` unset.
	LeaveOutGuests bool `protobuf:"varint,3,opt,name=leave_out_guests,json=leaveOutGuests,proto3" json:"leave_out_guests,omitempty"`
	// Whether to leave out the VM processes, `vms` empty, the daemon's own stats alone: no VM
	// process is asked, and nothing waited for.
	LeaveOutVms   bool `protobuf:"varint,4,opt,name=leave_out_vms,json=leaveOutVms,proto3" json:"leave_out_vms,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetStatsRequest) Reset() {
	*x = GetStatsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[56]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetStatsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetStatsRequest) ProtoMessage() {}

func (x *GetStatsRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[56]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GetStatsRequest.ProtoReflect.Descriptor instead.
func (*GetStatsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{56}
}

func (x *GetStatsRequest) GetWaitMs() uint32 {
	if x != nil {
		return x.WaitMs
	}
	return 0
}

func (x *GetStatsRequest) GetLeaveOutHistograms() bool {
	if x != nil {
		return x.LeaveOutHistograms
	}
	return false
}

func (x *GetStatsRequest) GetLeaveOutGuests() bool {
	if x != nil {
		return x.LeaveOutGuests
	}
	return false
}

func (x *GetStatsRequest) GetLeaveOutVms() bool {
	if x != nil {
		return x.LeaveOutVms
	}
	return false
}

// What the daemon, its VM processes and their guests counted, by the names its metrics give them.
// Counters only grow, so that a measurement is the difference of two; gauges are as of their
// taking.
type GetStatsResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// When the daemon read its own counters, its VM processes having answered or the wait being
	// over: nanoseconds since the Unix epoch.
	TakenUnixNs uint64 `protobuf:"varint,1,opt,name=taken_unix_ns,json=takenUnixNs,proto3" json:"taken_unix_ns,omitempty"`
	// The commit the daemon was built of.
	Build string `protobuf:"bytes,2,opt,name=build,proto3" json:"build,omitempty"`
	// Its counters: those its metrics name cladefsd_NAME_total, as `window_open_maps_refused`.
	Counters map[string]uint64 `protobuf:"bytes,3,rep,name=counters,proto3" json:"counters,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Its gauges: those its metrics name cladefsd_NAME.
	Gauges map[string]uint64 `protobuf:"bytes,4,rep,name=gauges,proto3" json:"gauges,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Its VM processes that opened a cell, in the order they connected.
	Vms []*VmStats `protobuf:"bytes,5,rep,name=vms,proto3" json:"vms,omitempty"`
	// The incarnation of its ephemeral cells' directory (CLADEFSD_EPHEMERAL), which their
	// references name and DecommissionHost takes (board 8521): 0 if it keeps none.
	Incarnation   uint64 `protobuf:"varint,6,opt,name=incarnation,proto3" json:"incarnation,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetStatsResponse) Reset() {
	*x = GetStatsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[57]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetStatsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetStatsResponse) ProtoMessage() {}

func (x *GetStatsResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[57]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GetStatsResponse.ProtoReflect.Descriptor instead.
func (*GetStatsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{57}
}

func (x *GetStatsResponse) GetTakenUnixNs() uint64 {
	if x != nil {
		return x.TakenUnixNs
	}
	return 0
}

func (x *GetStatsResponse) GetBuild() string {
	if x != nil {
		return x.Build
	}
	return ""
}

func (x *GetStatsResponse) GetCounters() map[string]uint64 {
	if x != nil {
		return x.Counters
	}
	return nil
}

func (x *GetStatsResponse) GetGauges() map[string]uint64 {
	if x != nil {
		return x.Gauges
	}
	return nil
}

func (x *GetStatsResponse) GetVms() []*VmStats {
	if x != nil {
		return x.Vms
	}
	return nil
}

func (x *GetStatsResponse) GetIncarnation() uint64 {
	if x != nil {
		return x.Incarnation
	}
	return 0
}

// A VM process, and its guest, as it answered, or as the daemon has it if it didn't.
type VmStats struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its connection's number, the daemon's count of connections as it connected: what tells apart
	// a cell's writers over time.
	Connection uint64 `protobuf:"varint,1,opt,name=connection,proto3" json:"connection,omitempty"`
	// Its cell.
	Cell uint64 `protobuf:"varint,2,opt,name=cell,proto3" json:"cell,omitempty"`
	// The commit it said it was built of, empty if it didn't.
	Build string `protobuf:"bytes,5,opt,name=build,proto3" json:"build,omitempty"`
	// Its counters: its window's stage operations, `stage_ops`, their nanoseconds, `stage_ns`, and
	// those that took access away from pages, `stage_overwrites` and `stage_leave_outs`.
	Counters map[string]uint64 `protobuf:"bytes,6,rep,name=counters,proto3" json:"counters,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Its gauges (TASK-111.5): the runs of pages its window has installed in its memory, each a
	// mapping the host's vm.max_map_count limits, `host_mappings`, and the most it keeps,
	// `host_mapping_budget`, if it said.
	Gauges map[string]uint64 `protobuf:"bytes,7,rep,name=gauges,proto3" json:"gauges,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Its window's, the daemon's, if it has one: `mappings`, `mapped_bytes`, `pages` and
	// `private_pages`.
	Window map[string]uint64 `protobuf:"bytes,8,rep,name=window,proto3" json:"window,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Whether it answered within the wait.
	Answered bool `protobuf:"varint,9,opt,name=answered,proto3" json:"answered,omitempty"`
	// How long after it was asked it answered, in nanoseconds: 0 if it didn't.
	AnswerNs uint64 `protobuf:"varint,10,opt,name=answer_ns,json=answerNs,proto3" json:"answer_ns,omitempty"`
	// Its guest's, if a guest answered.
	Guest *GuestStats `protobuf:"bytes,11,opt,name=guest,proto3,oneof" json:"guest,omitempty"`
	// Its cell's tenant.
	Tenant        uint64 `protobuf:"varint,12,opt,name=tenant,proto3" json:"tenant,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *VmStats) Reset() {
	*x = VmStats{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[58]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *VmStats) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*VmStats) ProtoMessage() {}

func (x *VmStats) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[58]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use VmStats.ProtoReflect.Descriptor instead.
func (*VmStats) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{58}
}

func (x *VmStats) GetConnection() uint64 {
	if x != nil {
		return x.Connection
	}
	return 0
}

func (x *VmStats) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *VmStats) GetBuild() string {
	if x != nil {
		return x.Build
	}
	return ""
}

func (x *VmStats) GetCounters() map[string]uint64 {
	if x != nil {
		return x.Counters
	}
	return nil
}

func (x *VmStats) GetGauges() map[string]uint64 {
	if x != nil {
		return x.Gauges
	}
	return nil
}

func (x *VmStats) GetWindow() map[string]uint64 {
	if x != nil {
		return x.Window
	}
	return nil
}

func (x *VmStats) GetAnswered() bool {
	if x != nil {
		return x.Answered
	}
	return false
}

func (x *VmStats) GetAnswerNs() uint64 {
	if x != nil {
		return x.AnswerNs
	}
	return 0
}

func (x *VmStats) GetGuest() *GuestStats {
	if x != nil {
		return x.Guest
	}
	return nil
}

func (x *VmStats) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

// What a guest's Fxfs counted, as its /proc/fs/cladefs/TAG says.
type GuestStats struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The commit it was built of, empty if it didn't say.
	Build string `protobuf:"bytes,1,opt,name=build,proto3" json:"build,omitempty"`
	// Its counters and gauges.
	Counters map[string]uint64 `protobuf:"bytes,2,rep,name=counters,proto3" json:"counters,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Its histograms, by name without `_hist`.
	Histograms    map[string]*Histogram `protobuf:"bytes,3,rep,name=histograms,proto3" json:"histograms,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"bytes,2,opt,name=value"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GuestStats) Reset() {
	*x = GuestStats{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[59]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GuestStats) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GuestStats) ProtoMessage() {}

func (x *GuestStats) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[59]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GuestStats.ProtoReflect.Descriptor instead.
func (*GuestStats) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{59}
}

func (x *GuestStats) GetBuild() string {
	if x != nil {
		return x.Build
	}
	return ""
}

func (x *GuestStats) GetCounters() map[string]uint64 {
	if x != nil {
		return x.Counters
	}
	return nil
}

func (x *GuestStats) GetHistograms() map[string]*Histogram {
	if x != nil {
		return x.Histograms
	}
	return nil
}

// A histogram's buckets' counts, in order.
type Histogram struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Each bucket's count.
	Buckets       []uint64 `protobuf:"varint,1,rep,packed,name=buckets,proto3" json:"buckets,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *Histogram) Reset() {
	*x = Histogram{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[60]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *Histogram) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*Histogram) ProtoMessage() {}

func (x *Histogram) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[60]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use Histogram.ProtoReflect.Descriptor instead.
func (*Histogram) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{60}
}

func (x *Histogram) GetBuckets() []uint64 {
	if x != nil {
		return x.Buckets
	}
	return nil
}

// What the daemon did with its bucket.
type ObjectStorageStats struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Requests, by kind (`get`, `get-range`, `head`, `list`, `create`, `update`, `overwrite`,
	// `delete`).
	Requests map[string]uint64 `protobuf:"bytes,1,rep,name=requests,proto3" json:"requests,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Bytes uploaded, by kind of object.
	UploadedBytes map[string]uint64 `protobuf:"bytes,2,rep,name=uploaded_bytes,json=uploadedBytes,proto3" json:"uploaded_bytes,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	// Bytes downloaded.
	DownloadedBytes uint64 `protobuf:"varint,3,opt,name=downloaded_bytes,json=downloadedBytes,proto3" json:"downloaded_bytes,omitempty"`
	// Requests that failed, unanswered, by kind, as `requests`: each is retried until the bucket
	// answers it, its error in the daemon's log (TASK-314). Not those the bucket answers with no
	// object, one there, or another version.
	Failures      map[string]uint64 `protobuf:"bytes,4,rep,name=failures,proto3" json:"failures,omitempty" protobuf_key:"bytes,1,opt,name=key" protobuf_val:"varint,2,opt,name=value"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ObjectStorageStats) Reset() {
	*x = ObjectStorageStats{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[61]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ObjectStorageStats) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ObjectStorageStats) ProtoMessage() {}

func (x *ObjectStorageStats) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[61]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ObjectStorageStats.ProtoReflect.Descriptor instead.
func (*ObjectStorageStats) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{61}
}

func (x *ObjectStorageStats) GetRequests() map[string]uint64 {
	if x != nil {
		return x.Requests
	}
	return nil
}

func (x *ObjectStorageStats) GetUploadedBytes() map[string]uint64 {
	if x != nil {
		return x.UploadedBytes
	}
	return nil
}

func (x *ObjectStorageStats) GetDownloadedBytes() uint64 {
	if x != nil {
		return x.DownloadedBytes
	}
	return 0
}

func (x *ObjectStorageStats) GetFailures() map[string]uint64 {
	if x != nil {
		return x.Failures
	}
	return nil
}

// Grants a tenant a VM socket. The daemon manages the tenant's storage across all configured
// JBOD disks automatically (hotbed question 547).
type GrantRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its ID in the bucket (TASK-364), not the default tenant's, 0 (INVALID_ARGUMENT): its VM
	// processes open cells of this tenant alone (PERMISSION_DENIED, EACCES). Its socket is named by
	// it, in base 32 (Crockford's digits, lower case, no leading zeros): `<tenant>.sock`.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The user to give its socket to, its jails': the socket is theirs and its group's alone. The
	// daemon's own user, if not given.
	Uid *uint32 `protobuf:"varint,3,opt,name=uid,proto3,oneof" json:"uid,omitempty"`
	// The group to give its socket to; the daemon's own, if not given.
	Gid           *uint32 `protobuf:"varint,4,opt,name=gid,proto3,oneof" json:"gid,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GrantRequest) Reset() {
	*x = GrantRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[62]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GrantRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GrantRequest) ProtoMessage() {}

func (x *GrantRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[62]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GrantRequest.ProtoReflect.Descriptor instead.
func (*GrantRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{62}
}

func (x *GrantRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *GrantRequest) GetUid() uint32 {
	if x != nil && x.Uid != nil {
		return *x.Uid
	}
	return 0
}

func (x *GrantRequest) GetGid() uint32 {
	if x != nil && x.Gid != nil {
		return *x.Gid
	}
	return 0
}

// The tenant's VM socket.
type GrantResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its path, on the daemon's host.
	Socket        string `protobuf:"bytes,1,opt,name=socket,proto3" json:"socket,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GrantResponse) Reset() {
	*x = GrantResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[63]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GrantResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GrantResponse) ProtoMessage() {}

func (x *GrantResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[63]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GrantResponse.ProtoReflect.Descriptor instead.
func (*GrantResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{63}
}

func (x *GrantResponse) GetSocket() string {
	if x != nil {
		return x.Socket
	}
	return ""
}

// Revokes a tenant's grant.
type RevokeRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant, granted (NOT_FOUND otherwise).
	Tenant        uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *RevokeRequest) Reset() {
	*x = RevokeRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[64]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *RevokeRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*RevokeRequest) ProtoMessage() {}

func (x *RevokeRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[64]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use RevokeRequest.ProtoReflect.Descriptor instead.
func (*RevokeRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{64}
}

func (x *RevokeRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

// The tenant's grant revoked.
type RevokeResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *RevokeResponse) Reset() {
	*x = RevokeResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[65]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *RevokeResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*RevokeResponse) ProtoMessage() {}

func (x *RevokeResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[65]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use RevokeResponse.ProtoReflect.Descriptor instead.
func (*RevokeResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{65}
}

// Limits the memory of a tenant, or of every tenant.
type SetMemoryLimitRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant whose memory to limit, alive now or granted later, until its limit is set again
	// (INVALID_ARGUMENT for the default tenant); absent for every tenant's together, the default
	// tenant's with them. A tenant revoked and granted again while its earlier VMs run has the limit
	// for each of its grants' VMs.
	Tenant *uint64 `protobuf:"varint,1,opt,name=tenant,proto3,oneof" json:"tenant,omitempty"`
	// The limit, in bytes, rounded down to 2 MiB, and at least that (INVALID_ARGUMENT otherwise);
	// absent, none.
	Bytes         *uint64 `protobuf:"varint,2,opt,name=bytes,proto3,oneof" json:"bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetMemoryLimitRequest) Reset() {
	*x = SetMemoryLimitRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[66]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryLimitRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryLimitRequest) ProtoMessage() {}

func (x *SetMemoryLimitRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[66]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetMemoryLimitRequest.ProtoReflect.Descriptor instead.
func (*SetMemoryLimitRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{66}
}

func (x *SetMemoryLimitRequest) GetTenant() uint64 {
	if x != nil && x.Tenant != nil {
		return *x.Tenant
	}
	return 0
}

func (x *SetMemoryLimitRequest) GetBytes() uint64 {
	if x != nil && x.Bytes != nil {
		return *x.Bytes
	}
	return 0
}

// The limit is set.
type SetMemoryLimitResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetMemoryLimitResponse) Reset() {
	*x = SetMemoryLimitResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[67]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryLimitResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryLimitResponse) ProtoMessage() {}

func (x *SetMemoryLimitResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[67]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetMemoryLimitResponse.ProtoReflect.Descriptor instead.
func (*SetMemoryLimitResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{67}
}

// Weighs a tenant's memory.
type SetMemoryWeightRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant, alive now or granted later, until its weight is set again (INVALID_ARGUMENT for the
	// default tenant).
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its weight: 0 for no share kept under pressure.
	Weight        uint64 `protobuf:"varint,2,opt,name=weight,proto3" json:"weight,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetMemoryWeightRequest) Reset() {
	*x = SetMemoryWeightRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[68]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryWeightRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryWeightRequest) ProtoMessage() {}

func (x *SetMemoryWeightRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[68]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetMemoryWeightRequest.ProtoReflect.Descriptor instead.
func (*SetMemoryWeightRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{68}
}

func (x *SetMemoryWeightRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *SetMemoryWeightRequest) GetWeight() uint64 {
	if x != nil {
		return x.Weight
	}
	return 0
}

// The weight is set.
type SetMemoryWeightResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetMemoryWeightResponse) Reset() {
	*x = SetMemoryWeightResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[69]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryWeightResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryWeightResponse) ProtoMessage() {}

func (x *SetMemoryWeightResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[69]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetMemoryWeightResponse.ProtoReflect.Descriptor instead.
func (*SetMemoryWeightResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{69}
}

// Sets the bytes in flight of a tenant's connections.
type SetTenantInFlightRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant whose bytes in flight to set, alive now or granted later, until they are set again
	// (INVALID_ARGUMENT for the default tenant); absent for every tenant's whose own aren't set, the
	// default tenant's with them.
	Tenant *uint64 `protobuf:"varint,1,opt,name=tenant,proto3,oneof" json:"tenant,omitempty"`
	// The bytes, a payload's most, 64 MiB, at least (INVALID_ARGUMENT otherwise); absent, every
	// tenant's, or for every tenant the default.
	Bytes         *uint64 `protobuf:"varint,2,opt,name=bytes,proto3,oneof" json:"bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetTenantInFlightRequest) Reset() {
	*x = SetTenantInFlightRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[70]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetTenantInFlightRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetTenantInFlightRequest) ProtoMessage() {}

func (x *SetTenantInFlightRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[70]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetTenantInFlightRequest.ProtoReflect.Descriptor instead.
func (*SetTenantInFlightRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{70}
}

func (x *SetTenantInFlightRequest) GetTenant() uint64 {
	if x != nil && x.Tenant != nil {
		return *x.Tenant
	}
	return 0
}

func (x *SetTenantInFlightRequest) GetBytes() uint64 {
	if x != nil && x.Bytes != nil {
		return *x.Bytes
	}
	return 0
}

// The bytes in flight are set.
type SetTenantInFlightResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetTenantInFlightResponse) Reset() {
	*x = SetTenantInFlightResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[71]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetTenantInFlightResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetTenantInFlightResponse) ProtoMessage() {}

func (x *SetTenantInFlightResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[71]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetTenantInFlightResponse.ProtoReflect.Descriptor instead.
func (*SetTenantInFlightResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{71}
}

// Evicts pages toward a target.
type ShrinkMemoryRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant whose memory to shrink, alive (NOT_FOUND otherwise); absent for every tenant's.
	Tenant *uint64 `protobuf:"varint,1,opt,name=tenant,proto3,oneof" json:"tenant,omitempty"`
	// The most bytes to hold.
	TargetBytes   uint64 `protobuf:"varint,2,opt,name=target_bytes,json=targetBytes,proto3" json:"target_bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShrinkMemoryRequest) Reset() {
	*x = ShrinkMemoryRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[72]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShrinkMemoryRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShrinkMemoryRequest) ProtoMessage() {}

func (x *ShrinkMemoryRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[72]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShrinkMemoryRequest.ProtoReflect.Descriptor instead.
func (*ShrinkMemoryRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{72}
}

func (x *ShrinkMemoryRequest) GetTenant() uint64 {
	if x != nil && x.Tenant != nil {
		return *x.Tenant
	}
	return 0
}

func (x *ShrinkMemoryRequest) GetTargetBytes() uint64 {
	if x != nil {
		return x.TargetBytes
	}
	return 0
}

// What is held once it shrank.
type ShrinkMemoryResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The bytes held: above the target if nothing more could be evicted.
	Bytes         uint64 `protobuf:"varint,1,opt,name=bytes,proto3" json:"bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShrinkMemoryResponse) Reset() {
	*x = ShrinkMemoryResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[73]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShrinkMemoryResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShrinkMemoryResponse) ProtoMessage() {}

func (x *ShrinkMemoryResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[73]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShrinkMemoryResponse.ProtoReflect.Descriptor instead.
func (*ShrinkMemoryResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{73}
}

func (x *ShrinkMemoryResponse) GetBytes() uint64 {
	if x != nil {
		return x.Bytes
	}
	return 0
}

// Limits the disk of a tenant, or of every tenant.
type SetDiskLimitRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant whose disk to limit, alive now or granted later, until its limit is set again
	// (INVALID_ARGUMENT for the default tenant); absent for every tenant's together. A tenant revoked
	// and granted again while its earlier VMs run has the limit for each of its directories.
	Tenant *uint64 `protobuf:"varint,1,opt,name=tenant,proto3,oneof" json:"tenant,omitempty"`
	// The limit, in bytes; absent, none.
	Bytes         *uint64 `protobuf:"varint,2,opt,name=bytes,proto3,oneof" json:"bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetDiskLimitRequest) Reset() {
	*x = SetDiskLimitRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[74]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskLimitRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskLimitRequest) ProtoMessage() {}

func (x *SetDiskLimitRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[74]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetDiskLimitRequest.ProtoReflect.Descriptor instead.
func (*SetDiskLimitRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{74}
}

func (x *SetDiskLimitRequest) GetTenant() uint64 {
	if x != nil && x.Tenant != nil {
		return *x.Tenant
	}
	return 0
}

func (x *SetDiskLimitRequest) GetBytes() uint64 {
	if x != nil && x.Bytes != nil {
		return *x.Bytes
	}
	return 0
}

// The limit is set.
type SetDiskLimitResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetDiskLimitResponse) Reset() {
	*x = SetDiskLimitResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[75]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskLimitResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskLimitResponse) ProtoMessage() {}

func (x *SetDiskLimitResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[75]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetDiskLimitResponse.ProtoReflect.Descriptor instead.
func (*SetDiskLimitResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{75}
}

// Weighs a tenant's disk.
type SetDiskWeightRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant, alive now or granted later, until its weight is set again (INVALID_ARGUMENT for the
	// default tenant).
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its weight: 0 for no share kept under pressure.
	Weight        uint64 `protobuf:"varint,2,opt,name=weight,proto3" json:"weight,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetDiskWeightRequest) Reset() {
	*x = SetDiskWeightRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[76]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskWeightRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskWeightRequest) ProtoMessage() {}

func (x *SetDiskWeightRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[76]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetDiskWeightRequest.ProtoReflect.Descriptor instead.
func (*SetDiskWeightRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{76}
}

func (x *SetDiskWeightRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *SetDiskWeightRequest) GetWeight() uint64 {
	if x != nil {
		return x.Weight
	}
	return 0
}

// The weight is set.
type SetDiskWeightResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetDiskWeightResponse) Reset() {
	*x = SetDiskWeightResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[77]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskWeightResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskWeightResponse) ProtoMessage() {}

func (x *SetDiskWeightResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[77]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetDiskWeightResponse.ProtoReflect.Descriptor instead.
func (*SetDiskWeightResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{77}
}

// The hosts sharing the daemon's bucket.
type SetPeersRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Each `HOST:PORT`, as it serves its peers (its CLADEFSD_PEER_ADDRESS), spelled as every
	// host spells it: this host's among them or not.
	Addresses     []string `protobuf:"bytes,1,rep,name=addresses,proto3" json:"addresses,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetPeersRequest) Reset() {
	*x = SetPeersRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[78]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetPeersRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetPeersRequest) ProtoMessage() {}

func (x *SetPeersRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[78]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetPeersRequest.ProtoReflect.Descriptor instead.
func (*SetPeersRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{78}
}

func (x *SetPeersRequest) GetAddresses() []string {
	if x != nil {
		return x.Addresses
	}
	return nil
}

// The peers are set.
type SetPeersResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SetPeersResponse) Reset() {
	*x = SetPeersResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[79]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetPeersResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetPeersResponse) ProtoMessage() {}

func (x *SetPeersResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[79]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use SetPeersResponse.ProtoReflect.Descriptor instead.
func (*SetPeersResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{79}
}

// Asks what a tenant's disk takes.
type GetTenantDiskRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant, alive (NOT_FOUND otherwise); absent for every tenant together.
	Tenant        *uint64 `protobuf:"varint,1,opt,name=tenant,proto3,oneof" json:"tenant,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetTenantDiskRequest) Reset() {
	*x = GetTenantDiskRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[80]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetTenantDiskRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetTenantDiskRequest) ProtoMessage() {}

func (x *GetTenantDiskRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[80]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GetTenantDiskRequest.ProtoReflect.Descriptor instead.
func (*GetTenantDiskRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{80}
}

func (x *GetTenantDiskRequest) GetTenant() uint64 {
	if x != nil && x.Tenant != nil {
		return *x.Tenant
	}
	return 0
}

// What a tenant's disk takes.
type GetTenantDiskResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The bytes its cells' files take: each file's length rounded up to 4 KiB, once however many
	// names it has, counted as written.
	Bytes uint64 `protobuf:"varint,1,opt,name=bytes,proto3" json:"bytes,omitempty"`
	// Its limit, if it has one.
	LimitBytes *uint64 `protobuf:"varint,2,opt,name=limit_bytes,json=limitBytes,proto3,oneof" json:"limit_bytes,omitempty"`
	// What it holds past `bytes` against its limit: what its cells' own work may still add for what
	// was written, and what writes admitted and not yet landed hold.
	ReservedBytes uint64 `protobuf:"varint,3,opt,name=reserved_bytes,json=reservedBytes,proto3" json:"reserved_bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetTenantDiskResponse) Reset() {
	*x = GetTenantDiskResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[81]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetTenantDiskResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetTenantDiskResponse) ProtoMessage() {}

func (x *GetTenantDiskResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[81]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use GetTenantDiskResponse.ProtoReflect.Descriptor instead.
func (*GetTenantDiskResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{81}
}

func (x *GetTenantDiskResponse) GetBytes() uint64 {
	if x != nil {
		return x.Bytes
	}
	return 0
}

func (x *GetTenantDiskResponse) GetLimitBytes() uint64 {
	if x != nil && x.LimitBytes != nil {
		return *x.LimitBytes
	}
	return 0
}

func (x *GetTenantDiskResponse) GetReservedBytes() uint64 {
	if x != nil {
		return x.ReservedBytes
	}
	return 0
}

// Shreds a cell.
type ShredCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant, as StatCell names it.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID in its tenant, as StatCell names it.
	Cell          uint64 `protobuf:"varint,2,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShredCellRequest) Reset() {
	*x = ShredCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[82]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredCellRequest) ProtoMessage() {}

func (x *ShredCellRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[82]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShredCellRequest.ProtoReflect.Descriptor instead.
func (*ShredCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{82}
}

func (x *ShredCellRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ShredCellRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// The cell is shredded, and this host forgot its keys.
type ShredCellResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// How many generations of the cell's KEK this call took out of its object: 0 if it was
	// shredded already.
	KeksRemoved uint32 `protobuf:"varint,1,opt,name=keks_removed,json=keksRemoved,proto3" json:"keks_removed,omitempty"`
	// What this host forgot.
	Forgotten     *Forgotten `protobuf:"bytes,2,opt,name=forgotten,proto3" json:"forgotten,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShredCellResponse) Reset() {
	*x = ShredCellResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[83]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredCellResponse) ProtoMessage() {}

func (x *ShredCellResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[83]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShredCellResponse.ProtoReflect.Descriptor instead.
func (*ShredCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{83}
}

func (x *ShredCellResponse) GetKeksRemoved() uint32 {
	if x != nil {
		return x.KeksRemoved
	}
	return 0
}

func (x *ShredCellResponse) GetForgotten() *Forgotten {
	if x != nil {
		return x.Forgotten
	}
	return nil
}

// Asks the daemon to forget a tenant's keys.
type ForgetTenantKeysRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant's ID in the bucket, as StatCell names it.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Why, for the daemon's log: e.g. that the KEK is destroyed.
	Why           string `protobuf:"bytes,2,opt,name=why,proto3" json:"why,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ForgetTenantKeysRequest) Reset() {
	*x = ForgetTenantKeysRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[84]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForgetTenantKeysRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForgetTenantKeysRequest) ProtoMessage() {}

func (x *ForgetTenantKeysRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[84]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ForgetTenantKeysRequest.ProtoReflect.Descriptor instead.
func (*ForgetTenantKeysRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{84}
}

func (x *ForgetTenantKeysRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ForgetTenantKeysRequest) GetWhy() string {
	if x != nil {
		return x.Why
	}
	return ""
}

// The daemon forgot the tenant's keys.
type ForgetTenantKeysResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// What it forgot.
	Forgotten     *Forgotten `protobuf:"bytes,1,opt,name=forgotten,proto3" json:"forgotten,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ForgetTenantKeysResponse) Reset() {
	*x = ForgetTenantKeysResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[85]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForgetTenantKeysResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForgetTenantKeysResponse) ProtoMessage() {}

func (x *ForgetTenantKeysResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[85]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ForgetTenantKeysResponse.ProtoReflect.Descriptor instead.
func (*ForgetTenantKeysResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{85}
}

func (x *ForgetTenantKeysResponse) GetForgotten() *Forgotten {
	if x != nil {
		return x.Forgotten
	}
	return nil
}

// Shreds a tenant.
type ShredTenantRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenant's ID in the bucket, as StatCell names it.
	Tenant        uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShredTenantRequest) Reset() {
	*x = ShredTenantRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[86]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredTenantRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredTenantRequest) ProtoMessage() {}

func (x *ShredTenantRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[86]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShredTenantRequest.ProtoReflect.Descriptor instead.
func (*ShredTenantRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{86}
}

func (x *ShredTenantRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

// The tenant is shredded, and this host forgot its keys.
type ShredTenantResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Whether this call put the tombstone there: false if it was shredded already.
	ShreddedNow bool `protobuf:"varint,1,opt,name=shredded_now,json=shreddedNow,proto3" json:"shredded_now,omitempty"`
	// What this host forgot.
	Forgotten     *Forgotten `protobuf:"bytes,2,opt,name=forgotten,proto3" json:"forgotten,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShredTenantResponse) Reset() {
	*x = ShredTenantResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[87]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredTenantResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredTenantResponse) ProtoMessage() {}

func (x *ShredTenantResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[87]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ShredTenantResponse.ProtoReflect.Descriptor instead.
func (*ShredTenantResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{87}
}

func (x *ShredTenantResponse) GetShreddedNow() bool {
	if x != nil {
		return x.ShreddedNow
	}
	return false
}

func (x *ShredTenantResponse) GetForgotten() *Forgotten {
	if x != nil {
		return x.Forgotten
	}
	return nil
}

// Reloads the daemon's KMS configuration.
type ReloadKmsRequest struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ReloadKmsRequest) Reset() {
	*x = ReloadKmsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[88]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ReloadKmsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ReloadKmsRequest) ProtoMessage() {}

func (x *ReloadKmsRequest) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[88]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ReloadKmsRequest.ProtoReflect.Descriptor instead.
func (*ReloadKmsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{88}
}

// What a reload of the daemon's KMS configuration changed.
type ReloadKmsResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The tenants, by their IDs in the bucket, whose KEKs' names changed, in order: an override
	// added, changed or removed.
	Renamed []uint64 `protobuf:"varint,1,rep,packed,name=renamed,proto3" json:"renamed,omitempty"`
	// The bucket KMSes whose master keys changed.
	MasterKeys    []*MasterKeysReloaded `protobuf:"bytes,2,rep,name=master_keys,json=masterKeys,proto3" json:"master_keys,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ReloadKmsResponse) Reset() {
	*x = ReloadKmsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[89]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ReloadKmsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ReloadKmsResponse) ProtoMessage() {}

func (x *ReloadKmsResponse) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[89]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use ReloadKmsResponse.ProtoReflect.Descriptor instead.
func (*ReloadKmsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{89}
}

func (x *ReloadKmsResponse) GetRenamed() []uint64 {
	if x != nil {
		return x.Renamed
	}
	return nil
}

func (x *ReloadKmsResponse) GetMasterKeys() []*MasterKeysReloaded {
	if x != nil {
		return x.MasterKeys
	}
	return nil
}

// A bucket KMS's master keys reloaded.
type MasterKeysReloaded struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its bucket's number in the registry of buckets: 0 for the daemon's own bucket, numbered none
	// yet.
	Bucket uint32 `protobuf:"varint,1,opt,name=bucket,proto3" json:"bucket,omitempty"`
	// The IDs of the master keys added, the newest first, each 16 hexadecimal digits.
	Added []string `protobuf:"bytes,2,rep,name=added,proto3" json:"added,omitempty"`
	// The IDs of the master keys removed.
	Removed       []string `protobuf:"bytes,3,rep,name=removed,proto3" json:"removed,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MasterKeysReloaded) Reset() {
	*x = MasterKeysReloaded{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[90]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MasterKeysReloaded) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MasterKeysReloaded) ProtoMessage() {}

func (x *MasterKeysReloaded) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[90]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use MasterKeysReloaded.ProtoReflect.Descriptor instead.
func (*MasterKeysReloaded) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{90}
}

func (x *MasterKeysReloaded) GetBucket() uint32 {
	if x != nil {
		return x.Bucket
	}
	return 0
}

func (x *MasterKeysReloaded) GetAdded() []string {
	if x != nil {
		return x.Added
	}
	return nil
}

func (x *MasterKeysReloaded) GetRemoved() []string {
	if x != nil {
		return x.Removed
	}
	return nil
}

// What a host forgot of a tenant's or a cell's keys.
type Forgotten struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cells open of them it halted.
	Stores uint32 `protobuf:"varint,1,opt,name=stores,proto3" json:"stores,omitempty"`
	// The cells' keys its fetchers held.
	Rings uint32 `protobuf:"varint,2,opt,name=rings,proto3" json:"rings,omitempty"`
	// The objects' tails and parts its fetchers kept in memory.
	Kept uint32 `protobuf:"varint,3,opt,name=kept,proto3" json:"kept,omitempty"`
	// The extents taken out of the cache.
	Extents uint64 `protobuf:"varint,4,opt,name=extents,proto3" json:"extents,omitempty"`
	// The bytes of their pages evicted.
	Bytes         uint64 `protobuf:"varint,5,opt,name=bytes,proto3" json:"bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *Forgotten) Reset() {
	*x = Forgotten{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[91]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *Forgotten) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*Forgotten) ProtoMessage() {}

func (x *Forgotten) ProtoReflect() protoreflect.Message {
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[91]
	if x != nil {
		ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
		if ms.LoadMessageInfo() == nil {
			ms.StoreMessageInfo(mi)
		}
		return ms
	}
	return mi.MessageOf(x)
}

// Deprecated: Use Forgotten.ProtoReflect.Descriptor instead.
func (*Forgotten) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{91}
}

func (x *Forgotten) GetStores() uint32 {
	if x != nil {
		return x.Stores
	}
	return 0
}

func (x *Forgotten) GetRings() uint32 {
	if x != nil {
		return x.Rings
	}
	return 0
}

func (x *Forgotten) GetKept() uint32 {
	if x != nil {
		return x.Kept
	}
	return 0
}

func (x *Forgotten) GetExtents() uint64 {
	if x != nil {
		return x.Extents
	}
	return 0
}

func (x *Forgotten) GetBytes() uint64 {
	if x != nil {
		return x.Bytes
	}
	return 0
}

var File_cladefs_cladefsd_v1_cladefsd_proto protoreflect.FileDescriptor

const file_cladefs_cladefsd_v1_cladefsd_proto_rawDesc = "" +
	"\n" +
	"\"cladefs/cladefsd/v1/cladefsd.proto\x12\x13cladefs.cladefsd.v1\"`\n" +
	"\x12MigrateCellRequest\x12\x12\n" +
	"\x04peer\x18\x02 \x01(\tR\x04peer\x12\x16\n" +
	"\x06tenant\x18\x04 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x05 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02J\x04\b\x03\x10\x04\"3\n" +
	"\x13MigrateCellResponse\x12\x1c\n" +
	"\tmigration\x18\x01 \x01(\x04R\tmigration\"p\n" +
	"\x13HandOverCellRequest\x12\x1d\n" +
	"\x05token\x18\x02 \x01(\x04B\x02\x18\x01H\x00R\x05token\x88\x01\x01\x12\x16\n" +
	"\x06tenant\x18\x03 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x04 \x01(\x04R\x04cellB\b\n" +
	"\x06_tokenJ\x04\b\x01\x10\x02\"\x16\n" +
	"\x14HandOverCellResponse\"I\n" +
	"\x15AbortMigrationRequest\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x18\n" +
	"\x16AbortMigrationResponse\"\x12\n" +
	"\x10ListDisksRequest\"H\n" +
	"\x11ListDisksResponse\x123\n" +
	"\x05disks\x18\x01 \x03(\v2\x1d.cladefs.cladefsd.v1.DiskInfoR\x05disks\"\xe5\x04\n" +
	"\bDiskInfo\x12\x0e\n" +
	"\x02id\x18\x01 \x01(\tR\x02id\x12\x16\n" +
	"\x06device\x18\x02 \x01(\tR\x06device\x129\n" +
	"\x05state\x18\x03 \x01(\x0e2#.cladefs.cladefsd.v1.DiskHealthModeR\x05state\x12D\n" +
	"\tplacement\x18\x04 \x01(\x0e2&.cladefs.cladefsd.v1.DiskPlacementModeR\tplacement\x12\x1d\n" +
	"\n" +
	"size_bytes\x18\x05 \x01(\x04R\tsizeBytes\x12\x1d\n" +
	"\n" +
	"free_bytes\x18\x06 \x01(\x04R\tfreeBytes\x12\x1b\n" +
	"\tcap_bytes\x18\a \x01(\x04R\bcapBytes\x12%\n" +
	"\x0eheadroom_bytes\x18\b \x01(\x04R\rheadroomBytes\x12'\n" +
	"\x0fallowance_bytes\x18\t \x01(\x04R\x0eallowanceBytes\x12\x1f\n" +
	"\vcells_bytes\x18\n" +
	" \x01(\x04R\n" +
	"cellsBytes\x12%\n" +
	"\x0ereserved_bytes\x18\v \x01(\x04R\rreservedBytes\x12\x1f\n" +
	"\vcache_bytes\x18\f \x01(\x04R\n" +
	"cacheBytes\x12&\n" +
	"\x0fcache_cap_bytes\x18\r \x01(\x04R\rcacheCapBytes\x12\x1f\n" +
	"\vspare_bytes\x18\x0e \x01(\x04R\n" +
	"spareBytes\x12%\n" +
	"\x0eunder_pressure\x18\x0f \x01(\bR\runderPressure\x12,\n" +
	"\x12dirty_budget_bytes\x18\x10 \x01(\x04R\x10dirtyBudgetBytes\"\xdc\x04\n" +
	"\x18SetDiskGovernanceRequest\x12\x0e\n" +
	"\x02id\x18\x01 \x01(\tR\x02id\x12 \n" +
	"\tcap_bytes\x18\x02 \x01(\x04H\x00R\bcapBytes\x88\x01\x01\x12\x1b\n" +
	"\tclear_cap\x18\x03 \x01(\bR\bclearCap\x12+\n" +
	"\x0fcache_cap_bytes\x18\x04 \x01(\x04H\x01R\rcacheCapBytes\x88\x01\x01\x12&\n" +
	"\x0fclear_cache_cap\x18\x05 \x01(\bR\rclearCacheCap\x12*\n" +
	"\x0eheadroom_bytes\x18\x06 \x01(\x04H\x02R\rheadroomBytes\x88\x01\x01\x12.\n" +
	"\x10headroom_percent\x18\a \x01(\rH\x03R\x0fheadroomPercent\x88\x01\x01\x12%\n" +
	"\x0eclear_headroom\x18\b \x01(\bR\rclearHeadroom\x121\n" +
	"\x12dirty_budget_bytes\x18\t \x01(\x04H\x04R\x10dirtyBudgetBytes\x88\x01\x01\x12,\n" +
	"\x12clear_dirty_budget\x18\n" +
	" \x01(\bR\x10clearDirtyBudget\x12I\n" +
	"\tplacement\x18\v \x01(\x0e2&.cladefs.cladefsd.v1.DiskPlacementModeH\x05R\tplacement\x88\x01\x01B\f\n" +
	"\n" +
	"_cap_bytesB\x12\n" +
	"\x10_cache_cap_bytesB\x11\n" +
	"\x0f_headroom_bytesB\x13\n" +
	"\x11_headroom_percentB\x15\n" +
	"\x13_dirty_budget_bytesB\f\n" +
	"\n" +
	"_placement\"\x1b\n" +
	"\x19SetDiskGovernanceResponse\"_\n" +
	"\x13WaitHandOverRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x02 \x01(\x04R\x04cell\x12\x1c\n" +
	"\tmigration\x18\x03 \x01(\x04R\tmigration\"`\n" +
	"\x14WaitHandOverResponse\x12\x1c\n" +
	"\tmigration\x18\x01 \x01(\x04R\tmigration\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cell\"O\n" +
	"\x17DecommissionHostRequest\x12 \n" +
	"\vincarnation\x18\x01 \x01(\x04R\vincarnation\x12\x12\n" +
	"\x04most\x18\x02 \x01(\x04R\x04most\"H\n" +
	"\x18DecommissionHostResponse\x12\x18\n" +
	"\aretired\x18\x01 \x01(\x04R\aretired\x12\x12\n" +
	"\x04left\x18\x02 \x01(\x04R\x04left\"\x1b\n" +
	"\x19ListEphemeralHostsRequest\"V\n" +
	"\x1aListEphemeralHostsResponse\x128\n" +
	"\x05hosts\x18\x01 \x03(\v2\".cladefs.cladefsd.v1.EphemeralHostR\x05hosts\"y\n" +
	"\rEphemeralHost\x12 \n" +
	"\vincarnation\x18\x01 \x01(\x04R\vincarnation\x12\x1e\n" +
	"\n" +
	"references\x18\x02 \x01(\x04R\n" +
	"references\x12&\n" +
	"\x0edecommissioned\x18\x03 \x01(\bR\x0edecommissioned\"\xd5\x03\n" +
	"\x11CreateCellRequest\x12\x1a\n" +
	"\bcapacity\x18\x02 \x01(\x04R\bcapacity\x12\x19\n" +
	"\x05token\x18\x04 \x01(\x04H\x00R\x05token\x88\x01\x01\x12\x16\n" +
	"\x06tenant\x18\x05 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x06 \x01(\x04R\x04cell\x121\n" +
	"\x04kind\x18\a \x01(\x0e2\x1d.cladefs.cladefsd.v1.CellKindR\x04kind\x12\x1f\n" +
	"\breplicas\x18\t \x01(\rH\x01R\breplicas\x88\x01\x01\x125\n" +
	"\x05fsync\x18\n" +
	" \x01(\x0e2\x1f.cladefs.cladefsd.v1.FsyncLevelR\x05fsync\x12\x14\n" +
	"\x05force\x18\v \x01(\bR\x05force\x121\n" +
	"\x12upload_interval_ms\x18\f \x01(\x04H\x02R\x10uploadIntervalMs\x88\x01\x01\x12\x1e\n" +
	"\bfast_wal\x18\r \x01(\bH\x03R\afastWal\x88\x01\x01\x12\x1c\n" +
	"\tephemeral\x18\x0e \x01(\bR\tephemeralB\b\n" +
	"\x06_tokenB\v\n" +
	"\t_replicasB\x15\n" +
	"\x13_upload_interval_msB\v\n" +
	"\t_fast_walJ\x04\b\x01\x10\x02J\x04\b\x03\x10\x04J\x04\b\b\x10\t\"F\n" +
	"\x12CreateCellResponse\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x82\x01\n" +
	"\x12ImportImageRequest\x12\x1a\n" +
	"\bcapacity\x18\x02 \x01(\x04R\bcapacity\x12\x14\n" +
	"\x05image\x18\x03 \x01(\tR\x05image\x12\x1c\n" +
	"\treference\x18\x04 \x01(\tR\treference\x12\x16\n" +
	"\x06tenant\x18\x05 \x01(\x04R\x06tenantJ\x04\b\x01\x10\x02\"G\n" +
	"\x13ImportImageResponse\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x88\x01\n" +
	"\x13ImportMemoryRequest\x12\x12\n" +
	"\x04file\x18\x02 \x01(\tR\x04file\x12\x16\n" +
	"\x06tenant\x18\x03 \x01(\x04R\x06tenant\x12!\n" +
	"\fmake_version\x18\x04 \x01(\bR\vmakeVersion\x12\x1c\n" +
	"\tephemeral\x18\x05 \x01(\bR\tephemeralJ\x04\b\x01\x10\x02\"\\\n" +
	"\x14ImportMemoryResponse\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x02 \x01(\x04R\x04cell\x12\x18\n" +
	"\aversion\x18\x03 \x01(\x04R\aversion\"\x87\x02\n" +
	"\x0fForkCellRequest\x12\x18\n" +
	"\aversion\x18\x06 \x01(\x04R\aversion\x12\x16\n" +
	"\x06tenant\x18\a \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\b \x01(\x04R\x04cell\x12 \n" +
	"\tephemeral\x18\t \x01(\bB\x02\x18\x01R\tephemeral\x12(\n" +
	"\rtarget_tenant\x18\n" +
	" \x01(\x04H\x00R\ftargetTenant\x88\x01\x01\x12#\n" +
	"\n" +
	"persistent\x18\v \x01(\bH\x01R\n" +
	"persistent\x88\x01\x01B\x10\n" +
	"\x0e_target_tenantB\r\n" +
	"\v_persistentJ\x04\b\x01\x10\x02J\x04\b\x02\x10\x03J\x04\b\x03\x10\x04J\x04\b\x04\x10\x05J\x04\b\x05\x10\x06\"D\n" +
	"\x10ForkCellResponse\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"E\n" +
	"\x11DeleteCellRequest\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x14\n" +
	"\x12DeleteCellResponse\"C\n" +
	"\x0fDropCellRequest\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x12\n" +
	"\x10DropCellResponse\"a\n" +
	"\x11ResizeCellRequest\x12\x1a\n" +
	"\bcapacity\x18\x02 \x01(\x04R\bcapacity\x12\x16\n" +
	"\x06tenant\x18\x03 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x04 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x14\n" +
	"\x12ResizeCellResponse\"p\n" +
	"\x10PlaceCellRequest\x12\x14\n" +
	"\x05token\x18\x02 \x01(\x04R\x05token\x12\x14\n" +
	"\x05force\x18\x03 \x01(\bR\x05force\x12\x16\n" +
	"\x06tenant\x18\x04 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x05 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x13\n" +
	"\x11PlaceCellResponse\"i\n" +
	"\x12MakeVersionRequest\x12\x1b\n" +
	"\tkeep_last\x18\x02 \x01(\rR\bkeepLast\x12\x16\n" +
	"\x06tenant\x18\x04 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x05 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02J\x04\b\x03\x10\x04\"/\n" +
	"\x13MakeVersionResponse\x12\x18\n" +
	"\aversion\x18\x01 \x01(\x04R\aversion\"G\n" +
	"\x13ListVersionsRequest\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"M\n" +
	"\aVersion\x12\x0e\n" +
	"\x02id\x18\x01 \x01(\x04R\x02id\x12\x1a\n" +
	"\buploaded\x18\x02 \x01(\bR\buploaded\x12\x16\n" +
	"\x06shared\x18\x03 \x01(\bR\x06shared\"P\n" +
	"\x14ListVersionsResponse\x128\n" +
	"\bversions\x18\x01 \x03(\v2\x1c.cladefs.cladefsd.v1.VersionR\bversions\"c\n" +
	"\x13DropVersionsRequest\x12\x1a\n" +
	"\bversions\x18\x02 \x03(\x04R\bversions\x12\x16\n" +
	"\x06tenant\x18\x03 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x04 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x16\n" +
	"\x14DropVersionsResponse\"y\n" +
	"\x13ShareVersionRequest\x12\x18\n" +
	"\aversion\x18\x02 \x01(\x04R\aversion\x12\x16\n" +
	"\x06shared\x18\x03 \x01(\bR\x06shared\x12\x16\n" +
	"\x06tenant\x18\x04 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x05 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x16\n" +
	"\x14ShareVersionResponse\"f\n" +
	"\x11MakePublicRequest\x12\x1f\n" +
	"\vattach_only\x18\x02 \x01(\bR\n" +
	"attachOnly\x12\x16\n" +
	"\x06tenant\x18\x03 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x04 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x14\n" +
	"\x12MakePublicResponse\"\x89\x01\n" +
	"\x11AttachBaseRequest\x12\x16\n" +
	"\x06tenant\x18\x03 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x04 \x01(\x04R\x04cell\x12\x1f\n" +
	"\vbase_tenant\x18\x05 \x01(\x04R\n" +
	"baseTenant\x12\x1b\n" +
	"\tbase_cell\x18\x06 \x01(\x04R\bbaseCellJ\x04\b\x01\x10\x02J\x04\b\x02\x10\x03\"\x14\n" +
	"\x12AttachBaseResponse\"C\n" +
	"\x0fSyncCellRequest\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\x12\n" +
	"\x10SyncCellResponse\"C\n" +
	"\x0fStatCellRequest\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cellJ\x04\b\x01\x10\x02\"\xaa\b\n" +
	"\x10StatCellResponse\x12\x1a\n" +
	"\bcapacity\x18\x02 \x01(\x04R\bcapacity\x12\x12\n" +
	"\x04used\x18\x03 \x01(\x04R\x04used\x12\x1c\n" +
	"\tavailable\x18\x04 \x01(\x04R\tavailable\x12\x16\n" +
	"\x06public\x18\x05 \x01(\bR\x06public\x12\x12\n" +
	"\x04open\x18\x06 \x01(\bR\x04open\x12\x10\n" +
	"\x03own\x18\a \x01(\x04R\x03own\x12\x1c\n" +
	"\tinherited\x18\b \x01(\x04R\tinherited\x12\x1c\n" +
	"\tcommitted\x18\t \x01(\x04R\tcommitted\x12\x1c\n" +
	"\tuploading\x18\n" +
	" \x01(\bR\tuploading\x12\x1f\n" +
	"\vtaken_token\x18\v \x01(\x04R\n" +
	"takenToken\x12\x1f\n" +
	"\vtaken_epoch\x18\f \x01(\x04R\n" +
	"takenEpoch\x12\"\n" +
	"\rtaken_unix_ms\x18\r \x01(\x04R\vtakenUnixMs\x12\x1a\n" +
	"\buploaded\x18\x0e \x01(\x04R\buploaded\x120\n" +
	"\x14pending_upload_bytes\x18\x0f \x01(\x04R\x12pendingUploadBytes\x12*\n" +
	"\x11oldest_pending_ms\x18\x10 \x01(\x04R\x0foldestPendingMs\x12\x16\n" +
	"\x06tenant\x18\x11 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x12 \x01(\x04R\x04cell\x12\x1f\n" +
	"\vown_extents\x18\x13 \x01(\x04R\n" +
	"ownExtents\x12+\n" +
	"\x11inherited_extents\x18\x14 \x01(\x04R\x10inheritedExtents\x12\x1a\n" +
	"\breplicas\x18\x16 \x01(\rR\breplicas\x125\n" +
	"\x05fsync\x18\x17 \x01(\x0e2\x1f.cladefs.cladefsd.v1.FsyncLevelR\x05fsync\x12$\n" +
	"\rconfiguration\x18\x18 \x01(\x04R\rconfiguration\x12\x1a\n" +
	"\bdegraded\x18\x19 \x01(\bR\bdegraded\x12\x1d\n" +
	"\n" +
	"members_up\x18\x1a \x01(\rR\tmembersUp\x12'\n" +
	"\x0factive_replicas\x18\x1b \x01(\rR\x0eactiveReplicas\x12&\n" +
	"\fforced_after\x18\x1c \x01(\x04H\x00R\vforcedAfter\x88\x01\x01\x12\x19\n" +
	"\bfast_wal\x18\x1d \x01(\bR\afastWal\x12&\n" +
	"\x0ffast_wal_bucket\x18\x1e \x01(\tR\rfastWalBucket\x12*\n" +
	"\x11fast_wal_degraded\x18\x1f \x01(\bR\x0ffastWalDegraded\x12\x1c\n" +
	"\tephemeral\x18  \x01(\bR\tephemeral\x12'\n" +
	"\x0fplacement_token\x18! \x01(\x04R\x0eplacementTokenB\x0f\n" +
	"\r_forced_afterJ\x04\b\x01\x10\x02J\x04\b\x15\x10\x16\"\x12\n" +
	"\x10ListCellsRequest\"L\n" +
	"\x11ListCellsResponse\x127\n" +
	"\x06stores\x18\x01 \x03(\v2\x1f.cladefs.cladefsd.v1.ListedCellR\x06stores\"\x92\x01\n" +
	"\n" +
	"ListedCell\x129\n" +
	"\x04stat\x18\x02 \x01(\v2%.cladefs.cladefsd.v1.StatCellResponseR\x04stat\x12\x16\n" +
	"\x06tenant\x18\x03 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x04 \x01(\x04R\x04cell\x12\x17\n" +
	"\adisk_id\x18\x05 \x01(\tR\x06diskIdJ\x04\b\x01\x10\x02\"\x14\n" +
	"\x12GetCountersRequest\"\xf9\x05\n" +
	"\x13GetCountersResponse\x12\x12\n" +
	"\x04maps\x18\x01 \x01(\x04R\x04maps\x12\x15\n" +
	"\x06map_ns\x18\x02 \x01(\x04R\x05mapNs\x12\x16\n" +
	"\x06faults\x18\x03 \x01(\x04R\x06faults\x12\x19\n" +
	"\bfault_ns\x18\x04 \x01(\x04R\afaultNs\x12\x19\n" +
	"\bpage_ins\x18\x05 \x01(\x04R\apageIns\x12\"\n" +
	"\rpage_in_bytes\x18\x06 \x01(\x04R\vpageInBytes\x12#\n" +
	"\rfetched_bytes\x18\a \x01(\x04R\ffetchedBytes\x12\x14\n" +
	"\x05reads\x18\b \x01(\x04R\x05reads\x12\x1d\n" +
	"\n" +
	"read_bytes\x18\t \x01(\x04R\treadBytes\x12*\n" +
	"\x11read_stored_bytes\x18\n" +
	" \x01(\x04R\x0freadStoredBytes\x12\x1e\n" +
	"\n" +
	"prefetches\x18\v \x01(\x04R\n" +
	"prefetches\x12\x18\n" +
	"\aunpacks\x18\f \x01(\x04R\aunpacks\x12S\n" +
	"\x0eobject_storage\x18\r \x01(\v2'.cladefs.cladefsd.v1.ObjectStorageStatsH\x00R\robjectStorage\x88\x01\x01\x12\x18\n" +
	"\acommits\x18\x0e \x01(\x04R\acommits\x12\x1b\n" +
	"\tcommit_ns\x18\x0f \x01(\x04R\bcommitNs\x12&\n" +
	"\x0fcommit_store_ns\x18\x12 \x01(\x04R\rcommitStoreNs\x12$\n" +
	"\x0ecommit_wait_ns\x18\x14 \x01(\x04R\fcommitWaitNs\x12%\n" +
	"\x0esegment_closes\x18\x15 \x01(\x04R\rsegmentCloses\x12(\n" +
	"\x10segment_close_ns\x18\x16 \x01(\x04R\x0esegmentCloseNsB\x11\n" +
	"\x0f_object_storageJ\x04\b\x10\x10\x11J\x04\b\x11\x10\x12J\x04\b\x13\x10\x14R\x0fcommit_write_nsR\x14commit_written_bytesR\x0ecommit_seal_ns\"\xaa\x01\n" +
	"\x0fGetStatsRequest\x12\x17\n" +
	"\await_ms\x18\x01 \x01(\rR\x06waitMs\x120\n" +
	"\x14leave_out_histograms\x18\x02 \x01(\bR\x12leaveOutHistograms\x12(\n" +
	"\x10leave_out_guests\x18\x03 \x01(\bR\x0eleaveOutGuests\x12\"\n" +
	"\rleave_out_vms\x18\x04 \x01(\bR\vleaveOutVms\"\xb2\x03\n" +
	"\x10GetStatsResponse\x12\"\n" +
	"\rtaken_unix_ns\x18\x01 \x01(\x04R\vtakenUnixNs\x12\x14\n" +
	"\x05build\x18\x02 \x01(\tR\x05build\x12O\n" +
	"\bcounters\x18\x03 \x03(\v23.cladefs.cladefsd.v1.GetStatsResponse.CountersEntryR\bcounters\x12I\n" +
	"\x06gauges\x18\x04 \x03(\v21.cladefs.cladefsd.v1.GetStatsResponse.GaugesEntryR\x06gauges\x12.\n" +
	"\x03vms\x18\x05 \x03(\v2\x1c.cladefs.cladefsd.v1.VmStatsR\x03vms\x12 \n" +
	"\vincarnation\x18\x06 \x01(\x04R\vincarnation\x1a;\n" +
	"\rCountersEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a9\n" +
	"\vGaugesEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\"\xf5\x04\n" +
	"\aVmStats\x12\x1e\n" +
	"\n" +
	"connection\x18\x01 \x01(\x04R\n" +
	"connection\x12\x12\n" +
	"\x04cell\x18\x02 \x01(\x04R\x04cell\x12\x14\n" +
	"\x05build\x18\x05 \x01(\tR\x05build\x12F\n" +
	"\bcounters\x18\x06 \x03(\v2*.cladefs.cladefsd.v1.VmStats.CountersEntryR\bcounters\x12@\n" +
	"\x06gauges\x18\a \x03(\v2(.cladefs.cladefsd.v1.VmStats.GaugesEntryR\x06gauges\x12@\n" +
	"\x06window\x18\b \x03(\v2(.cladefs.cladefsd.v1.VmStats.WindowEntryR\x06window\x12\x1a\n" +
	"\banswered\x18\t \x01(\bR\banswered\x12\x1b\n" +
	"\tanswer_ns\x18\n" +
	" \x01(\x04R\banswerNs\x12:\n" +
	"\x05guest\x18\v \x01(\v2\x1f.cladefs.cladefsd.v1.GuestStatsH\x00R\x05guest\x88\x01\x01\x12\x16\n" +
	"\x06tenant\x18\f \x01(\x04R\x06tenant\x1a;\n" +
	"\rCountersEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a9\n" +
	"\vGaugesEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a9\n" +
	"\vWindowEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01B\b\n" +
	"\x06_guestJ\x04\b\x03\x10\x04J\x04\b\x04\x10\x05\"\xda\x02\n" +
	"\n" +
	"GuestStats\x12\x14\n" +
	"\x05build\x18\x01 \x01(\tR\x05build\x12I\n" +
	"\bcounters\x18\x02 \x03(\v2-.cladefs.cladefsd.v1.GuestStats.CountersEntryR\bcounters\x12O\n" +
	"\n" +
	"histograms\x18\x03 \x03(\v2/.cladefs.cladefsd.v1.GuestStats.HistogramsEntryR\n" +
	"histograms\x1a;\n" +
	"\rCountersEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a]\n" +
	"\x0fHistogramsEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x124\n" +
	"\x05value\x18\x02 \x01(\v2\x1e.cladefs.cladefsd.v1.HistogramR\x05value:\x028\x01\"%\n" +
	"\tHistogram\x12\x18\n" +
	"\abuckets\x18\x01 \x03(\x04R\abuckets\"\x84\x04\n" +
	"\x12ObjectStorageStats\x12Q\n" +
	"\brequests\x18\x01 \x03(\v25.cladefs.cladefsd.v1.ObjectStorageStats.RequestsEntryR\brequests\x12a\n" +
	"\x0euploaded_bytes\x18\x02 \x03(\v2:.cladefs.cladefsd.v1.ObjectStorageStats.UploadedBytesEntryR\ruploadedBytes\x12)\n" +
	"\x10downloaded_bytes\x18\x03 \x01(\x04R\x0fdownloadedBytes\x12Q\n" +
	"\bfailures\x18\x04 \x03(\v25.cladefs.cladefsd.v1.ObjectStorageStats.FailuresEntryR\bfailures\x1a;\n" +
	"\rRequestsEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a@\n" +
	"\x12UploadedBytesEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a;\n" +
	"\rFailuresEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\"p\n" +
	"\fGrantRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x15\n" +
	"\x03uid\x18\x03 \x01(\rH\x00R\x03uid\x88\x01\x01\x12\x15\n" +
	"\x03gid\x18\x04 \x01(\rH\x01R\x03gid\x88\x01\x01B\x06\n" +
	"\x04_uidB\x06\n" +
	"\x04_gidJ\x04\b\x02\x10\x03J\x04\b\x05\x10\x06\"'\n" +
	"\rGrantResponse\x12\x16\n" +
	"\x06socket\x18\x01 \x01(\tR\x06socket\"'\n" +
	"\rRevokeRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\"\x10\n" +
	"\x0eRevokeResponse\"d\n" +
	"\x15SetMemoryLimitRequest\x12\x1b\n" +
	"\x06tenant\x18\x01 \x01(\x04H\x00R\x06tenant\x88\x01\x01\x12\x19\n" +
	"\x05bytes\x18\x02 \x01(\x04H\x01R\x05bytes\x88\x01\x01B\t\n" +
	"\a_tenantB\b\n" +
	"\x06_bytes\"\x18\n" +
	"\x16SetMemoryLimitResponse\"H\n" +
	"\x16SetMemoryWeightRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x16\n" +
	"\x06weight\x18\x02 \x01(\x04R\x06weight\"\x19\n" +
	"\x17SetMemoryWeightResponse\"g\n" +
	"\x18SetTenantInFlightRequest\x12\x1b\n" +
	"\x06tenant\x18\x01 \x01(\x04H\x00R\x06tenant\x88\x01\x01\x12\x19\n" +
	"\x05bytes\x18\x02 \x01(\x04H\x01R\x05bytes\x88\x01\x01B\t\n" +
	"\a_tenantB\b\n" +
	"\x06_bytes\"\x1b\n" +
	"\x19SetTenantInFlightResponse\"`\n" +
	"\x13ShrinkMemoryRequest\x12\x1b\n" +
	"\x06tenant\x18\x01 \x01(\x04H\x00R\x06tenant\x88\x01\x01\x12!\n" +
	"\ftarget_bytes\x18\x02 \x01(\x04R\vtargetBytesB\t\n" +
	"\a_tenant\",\n" +
	"\x14ShrinkMemoryResponse\x12\x14\n" +
	"\x05bytes\x18\x01 \x01(\x04R\x05bytes\"b\n" +
	"\x13SetDiskLimitRequest\x12\x1b\n" +
	"\x06tenant\x18\x01 \x01(\x04H\x00R\x06tenant\x88\x01\x01\x12\x19\n" +
	"\x05bytes\x18\x02 \x01(\x04H\x01R\x05bytes\x88\x01\x01B\t\n" +
	"\a_tenantB\b\n" +
	"\x06_bytes\"\x16\n" +
	"\x14SetDiskLimitResponse\"F\n" +
	"\x14SetDiskWeightRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x16\n" +
	"\x06weight\x18\x02 \x01(\x04R\x06weight\"\x17\n" +
	"\x15SetDiskWeightResponse\"/\n" +
	"\x0fSetPeersRequest\x12\x1c\n" +
	"\taddresses\x18\x01 \x03(\tR\taddresses\"\x12\n" +
	"\x10SetPeersResponse\">\n" +
	"\x14GetTenantDiskRequest\x12\x1b\n" +
	"\x06tenant\x18\x01 \x01(\x04H\x00R\x06tenant\x88\x01\x01B\t\n" +
	"\a_tenant\"\x8a\x01\n" +
	"\x15GetTenantDiskResponse\x12\x14\n" +
	"\x05bytes\x18\x01 \x01(\x04R\x05bytes\x12$\n" +
	"\vlimit_bytes\x18\x02 \x01(\x04H\x00R\n" +
	"limitBytes\x88\x01\x01\x12%\n" +
	"\x0ereserved_bytes\x18\x03 \x01(\x04R\rreservedBytesB\x0e\n" +
	"\f_limit_bytes\">\n" +
	"\x10ShredCellRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x02 \x01(\x04R\x04cell\"t\n" +
	"\x11ShredCellResponse\x12!\n" +
	"\fkeks_removed\x18\x01 \x01(\rR\vkeksRemoved\x12<\n" +
	"\tforgotten\x18\x02 \x01(\v2\x1e.cladefs.cladefsd.v1.ForgottenR\tforgotten\"C\n" +
	"\x17ForgetTenantKeysRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x10\n" +
	"\x03why\x18\x02 \x01(\tR\x03why\"X\n" +
	"\x18ForgetTenantKeysResponse\x12<\n" +
	"\tforgotten\x18\x01 \x01(\v2\x1e.cladefs.cladefsd.v1.ForgottenR\tforgotten\",\n" +
	"\x12ShredTenantRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\"v\n" +
	"\x13ShredTenantResponse\x12!\n" +
	"\fshredded_now\x18\x01 \x01(\bR\vshreddedNow\x12<\n" +
	"\tforgotten\x18\x02 \x01(\v2\x1e.cladefs.cladefsd.v1.ForgottenR\tforgotten\"\x12\n" +
	"\x10ReloadKmsRequest\"w\n" +
	"\x11ReloadKmsResponse\x12\x18\n" +
	"\arenamed\x18\x01 \x03(\x04R\arenamed\x12H\n" +
	"\vmaster_keys\x18\x02 \x03(\v2'.cladefs.cladefsd.v1.MasterKeysReloadedR\n" +
	"masterKeys\"\\\n" +
	"\x12MasterKeysReloaded\x12\x16\n" +
	"\x06bucket\x18\x01 \x01(\rR\x06bucket\x12\x14\n" +
	"\x05added\x18\x02 \x03(\tR\x05added\x12\x18\n" +
	"\aremoved\x18\x03 \x03(\tR\aremoved\"}\n" +
	"\tForgotten\x12\x16\n" +
	"\x06stores\x18\x01 \x01(\rR\x06stores\x12\x14\n" +
	"\x05rings\x18\x02 \x01(\rR\x05rings\x12\x12\n" +
	"\x04kept\x18\x03 \x01(\rR\x04kept\x12\x18\n" +
	"\aextents\x18\x04 \x01(\x04R\aextents\x12\x14\n" +
	"\x05bytes\x18\x05 \x01(\x04R\x05bytes*j\n" +
	"\x11DiskPlacementMode\x12\x1d\n" +
	"\x19DISK_PLACEMENT_READ_WRITE\x10\x00\x12\x18\n" +
	"\x14DISK_PLACEMENT_DRAIN\x10\x01\x12\x1c\n" +
	"\x18DISK_PLACEMENT_READ_ONLY\x10\x02*u\n" +
	"\x0eDiskHealthMode\x12\x16\n" +
	"\x12DISK_HEALTH_ONLINE\x10\x00\x12\x17\n" +
	"\x13DISK_HEALTH_SUSPECT\x10\x01\x12\x17\n" +
	"\x13DISK_HEALTH_OFFLINE\x10\x02\x12\x19\n" +
	"\x15DISK_HEALTH_RETURNING\x10\x03*v\n" +
	"\n" +
	"FsyncLevel\x12\x17\n" +
	"\x13FSYNC_LEVEL_ORDERED\x10\x00\x12\x19\n" +
	"\x11FSYNC_LEVEL_LOCAL\x10\x01\x1a\x02\b\x01\x12\x1a\n" +
	"\x16FSYNC_LEVEL_REPLICATED\x10\x02\x12\x18\n" +
	"\x14FSYNC_LEVEL_FAST_WAL\x10\x03*J\n" +
	"\bCellKind\x12\x12\n" +
	"\x0eCELL_KIND_FXFS\x10\x00\x12\x14\n" +
	"\x10CELL_KIND_SQLITE\x10\x01\x12\x14\n" +
	"\x10CELL_KIND_MEMORY\x10\x022\xd1\x1f\n" +
	"\bCladefsd\x12]\n" +
	"\n" +
	"CreateCell\x12&.cladefs.cladefsd.v1.CreateCellRequest\x1a'.cladefs.cladefsd.v1.CreateCellResponse\x12`\n" +
	"\vImportImage\x12'.cladefs.cladefsd.v1.ImportImageRequest\x1a(.cladefs.cladefsd.v1.ImportImageResponse\x12c\n" +
	"\fImportMemory\x12(.cladefs.cladefsd.v1.ImportMemoryRequest\x1a).cladefs.cladefsd.v1.ImportMemoryResponse\x12W\n" +
	"\bForkCell\x12$.cladefs.cladefsd.v1.ForkCellRequest\x1a%.cladefs.cladefsd.v1.ForkCellResponse\x12]\n" +
	"\n" +
	"DeleteCell\x12&.cladefs.cladefsd.v1.DeleteCellRequest\x1a'.cladefs.cladefsd.v1.DeleteCellResponse\x12W\n" +
	"\bDropCell\x12$.cladefs.cladefsd.v1.DropCellRequest\x1a%.cladefs.cladefsd.v1.DropCellResponse\x12]\n" +
	"\n" +
	"ResizeCell\x12&.cladefs.cladefsd.v1.ResizeCellRequest\x1a'.cladefs.cladefsd.v1.ResizeCellResponse\x12Z\n" +
	"\tPlaceCell\x12%.cladefs.cladefsd.v1.PlaceCellRequest\x1a&.cladefs.cladefsd.v1.PlaceCellResponse\x12]\n" +
	"\n" +
	"MakePublic\x12&.cladefs.cladefsd.v1.MakePublicRequest\x1a'.cladefs.cladefsd.v1.MakePublicResponse\x12`\n" +
	"\vMakeVersion\x12'.cladefs.cladefsd.v1.MakeVersionRequest\x1a(.cladefs.cladefsd.v1.MakeVersionResponse\x12c\n" +
	"\fListVersions\x12(.cladefs.cladefsd.v1.ListVersionsRequest\x1a).cladefs.cladefsd.v1.ListVersionsResponse\x12c\n" +
	"\fDropVersions\x12(.cladefs.cladefsd.v1.DropVersionsRequest\x1a).cladefs.cladefsd.v1.DropVersionsResponse\x12c\n" +
	"\fShareVersion\x12(.cladefs.cladefsd.v1.ShareVersionRequest\x1a).cladefs.cladefsd.v1.ShareVersionResponse\x12]\n" +
	"\n" +
	"AttachBase\x12&.cladefs.cladefsd.v1.AttachBaseRequest\x1a'.cladefs.cladefsd.v1.AttachBaseResponse\x12W\n" +
	"\bSyncCell\x12$.cladefs.cladefsd.v1.SyncCellRequest\x1a%.cladefs.cladefsd.v1.SyncCellResponse\x12W\n" +
	"\bStatCell\x12$.cladefs.cladefsd.v1.StatCellRequest\x1a%.cladefs.cladefsd.v1.StatCellResponse\x12Z\n" +
	"\tListCells\x12%.cladefs.cladefsd.v1.ListCellsRequest\x1a&.cladefs.cladefsd.v1.ListCellsResponse\x12`\n" +
	"\vGetCounters\x12'.cladefs.cladefsd.v1.GetCountersRequest\x1a(.cladefs.cladefsd.v1.GetCountersResponse\x12W\n" +
	"\bGetStats\x12$.cladefs.cladefsd.v1.GetStatsRequest\x1a%.cladefs.cladefsd.v1.GetStatsResponse\x12N\n" +
	"\x05Grant\x12!.cladefs.cladefsd.v1.GrantRequest\x1a\".cladefs.cladefsd.v1.GrantResponse\x12Q\n" +
	"\x06Revoke\x12\".cladefs.cladefsd.v1.RevokeRequest\x1a#.cladefs.cladefsd.v1.RevokeResponse\x12i\n" +
	"\x0eSetMemoryLimit\x12*.cladefs.cladefsd.v1.SetMemoryLimitRequest\x1a+.cladefs.cladefsd.v1.SetMemoryLimitResponse\x12l\n" +
	"\x0fSetMemoryWeight\x12+.cladefs.cladefsd.v1.SetMemoryWeightRequest\x1a,.cladefs.cladefsd.v1.SetMemoryWeightResponse\x12r\n" +
	"\x11SetTenantInFlight\x12-.cladefs.cladefsd.v1.SetTenantInFlightRequest\x1a..cladefs.cladefsd.v1.SetTenantInFlightResponse\x12c\n" +
	"\fShrinkMemory\x12(.cladefs.cladefsd.v1.ShrinkMemoryRequest\x1a).cladefs.cladefsd.v1.ShrinkMemoryResponse\x12c\n" +
	"\fSetDiskLimit\x12(.cladefs.cladefsd.v1.SetDiskLimitRequest\x1a).cladefs.cladefsd.v1.SetDiskLimitResponse\x12f\n" +
	"\rSetDiskWeight\x12).cladefs.cladefsd.v1.SetDiskWeightRequest\x1a*.cladefs.cladefsd.v1.SetDiskWeightResponse\x12f\n" +
	"\rGetTenantDisk\x12).cladefs.cladefsd.v1.GetTenantDiskRequest\x1a*.cladefs.cladefsd.v1.GetTenantDiskResponse\x12W\n" +
	"\bSetPeers\x12$.cladefs.cladefsd.v1.SetPeersRequest\x1a%.cladefs.cladefsd.v1.SetPeersResponse\x12Z\n" +
	"\tShredCell\x12%.cladefs.cladefsd.v1.ShredCellRequest\x1a&.cladefs.cladefsd.v1.ShredCellResponse\x12o\n" +
	"\x10ForgetTenantKeys\x12,.cladefs.cladefsd.v1.ForgetTenantKeysRequest\x1a-.cladefs.cladefsd.v1.ForgetTenantKeysResponse\x12`\n" +
	"\vShredTenant\x12'.cladefs.cladefsd.v1.ShredTenantRequest\x1a(.cladefs.cladefsd.v1.ShredTenantResponse\x12Z\n" +
	"\tReloadKms\x12%.cladefs.cladefsd.v1.ReloadKmsRequest\x1a&.cladefs.cladefsd.v1.ReloadKmsResponse\x12o\n" +
	"\x10DecommissionHost\x12,.cladefs.cladefsd.v1.DecommissionHostRequest\x1a-.cladefs.cladefsd.v1.DecommissionHostResponse\x12u\n" +
	"\x12ListEphemeralHosts\x12..cladefs.cladefsd.v1.ListEphemeralHostsRequest\x1a/.cladefs.cladefsd.v1.ListEphemeralHostsResponse\x12`\n" +
	"\vMigrateCell\x12'.cladefs.cladefsd.v1.MigrateCellRequest\x1a(.cladefs.cladefsd.v1.MigrateCellResponse\x12c\n" +
	"\fHandOverCell\x12(.cladefs.cladefsd.v1.HandOverCellRequest\x1a).cladefs.cladefsd.v1.HandOverCellResponse\x12i\n" +
	"\x0eAbortMigration\x12*.cladefs.cladefsd.v1.AbortMigrationRequest\x1a+.cladefs.cladefsd.v1.AbortMigrationResponse\x12Z\n" +
	"\tListDisks\x12%.cladefs.cladefsd.v1.ListDisksRequest\x1a&.cladefs.cladefsd.v1.ListDisksResponse\x12r\n" +
	"\x11SetDiskGovernance\x12-.cladefs.cladefsd.v1.SetDiskGovernanceRequest\x1a..cladefs.cladefsd.v1.SetDiskGovernanceResponse\x12c\n" +
	"\fWaitHandOver\x12(.cladefs.cladefsd.v1.WaitHandOverRequest\x1a).cladefs.cladefsd.v1.WaitHandOverResponseB9Z7github.com/semistrict/cladefs/go/cladefsd/v1;cladefsdv1b\x06proto3"

var (
	file_cladefs_cladefsd_v1_cladefsd_proto_rawDescOnce sync.Once
	file_cladefs_cladefsd_v1_cladefsd_proto_rawDescData []byte
)

func file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP() []byte {
	file_cladefs_cladefsd_v1_cladefsd_proto_rawDescOnce.Do(func() {
		file_cladefs_cladefsd_v1_cladefsd_proto_rawDescData = protoimpl.X.CompressGZIP(unsafe.Slice(unsafe.StringData(file_cladefs_cladefsd_v1_cladefsd_proto_rawDesc), len(file_cladefs_cladefsd_v1_cladefsd_proto_rawDesc)))
	})
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescData
}

var file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes = make([]protoimpl.EnumInfo, 4)
var file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes = make([]protoimpl.MessageInfo, 102)
var file_cladefs_cladefsd_v1_cladefsd_proto_goTypes = []any{
	(DiskPlacementMode)(0),             // 0: cladefs.cladefsd.v1.DiskPlacementMode
	(DiskHealthMode)(0),                // 1: cladefs.cladefsd.v1.DiskHealthMode
	(FsyncLevel)(0),                    // 2: cladefs.cladefsd.v1.FsyncLevel
	(CellKind)(0),                      // 3: cladefs.cladefsd.v1.CellKind
	(*MigrateCellRequest)(nil),         // 4: cladefs.cladefsd.v1.MigrateCellRequest
	(*MigrateCellResponse)(nil),        // 5: cladefs.cladefsd.v1.MigrateCellResponse
	(*HandOverCellRequest)(nil),        // 6: cladefs.cladefsd.v1.HandOverCellRequest
	(*HandOverCellResponse)(nil),       // 7: cladefs.cladefsd.v1.HandOverCellResponse
	(*AbortMigrationRequest)(nil),      // 8: cladefs.cladefsd.v1.AbortMigrationRequest
	(*AbortMigrationResponse)(nil),     // 9: cladefs.cladefsd.v1.AbortMigrationResponse
	(*ListDisksRequest)(nil),           // 10: cladefs.cladefsd.v1.ListDisksRequest
	(*ListDisksResponse)(nil),          // 11: cladefs.cladefsd.v1.ListDisksResponse
	(*DiskInfo)(nil),                   // 12: cladefs.cladefsd.v1.DiskInfo
	(*SetDiskGovernanceRequest)(nil),   // 13: cladefs.cladefsd.v1.SetDiskGovernanceRequest
	(*SetDiskGovernanceResponse)(nil),  // 14: cladefs.cladefsd.v1.SetDiskGovernanceResponse
	(*WaitHandOverRequest)(nil),        // 15: cladefs.cladefsd.v1.WaitHandOverRequest
	(*WaitHandOverResponse)(nil),       // 16: cladefs.cladefsd.v1.WaitHandOverResponse
	(*DecommissionHostRequest)(nil),    // 17: cladefs.cladefsd.v1.DecommissionHostRequest
	(*DecommissionHostResponse)(nil),   // 18: cladefs.cladefsd.v1.DecommissionHostResponse
	(*ListEphemeralHostsRequest)(nil),  // 19: cladefs.cladefsd.v1.ListEphemeralHostsRequest
	(*ListEphemeralHostsResponse)(nil), // 20: cladefs.cladefsd.v1.ListEphemeralHostsResponse
	(*EphemeralHost)(nil),              // 21: cladefs.cladefsd.v1.EphemeralHost
	(*CreateCellRequest)(nil),          // 22: cladefs.cladefsd.v1.CreateCellRequest
	(*CreateCellResponse)(nil),         // 23: cladefs.cladefsd.v1.CreateCellResponse
	(*ImportImageRequest)(nil),         // 24: cladefs.cladefsd.v1.ImportImageRequest
	(*ImportImageResponse)(nil),        // 25: cladefs.cladefsd.v1.ImportImageResponse
	(*ImportMemoryRequest)(nil),        // 26: cladefs.cladefsd.v1.ImportMemoryRequest
	(*ImportMemoryResponse)(nil),       // 27: cladefs.cladefsd.v1.ImportMemoryResponse
	(*ForkCellRequest)(nil),            // 28: cladefs.cladefsd.v1.ForkCellRequest
	(*ForkCellResponse)(nil),           // 29: cladefs.cladefsd.v1.ForkCellResponse
	(*DeleteCellRequest)(nil),          // 30: cladefs.cladefsd.v1.DeleteCellRequest
	(*DeleteCellResponse)(nil),         // 31: cladefs.cladefsd.v1.DeleteCellResponse
	(*DropCellRequest)(nil),            // 32: cladefs.cladefsd.v1.DropCellRequest
	(*DropCellResponse)(nil),           // 33: cladefs.cladefsd.v1.DropCellResponse
	(*ResizeCellRequest)(nil),          // 34: cladefs.cladefsd.v1.ResizeCellRequest
	(*ResizeCellResponse)(nil),         // 35: cladefs.cladefsd.v1.ResizeCellResponse
	(*PlaceCellRequest)(nil),           // 36: cladefs.cladefsd.v1.PlaceCellRequest
	(*PlaceCellResponse)(nil),          // 37: cladefs.cladefsd.v1.PlaceCellResponse
	(*MakeVersionRequest)(nil),         // 38: cladefs.cladefsd.v1.MakeVersionRequest
	(*MakeVersionResponse)(nil),        // 39: cladefs.cladefsd.v1.MakeVersionResponse
	(*ListVersionsRequest)(nil),        // 40: cladefs.cladefsd.v1.ListVersionsRequest
	(*Version)(nil),                    // 41: cladefs.cladefsd.v1.Version
	(*ListVersionsResponse)(nil),       // 42: cladefs.cladefsd.v1.ListVersionsResponse
	(*DropVersionsRequest)(nil),        // 43: cladefs.cladefsd.v1.DropVersionsRequest
	(*DropVersionsResponse)(nil),       // 44: cladefs.cladefsd.v1.DropVersionsResponse
	(*ShareVersionRequest)(nil),        // 45: cladefs.cladefsd.v1.ShareVersionRequest
	(*ShareVersionResponse)(nil),       // 46: cladefs.cladefsd.v1.ShareVersionResponse
	(*MakePublicRequest)(nil),          // 47: cladefs.cladefsd.v1.MakePublicRequest
	(*MakePublicResponse)(nil),         // 48: cladefs.cladefsd.v1.MakePublicResponse
	(*AttachBaseRequest)(nil),          // 49: cladefs.cladefsd.v1.AttachBaseRequest
	(*AttachBaseResponse)(nil),         // 50: cladefs.cladefsd.v1.AttachBaseResponse
	(*SyncCellRequest)(nil),            // 51: cladefs.cladefsd.v1.SyncCellRequest
	(*SyncCellResponse)(nil),           // 52: cladefs.cladefsd.v1.SyncCellResponse
	(*StatCellRequest)(nil),            // 53: cladefs.cladefsd.v1.StatCellRequest
	(*StatCellResponse)(nil),           // 54: cladefs.cladefsd.v1.StatCellResponse
	(*ListCellsRequest)(nil),           // 55: cladefs.cladefsd.v1.ListCellsRequest
	(*ListCellsResponse)(nil),          // 56: cladefs.cladefsd.v1.ListCellsResponse
	(*ListedCell)(nil),                 // 57: cladefs.cladefsd.v1.ListedCell
	(*GetCountersRequest)(nil),         // 58: cladefs.cladefsd.v1.GetCountersRequest
	(*GetCountersResponse)(nil),        // 59: cladefs.cladefsd.v1.GetCountersResponse
	(*GetStatsRequest)(nil),            // 60: cladefs.cladefsd.v1.GetStatsRequest
	(*GetStatsResponse)(nil),           // 61: cladefs.cladefsd.v1.GetStatsResponse
	(*VmStats)(nil),                    // 62: cladefs.cladefsd.v1.VmStats
	(*GuestStats)(nil),                 // 63: cladefs.cladefsd.v1.GuestStats
	(*Histogram)(nil),                  // 64: cladefs.cladefsd.v1.Histogram
	(*ObjectStorageStats)(nil),         // 65: cladefs.cladefsd.v1.ObjectStorageStats
	(*GrantRequest)(nil),               // 66: cladefs.cladefsd.v1.GrantRequest
	(*GrantResponse)(nil),              // 67: cladefs.cladefsd.v1.GrantResponse
	(*RevokeRequest)(nil),              // 68: cladefs.cladefsd.v1.RevokeRequest
	(*RevokeResponse)(nil),             // 69: cladefs.cladefsd.v1.RevokeResponse
	(*SetMemoryLimitRequest)(nil),      // 70: cladefs.cladefsd.v1.SetMemoryLimitRequest
	(*SetMemoryLimitResponse)(nil),     // 71: cladefs.cladefsd.v1.SetMemoryLimitResponse
	(*SetMemoryWeightRequest)(nil),     // 72: cladefs.cladefsd.v1.SetMemoryWeightRequest
	(*SetMemoryWeightResponse)(nil),    // 73: cladefs.cladefsd.v1.SetMemoryWeightResponse
	(*SetTenantInFlightRequest)(nil),   // 74: cladefs.cladefsd.v1.SetTenantInFlightRequest
	(*SetTenantInFlightResponse)(nil),  // 75: cladefs.cladefsd.v1.SetTenantInFlightResponse
	(*ShrinkMemoryRequest)(nil),        // 76: cladefs.cladefsd.v1.ShrinkMemoryRequest
	(*ShrinkMemoryResponse)(nil),       // 77: cladefs.cladefsd.v1.ShrinkMemoryResponse
	(*SetDiskLimitRequest)(nil),        // 78: cladefs.cladefsd.v1.SetDiskLimitRequest
	(*SetDiskLimitResponse)(nil),       // 79: cladefs.cladefsd.v1.SetDiskLimitResponse
	(*SetDiskWeightRequest)(nil),       // 80: cladefs.cladefsd.v1.SetDiskWeightRequest
	(*SetDiskWeightResponse)(nil),      // 81: cladefs.cladefsd.v1.SetDiskWeightResponse
	(*SetPeersRequest)(nil),            // 82: cladefs.cladefsd.v1.SetPeersRequest
	(*SetPeersResponse)(nil),           // 83: cladefs.cladefsd.v1.SetPeersResponse
	(*GetTenantDiskRequest)(nil),       // 84: cladefs.cladefsd.v1.GetTenantDiskRequest
	(*GetTenantDiskResponse)(nil),      // 85: cladefs.cladefsd.v1.GetTenantDiskResponse
	(*ShredCellRequest)(nil),           // 86: cladefs.cladefsd.v1.ShredCellRequest
	(*ShredCellResponse)(nil),          // 87: cladefs.cladefsd.v1.ShredCellResponse
	(*ForgetTenantKeysRequest)(nil),    // 88: cladefs.cladefsd.v1.ForgetTenantKeysRequest
	(*ForgetTenantKeysResponse)(nil),   // 89: cladefs.cladefsd.v1.ForgetTenantKeysResponse
	(*ShredTenantRequest)(nil),         // 90: cladefs.cladefsd.v1.ShredTenantRequest
	(*ShredTenantResponse)(nil),        // 91: cladefs.cladefsd.v1.ShredTenantResponse
	(*ReloadKmsRequest)(nil),           // 92: cladefs.cladefsd.v1.ReloadKmsRequest
	(*ReloadKmsResponse)(nil),          // 93: cladefs.cladefsd.v1.ReloadKmsResponse
	(*MasterKeysReloaded)(nil),         // 94: cladefs.cladefsd.v1.MasterKeysReloaded
	(*Forgotten)(nil),                  // 95: cladefs.cladefsd.v1.Forgotten
	nil,                                // 96: cladefs.cladefsd.v1.GetStatsResponse.CountersEntry
	nil,                                // 97: cladefs.cladefsd.v1.GetStatsResponse.GaugesEntry
	nil,                                // 98: cladefs.cladefsd.v1.VmStats.CountersEntry
	nil,                                // 99: cladefs.cladefsd.v1.VmStats.GaugesEntry
	nil,                                // 100: cladefs.cladefsd.v1.VmStats.WindowEntry
	nil,                                // 101: cladefs.cladefsd.v1.GuestStats.CountersEntry
	nil,                                // 102: cladefs.cladefsd.v1.GuestStats.HistogramsEntry
	nil,                                // 103: cladefs.cladefsd.v1.ObjectStorageStats.RequestsEntry
	nil,                                // 104: cladefs.cladefsd.v1.ObjectStorageStats.UploadedBytesEntry
	nil,                                // 105: cladefs.cladefsd.v1.ObjectStorageStats.FailuresEntry
}
var file_cladefs_cladefsd_v1_cladefsd_proto_depIdxs = []int32{
	12,  // 0: cladefs.cladefsd.v1.ListDisksResponse.disks:type_name -> cladefs.cladefsd.v1.DiskInfo
	1,   // 1: cladefs.cladefsd.v1.DiskInfo.state:type_name -> cladefs.cladefsd.v1.DiskHealthMode
	0,   // 2: cladefs.cladefsd.v1.DiskInfo.placement:type_name -> cladefs.cladefsd.v1.DiskPlacementMode
	0,   // 3: cladefs.cladefsd.v1.SetDiskGovernanceRequest.placement:type_name -> cladefs.cladefsd.v1.DiskPlacementMode
	21,  // 4: cladefs.cladefsd.v1.ListEphemeralHostsResponse.hosts:type_name -> cladefs.cladefsd.v1.EphemeralHost
	3,   // 5: cladefs.cladefsd.v1.CreateCellRequest.kind:type_name -> cladefs.cladefsd.v1.CellKind
	2,   // 6: cladefs.cladefsd.v1.CreateCellRequest.fsync:type_name -> cladefs.cladefsd.v1.FsyncLevel
	41,  // 7: cladefs.cladefsd.v1.ListVersionsResponse.versions:type_name -> cladefs.cladefsd.v1.Version
	2,   // 8: cladefs.cladefsd.v1.StatCellResponse.fsync:type_name -> cladefs.cladefsd.v1.FsyncLevel
	57,  // 9: cladefs.cladefsd.v1.ListCellsResponse.stores:type_name -> cladefs.cladefsd.v1.ListedCell
	54,  // 10: cladefs.cladefsd.v1.ListedCell.stat:type_name -> cladefs.cladefsd.v1.StatCellResponse
	65,  // 11: cladefs.cladefsd.v1.GetCountersResponse.object_storage:type_name -> cladefs.cladefsd.v1.ObjectStorageStats
	96,  // 12: cladefs.cladefsd.v1.GetStatsResponse.counters:type_name -> cladefs.cladefsd.v1.GetStatsResponse.CountersEntry
	97,  // 13: cladefs.cladefsd.v1.GetStatsResponse.gauges:type_name -> cladefs.cladefsd.v1.GetStatsResponse.GaugesEntry
	62,  // 14: cladefs.cladefsd.v1.GetStatsResponse.vms:type_name -> cladefs.cladefsd.v1.VmStats
	98,  // 15: cladefs.cladefsd.v1.VmStats.counters:type_name -> cladefs.cladefsd.v1.VmStats.CountersEntry
	99,  // 16: cladefs.cladefsd.v1.VmStats.gauges:type_name -> cladefs.cladefsd.v1.VmStats.GaugesEntry
	100, // 17: cladefs.cladefsd.v1.VmStats.window:type_name -> cladefs.cladefsd.v1.VmStats.WindowEntry
	63,  // 18: cladefs.cladefsd.v1.VmStats.guest:type_name -> cladefs.cladefsd.v1.GuestStats
	101, // 19: cladefs.cladefsd.v1.GuestStats.counters:type_name -> cladefs.cladefsd.v1.GuestStats.CountersEntry
	102, // 20: cladefs.cladefsd.v1.GuestStats.histograms:type_name -> cladefs.cladefsd.v1.GuestStats.HistogramsEntry
	103, // 21: cladefs.cladefsd.v1.ObjectStorageStats.requests:type_name -> cladefs.cladefsd.v1.ObjectStorageStats.RequestsEntry
	104, // 22: cladefs.cladefsd.v1.ObjectStorageStats.uploaded_bytes:type_name -> cladefs.cladefsd.v1.ObjectStorageStats.UploadedBytesEntry
	105, // 23: cladefs.cladefsd.v1.ObjectStorageStats.failures:type_name -> cladefs.cladefsd.v1.ObjectStorageStats.FailuresEntry
	95,  // 24: cladefs.cladefsd.v1.ShredCellResponse.forgotten:type_name -> cladefs.cladefsd.v1.Forgotten
	95,  // 25: cladefs.cladefsd.v1.ForgetTenantKeysResponse.forgotten:type_name -> cladefs.cladefsd.v1.Forgotten
	95,  // 26: cladefs.cladefsd.v1.ShredTenantResponse.forgotten:type_name -> cladefs.cladefsd.v1.Forgotten
	94,  // 27: cladefs.cladefsd.v1.ReloadKmsResponse.master_keys:type_name -> cladefs.cladefsd.v1.MasterKeysReloaded
	64,  // 28: cladefs.cladefsd.v1.GuestStats.HistogramsEntry.value:type_name -> cladefs.cladefsd.v1.Histogram
	22,  // 29: cladefs.cladefsd.v1.Cladefsd.CreateCell:input_type -> cladefs.cladefsd.v1.CreateCellRequest
	24,  // 30: cladefs.cladefsd.v1.Cladefsd.ImportImage:input_type -> cladefs.cladefsd.v1.ImportImageRequest
	26,  // 31: cladefs.cladefsd.v1.Cladefsd.ImportMemory:input_type -> cladefs.cladefsd.v1.ImportMemoryRequest
	28,  // 32: cladefs.cladefsd.v1.Cladefsd.ForkCell:input_type -> cladefs.cladefsd.v1.ForkCellRequest
	30,  // 33: cladefs.cladefsd.v1.Cladefsd.DeleteCell:input_type -> cladefs.cladefsd.v1.DeleteCellRequest
	32,  // 34: cladefs.cladefsd.v1.Cladefsd.DropCell:input_type -> cladefs.cladefsd.v1.DropCellRequest
	34,  // 35: cladefs.cladefsd.v1.Cladefsd.ResizeCell:input_type -> cladefs.cladefsd.v1.ResizeCellRequest
	36,  // 36: cladefs.cladefsd.v1.Cladefsd.PlaceCell:input_type -> cladefs.cladefsd.v1.PlaceCellRequest
	47,  // 37: cladefs.cladefsd.v1.Cladefsd.MakePublic:input_type -> cladefs.cladefsd.v1.MakePublicRequest
	38,  // 38: cladefs.cladefsd.v1.Cladefsd.MakeVersion:input_type -> cladefs.cladefsd.v1.MakeVersionRequest
	40,  // 39: cladefs.cladefsd.v1.Cladefsd.ListVersions:input_type -> cladefs.cladefsd.v1.ListVersionsRequest
	43,  // 40: cladefs.cladefsd.v1.Cladefsd.DropVersions:input_type -> cladefs.cladefsd.v1.DropVersionsRequest
	45,  // 41: cladefs.cladefsd.v1.Cladefsd.ShareVersion:input_type -> cladefs.cladefsd.v1.ShareVersionRequest
	49,  // 42: cladefs.cladefsd.v1.Cladefsd.AttachBase:input_type -> cladefs.cladefsd.v1.AttachBaseRequest
	51,  // 43: cladefs.cladefsd.v1.Cladefsd.SyncCell:input_type -> cladefs.cladefsd.v1.SyncCellRequest
	53,  // 44: cladefs.cladefsd.v1.Cladefsd.StatCell:input_type -> cladefs.cladefsd.v1.StatCellRequest
	55,  // 45: cladefs.cladefsd.v1.Cladefsd.ListCells:input_type -> cladefs.cladefsd.v1.ListCellsRequest
	58,  // 46: cladefs.cladefsd.v1.Cladefsd.GetCounters:input_type -> cladefs.cladefsd.v1.GetCountersRequest
	60,  // 47: cladefs.cladefsd.v1.Cladefsd.GetStats:input_type -> cladefs.cladefsd.v1.GetStatsRequest
	66,  // 48: cladefs.cladefsd.v1.Cladefsd.Grant:input_type -> cladefs.cladefsd.v1.GrantRequest
	68,  // 49: cladefs.cladefsd.v1.Cladefsd.Revoke:input_type -> cladefs.cladefsd.v1.RevokeRequest
	70,  // 50: cladefs.cladefsd.v1.Cladefsd.SetMemoryLimit:input_type -> cladefs.cladefsd.v1.SetMemoryLimitRequest
	72,  // 51: cladefs.cladefsd.v1.Cladefsd.SetMemoryWeight:input_type -> cladefs.cladefsd.v1.SetMemoryWeightRequest
	74,  // 52: cladefs.cladefsd.v1.Cladefsd.SetTenantInFlight:input_type -> cladefs.cladefsd.v1.SetTenantInFlightRequest
	76,  // 53: cladefs.cladefsd.v1.Cladefsd.ShrinkMemory:input_type -> cladefs.cladefsd.v1.ShrinkMemoryRequest
	78,  // 54: cladefs.cladefsd.v1.Cladefsd.SetDiskLimit:input_type -> cladefs.cladefsd.v1.SetDiskLimitRequest
	80,  // 55: cladefs.cladefsd.v1.Cladefsd.SetDiskWeight:input_type -> cladefs.cladefsd.v1.SetDiskWeightRequest
	84,  // 56: cladefs.cladefsd.v1.Cladefsd.GetTenantDisk:input_type -> cladefs.cladefsd.v1.GetTenantDiskRequest
	82,  // 57: cladefs.cladefsd.v1.Cladefsd.SetPeers:input_type -> cladefs.cladefsd.v1.SetPeersRequest
	86,  // 58: cladefs.cladefsd.v1.Cladefsd.ShredCell:input_type -> cladefs.cladefsd.v1.ShredCellRequest
	88,  // 59: cladefs.cladefsd.v1.Cladefsd.ForgetTenantKeys:input_type -> cladefs.cladefsd.v1.ForgetTenantKeysRequest
	90,  // 60: cladefs.cladefsd.v1.Cladefsd.ShredTenant:input_type -> cladefs.cladefsd.v1.ShredTenantRequest
	92,  // 61: cladefs.cladefsd.v1.Cladefsd.ReloadKms:input_type -> cladefs.cladefsd.v1.ReloadKmsRequest
	17,  // 62: cladefs.cladefsd.v1.Cladefsd.DecommissionHost:input_type -> cladefs.cladefsd.v1.DecommissionHostRequest
	19,  // 63: cladefs.cladefsd.v1.Cladefsd.ListEphemeralHosts:input_type -> cladefs.cladefsd.v1.ListEphemeralHostsRequest
	4,   // 64: cladefs.cladefsd.v1.Cladefsd.MigrateCell:input_type -> cladefs.cladefsd.v1.MigrateCellRequest
	6,   // 65: cladefs.cladefsd.v1.Cladefsd.HandOverCell:input_type -> cladefs.cladefsd.v1.HandOverCellRequest
	8,   // 66: cladefs.cladefsd.v1.Cladefsd.AbortMigration:input_type -> cladefs.cladefsd.v1.AbortMigrationRequest
	10,  // 67: cladefs.cladefsd.v1.Cladefsd.ListDisks:input_type -> cladefs.cladefsd.v1.ListDisksRequest
	13,  // 68: cladefs.cladefsd.v1.Cladefsd.SetDiskGovernance:input_type -> cladefs.cladefsd.v1.SetDiskGovernanceRequest
	15,  // 69: cladefs.cladefsd.v1.Cladefsd.WaitHandOver:input_type -> cladefs.cladefsd.v1.WaitHandOverRequest
	23,  // 70: cladefs.cladefsd.v1.Cladefsd.CreateCell:output_type -> cladefs.cladefsd.v1.CreateCellResponse
	25,  // 71: cladefs.cladefsd.v1.Cladefsd.ImportImage:output_type -> cladefs.cladefsd.v1.ImportImageResponse
	27,  // 72: cladefs.cladefsd.v1.Cladefsd.ImportMemory:output_type -> cladefs.cladefsd.v1.ImportMemoryResponse
	29,  // 73: cladefs.cladefsd.v1.Cladefsd.ForkCell:output_type -> cladefs.cladefsd.v1.ForkCellResponse
	31,  // 74: cladefs.cladefsd.v1.Cladefsd.DeleteCell:output_type -> cladefs.cladefsd.v1.DeleteCellResponse
	33,  // 75: cladefs.cladefsd.v1.Cladefsd.DropCell:output_type -> cladefs.cladefsd.v1.DropCellResponse
	35,  // 76: cladefs.cladefsd.v1.Cladefsd.ResizeCell:output_type -> cladefs.cladefsd.v1.ResizeCellResponse
	37,  // 77: cladefs.cladefsd.v1.Cladefsd.PlaceCell:output_type -> cladefs.cladefsd.v1.PlaceCellResponse
	48,  // 78: cladefs.cladefsd.v1.Cladefsd.MakePublic:output_type -> cladefs.cladefsd.v1.MakePublicResponse
	39,  // 79: cladefs.cladefsd.v1.Cladefsd.MakeVersion:output_type -> cladefs.cladefsd.v1.MakeVersionResponse
	42,  // 80: cladefs.cladefsd.v1.Cladefsd.ListVersions:output_type -> cladefs.cladefsd.v1.ListVersionsResponse
	44,  // 81: cladefs.cladefsd.v1.Cladefsd.DropVersions:output_type -> cladefs.cladefsd.v1.DropVersionsResponse
	46,  // 82: cladefs.cladefsd.v1.Cladefsd.ShareVersion:output_type -> cladefs.cladefsd.v1.ShareVersionResponse
	50,  // 83: cladefs.cladefsd.v1.Cladefsd.AttachBase:output_type -> cladefs.cladefsd.v1.AttachBaseResponse
	52,  // 84: cladefs.cladefsd.v1.Cladefsd.SyncCell:output_type -> cladefs.cladefsd.v1.SyncCellResponse
	54,  // 85: cladefs.cladefsd.v1.Cladefsd.StatCell:output_type -> cladefs.cladefsd.v1.StatCellResponse
	56,  // 86: cladefs.cladefsd.v1.Cladefsd.ListCells:output_type -> cladefs.cladefsd.v1.ListCellsResponse
	59,  // 87: cladefs.cladefsd.v1.Cladefsd.GetCounters:output_type -> cladefs.cladefsd.v1.GetCountersResponse
	61,  // 88: cladefs.cladefsd.v1.Cladefsd.GetStats:output_type -> cladefs.cladefsd.v1.GetStatsResponse
	67,  // 89: cladefs.cladefsd.v1.Cladefsd.Grant:output_type -> cladefs.cladefsd.v1.GrantResponse
	69,  // 90: cladefs.cladefsd.v1.Cladefsd.Revoke:output_type -> cladefs.cladefsd.v1.RevokeResponse
	71,  // 91: cladefs.cladefsd.v1.Cladefsd.SetMemoryLimit:output_type -> cladefs.cladefsd.v1.SetMemoryLimitResponse
	73,  // 92: cladefs.cladefsd.v1.Cladefsd.SetMemoryWeight:output_type -> cladefs.cladefsd.v1.SetMemoryWeightResponse
	75,  // 93: cladefs.cladefsd.v1.Cladefsd.SetTenantInFlight:output_type -> cladefs.cladefsd.v1.SetTenantInFlightResponse
	77,  // 94: cladefs.cladefsd.v1.Cladefsd.ShrinkMemory:output_type -> cladefs.cladefsd.v1.ShrinkMemoryResponse
	79,  // 95: cladefs.cladefsd.v1.Cladefsd.SetDiskLimit:output_type -> cladefs.cladefsd.v1.SetDiskLimitResponse
	81,  // 96: cladefs.cladefsd.v1.Cladefsd.SetDiskWeight:output_type -> cladefs.cladefsd.v1.SetDiskWeightResponse
	85,  // 97: cladefs.cladefsd.v1.Cladefsd.GetTenantDisk:output_type -> cladefs.cladefsd.v1.GetTenantDiskResponse
	83,  // 98: cladefs.cladefsd.v1.Cladefsd.SetPeers:output_type -> cladefs.cladefsd.v1.SetPeersResponse
	87,  // 99: cladefs.cladefsd.v1.Cladefsd.ShredCell:output_type -> cladefs.cladefsd.v1.ShredCellResponse
	89,  // 100: cladefs.cladefsd.v1.Cladefsd.ForgetTenantKeys:output_type -> cladefs.cladefsd.v1.ForgetTenantKeysResponse
	91,  // 101: cladefs.cladefsd.v1.Cladefsd.ShredTenant:output_type -> cladefs.cladefsd.v1.ShredTenantResponse
	93,  // 102: cladefs.cladefsd.v1.Cladefsd.ReloadKms:output_type -> cladefs.cladefsd.v1.ReloadKmsResponse
	18,  // 103: cladefs.cladefsd.v1.Cladefsd.DecommissionHost:output_type -> cladefs.cladefsd.v1.DecommissionHostResponse
	20,  // 104: cladefs.cladefsd.v1.Cladefsd.ListEphemeralHosts:output_type -> cladefs.cladefsd.v1.ListEphemeralHostsResponse
	5,   // 105: cladefs.cladefsd.v1.Cladefsd.MigrateCell:output_type -> cladefs.cladefsd.v1.MigrateCellResponse
	7,   // 106: cladefs.cladefsd.v1.Cladefsd.HandOverCell:output_type -> cladefs.cladefsd.v1.HandOverCellResponse
	9,   // 107: cladefs.cladefsd.v1.Cladefsd.AbortMigration:output_type -> cladefs.cladefsd.v1.AbortMigrationResponse
	11,  // 108: cladefs.cladefsd.v1.Cladefsd.ListDisks:output_type -> cladefs.cladefsd.v1.ListDisksResponse
	14,  // 109: cladefs.cladefsd.v1.Cladefsd.SetDiskGovernance:output_type -> cladefs.cladefsd.v1.SetDiskGovernanceResponse
	16,  // 110: cladefs.cladefsd.v1.Cladefsd.WaitHandOver:output_type -> cladefs.cladefsd.v1.WaitHandOverResponse
	70,  // [70:111] is the sub-list for method output_type
	29,  // [29:70] is the sub-list for method input_type
	29,  // [29:29] is the sub-list for extension type_name
	29,  // [29:29] is the sub-list for extension extendee
	0,   // [0:29] is the sub-list for field type_name
}

func init() { file_cladefs_cladefsd_v1_cladefsd_proto_init() }
func file_cladefs_cladefsd_v1_cladefsd_proto_init() {
	if File_cladefs_cladefsd_v1_cladefsd_proto != nil {
		return
	}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[2].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[9].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[18].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[24].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[50].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[55].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[58].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[62].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[66].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[70].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[72].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[74].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[80].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[81].OneofWrappers = []any{}
	type x struct{}
	out := protoimpl.TypeBuilder{
		File: protoimpl.DescBuilder{
			GoPackagePath: reflect.TypeOf(x{}).PkgPath(),
			RawDescriptor: unsafe.Slice(unsafe.StringData(file_cladefs_cladefsd_v1_cladefsd_proto_rawDesc), len(file_cladefs_cladefsd_v1_cladefsd_proto_rawDesc)),
			NumEnums:      4,
			NumMessages:   102,
			NumExtensions: 0,
			NumServices:   1,
		},
		GoTypes:           file_cladefs_cladefsd_v1_cladefsd_proto_goTypes,
		DependencyIndexes: file_cladefs_cladefsd_v1_cladefsd_proto_depIdxs,
		EnumInfos:         file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes,
		MessageInfos:      file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes,
	}.Build()
	File_cladefs_cladefsd_v1_cladefsd_proto = out.File
	file_cladefs_cladefsd_v1_cladefsd_proto_goTypes = nil
	file_cladefs_cladefsd_v1_cladefsd_proto_depIdxs = nil
}
