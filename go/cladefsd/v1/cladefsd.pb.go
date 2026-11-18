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
// VM processes connect to the tenant's own socket, through which they name stores in the
// tenant's directory alone, and hold the tenant's cached pages alone, with public bases' pages
// read-only. Guest isolation is the security boundary, which nothing relies on tenants for: every
// VM may as well be of one tenant, or of none, on the daemon's own VM socket. Grants are not kept
// across restarts of the daemon: the caller grants again.
//
// A version is a named state of a cell, made by its writer as one local commit (MakeVersion) and
// uploaded behind. Forks are made of versions, through a copy of one: a store at the version,
// which nothing writes and no writer takes, so that a restore on one host changes nothing another
// sees, and no host's copy goes stale.
//
// A VM snapshot of a tenant's VM makes a version of each of its cells, with a copy of each at the
// path the snapshot names, and restores on any number of hosts at once (TASK-154):
//
// - On the snapshot's host, once it is taken: StatStore of the copy says its `branch` and
//   `version`, kept with the snapshot. The version is in the bucket once SyncStore of the VM's
//   store returns.
// - On each host to restore it, once, before the VMM's first `PUT /snapshot/load`: CreateStore at a
//   path in the tenant's directory, of that branch and `version`, which makes a copy there.
//   ALREADY_EXISTS means a store is there, a VM process's perhaps: use it only if StatStore says
//   that branch and version. A copy needs no `token`.
// - Then each load, whose `cladefs_overrides` name the copy as `copy_on_host`, the VM's store in
//   the tenant's directory as `path_on_host`, and the tenant's socket as `cladefsd_socket`. Each
//   restore forks the copy into a new cell, on as many hosts at once as there are. Without the
//   CreateStore, it fails with ENOENT, whatever the VMM names; so does one of a version dropped
//   since.
// - Once this host restores it no more: DropStore of the copy, optional, which ends the grant here
//   and frees it. DropVersions drops the version on every host: VMs restored from it keep what
//   they inherited.
//
// A copy so granted is the tenant's to read and restore: a VM process opening one to write it
// fails with EROFS. Grant one only to the tenant whose VM snapshot made it.
//
// Shared versions (TASK-395.7): a version forks only into cells of its own tenant's, unless it is
// shared (ShareVersion): a base image's or a VM snapshot's that many tenants fork. A restore, or a
// ForkStore, of another tenant's version that isn't shared fails with PERMISSION_DENIED (EACCES),
// making nothing; each fork reads whether the version is shared, in the bucket, as it is made, so
// that unsharing it stops new forks on every host, and those made live on. A store with a writer
// forks into cells of other tenants' only if it is the default tenant's, as the daemon's own
// stores and public bases are (TASK-395.9): a ForkStore of another tenant's into another tenant's
// directory fails with PERMISSION_DENIED (EACCES), making nothing.
//
// A fork of a store with a writer is a commit of its cell, which takes its writer: such a store
// forks on one host at a time. A base, or any store, forked on several hosts is given a version
// first, and forked from a copy of it on each host, as a VM snapshot's is.
//
// Stores are named by the paths of their directories on the daemon's host, UTF-8. A store a VM
// process has open is its connection's: what would change it under the guest's feet fails with
// FAILED_PRECONDITION and errno EBUSY, unless the call says otherwise.
//
// Errors are gRPC statuses whose code follows the errno the daemon failed with, which the
// `cladefs-errno` metadata entry carries, as a decimal number (Linux's): ENOENT is NOT_FOUND,
// EEXIST ALREADY_EXISTS, EACCES and EPERM PERMISSION_DENIED, EINVAL INVALID_ARGUMENT, ENOSPC
// RESOURCE_EXHAUSTED, ETIMEDOUT DEADLINE_EXCEEDED, EBUSY, ESTALE, ENOTSUP and EROFS
// FAILED_PRECONDITION, EIO and others INTERNAL.
//
// Calls block for as long as their work takes: a sync until the store's commits are in object
// storage, an import for minutes. A caller bounds them with deadlines; one that gives up does not
// stop the work, which completes, or fails, on its own.
//
// Compatibility (TASK-50.5): v1 only grows. Calls and fields are added; none is removed,
// renamed, renumbered or given another meaning or type, and no field number is used twice (a
// field that goes is `reserved`). A field a daemon doesn't know of is zero, empty or false, which
// each says it means; a caller ignores fields it doesn't know of. A call an older daemon lacks
// fails with UNIMPLEMENTED. Errnos keep their codes. A change that breaks a caller is v2, served
// beside v1 until callers move.
//
// Cells (TASK-364, doc-20): a tenant's cell has one kind and one branch, kept under the cell's
// prefix in the bucket, and a host takes a cell whole, its branch's head with it. A branch
// is named by its tenant, its cell and its ID: `tenant`, `cell` and `branch` beside one another.
// Tenants and cells are IDs alone, which the caller keeps the meaning of: every call names a
// tenant by its ID in the bucket, `tenant`, and no tenant has a name. A granted tenant's ID is the
// one Grant named; the daemon's own socket's VM processes make stores of the default tenant, whose
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

// What a cell holds, which says what opens it (the type's name is renamed with the rest,
// TASK-508.4).
type VdiskKind int32

const (
	// An Fxfs filesystem: a VM's disk, or a host's mount's.
	VdiskKind_VDISK_KIND_FXFS VdiskKind = 0
	// A database of sqlite_extents, which its VFS alone opens.
	VdiskKind_VDISK_KIND_SQLITE VdiskKind = 1
	// A VM's memory: an extent_files file set of one file, its image (TASK-456).
	VdiskKind_VDISK_KIND_MEMORY VdiskKind = 2
)

// Enum value maps for VdiskKind.
var (
	VdiskKind_name = map[int32]string{
		0: "VDISK_KIND_FXFS",
		1: "VDISK_KIND_SQLITE",
		2: "VDISK_KIND_MEMORY",
	}
	VdiskKind_value = map[string]int32{
		"VDISK_KIND_FXFS":   0,
		"VDISK_KIND_SQLITE": 1,
		"VDISK_KIND_MEMORY": 2,
	}
)

func (x VdiskKind) Enum() *VdiskKind {
	p := new(VdiskKind)
	*p = x
	return p
}

func (x VdiskKind) String() string {
	return protoimpl.X.EnumStringOf(x.Descriptor(), protoreflect.EnumNumber(x))
}

func (VdiskKind) Descriptor() protoreflect.EnumDescriptor {
	return file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[0].Descriptor()
}

func (VdiskKind) Type() protoreflect.EnumType {
	return &file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes[0]
}

func (x VdiskKind) Number() protoreflect.EnumNumber {
	return protoreflect.EnumNumber(x)
}

// Deprecated: Use VdiskKind.Descriptor instead.
func (VdiskKind) EnumDescriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{0}
}

// Makes an empty store.
type CreateStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Where to make it: a directory that must not exist (ALREADY_EXISTS).
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// The most bytes it holds.
	Capacity uint64 `protobuf:"varint,2,opt,name=capacity,proto3" json:"capacity,omitempty"`
	// The branch kept in the daemon's bucket to take, of `cell` (NOT_FOUND, ENOENT, if it has none;
	// INVALID_ARGUMENT, EINVAL, of a cell of 0); or 0, with a cell of 0, for a new cell, its ID the
	// bucket's to give (TASK-364), which the response says.
	Branch uint64 `protobuf:"varint,3,opt,name=branch,proto3" json:"branch,omitempty"`
	// The fencing token to place it at (PlaceStore), if any: of a placement no earlier than the
	// last writer's of the branch to take, or the take fails (FAILED_PRECONDITION, ESTALE). A
	// version's copy has no writer, and needs none.
	Token *uint64 `protobuf:"varint,4,opt,name=token,proto3,oneof" json:"token,omitempty"`
	// The tenant whose cell's branch it is. Of a new cell in a granted tenant's
	// directory, that tenant's ID (Grant's `tenant`), or 0 for it; another is refused
	// (PERMISSION_DENIED, EPERM). A copy of another tenant's version there is the VM manager's to
	// give: the tenant's VM processes restore from it, each restore a cell of their tenant's, if the
	// version is shared (ShareVersion); otherwise each restore fails (PERMISSION_DENIED, EACCES).
	Tenant uint64 `protobuf:"varint,5,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell, or 0 for a new one, whose ID, unique in the daemon's bucket, is its branch's too,
	// the bucket's to give, which the response says. A cell named takes `branch`, which must be
	// named: its own (INVALID_ARGUMENT, EINVAL, otherwise).
	Cell uint64 `protobuf:"varint,6,opt,name=cell,proto3" json:"cell,omitempty"`
	// The cell's kind, which says what opens it.
	Kind VdiskKind `protobuf:"varint,7,opt,name=kind,proto3,enum=cladefs.cladefsd.v1.VdiskKind" json:"kind,omitempty"`
	// If not 0, the store is a copy of this version of `branch`, which must be named: the cell's
	// state at the version, which nothing writes, and which ForkStore forks, on as many hosts at
	// once as wanted. Its first fork waits for the cell's head to name the version.
	Version       uint64 `protobuf:"varint,8,opt,name=version,proto3" json:"version,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *CreateStoreRequest) Reset() {
	*x = CreateStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[0]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *CreateStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*CreateStoreRequest) ProtoMessage() {}

func (x *CreateStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use CreateStoreRequest.ProtoReflect.Descriptor instead.
func (*CreateStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{0}
}

func (x *CreateStoreRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *CreateStoreRequest) GetCapacity() uint64 {
	if x != nil {
		return x.Capacity
	}
	return 0
}

func (x *CreateStoreRequest) GetBranch() uint64 {
	if x != nil {
		return x.Branch
	}
	return 0
}

func (x *CreateStoreRequest) GetToken() uint64 {
	if x != nil && x.Token != nil {
		return *x.Token
	}
	return 0
}

func (x *CreateStoreRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *CreateStoreRequest) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *CreateStoreRequest) GetKind() VdiskKind {
	if x != nil {
		return x.Kind
	}
	return VdiskKind_VDISK_KIND_FXFS
}

func (x *CreateStoreRequest) GetVersion() uint64 {
	if x != nil {
		return x.Version
	}
	return 0
}

// The store made.
type CreateStoreResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its branch: the one taken, or the new one.
	Branch uint64 `protobuf:"varint,1,opt,name=branch,proto3" json:"branch,omitempty"`
	// Its tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell: the one named, or the new one.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *CreateStoreResponse) Reset() {
	*x = CreateStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[1]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *CreateStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*CreateStoreResponse) ProtoMessage() {}

func (x *CreateStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use CreateStoreResponse.ProtoReflect.Descriptor instead.
func (*CreateStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{1}
}

func (x *CreateStoreResponse) GetBranch() uint64 {
	if x != nil {
		return x.Branch
	}
	return 0
}

func (x *CreateStoreResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *CreateStoreResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// Makes a store of a container image.
type ImportImageRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Where to make it: a directory that must not exist (ALREADY_EXISTS).
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// The most bytes it holds.
	Capacity uint64 `protobuf:"varint,2,opt,name=capacity,proto3" json:"capacity,omitempty"`
	// The image, on the daemon's host: an OCI image layout (as `skopeo copy` makes, or
	// `docker save`'s tarball extracted), or a tar archive of one layer, gzipped or not (as
	// `docker export` makes).
	Image string `protobuf:"bytes,3,opt,name=image,proto3" json:"image,omitempty"`
	// The image in the layout to import, by its reference; empty for its only image.
	Reference string `protobuf:"bytes,4,opt,name=reference,proto3" json:"reference,omitempty"`
	// The tenant of the new cell whose store it is. In a granted tenant's directory,
	// that tenant's ID, or 0 for it; another is refused (PERMISSION_DENIED, EPERM).
	Tenant        uint64 `protobuf:"varint,5,opt,name=tenant,proto3" json:"tenant,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ImportImageRequest) Reset() {
	*x = ImportImageRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[2]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ImportImageRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ImportImageRequest) ProtoMessage() {}

func (x *ImportImageRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ImportImageRequest.ProtoReflect.Descriptor instead.
func (*ImportImageRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{2}
}

func (x *ImportImageRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
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

// The store made.
type ImportImageResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its branch, a new one.
	Branch uint64 `protobuf:"varint,1,opt,name=branch,proto3" json:"branch,omitempty"`
	// Its tenant.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell, a new one.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ImportImageResponse) Reset() {
	*x = ImportImageResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[3]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ImportImageResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ImportImageResponse) ProtoMessage() {}

func (x *ImportImageResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ImportImageResponse.ProtoReflect.Descriptor instead.
func (*ImportImageResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{3}
}

func (x *ImportImageResponse) GetBranch() uint64 {
	if x != nil {
		return x.Branch
	}
	return 0
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

// Forks a store.
type ForkStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store to fork.
	From string `protobuf:"bytes,1,opt,name=from,proto3" json:"from,omitempty"`
	// Where the fork goes: a directory that must not exist (ALREADY_EXISTS), made a store of its
	// own, holding the fork's branch alone. The fork is made in the daemon's bucket (backlog doc-7,
	// Forks): `from`'s commit naming the fork, once acknowledged there, gives the fork's branch its
	// record, inheriting `from`'s extents rather than copies; only then is `to` made, taking the
	// branch's state from the bucket. A fork that fails deletes `to`, and its branch in the bucket
	// if its commit was made; its host crashing part way, `to` is left unmade, which DeleteStore of
	// `to` deletes, and the fork stands or never was, as the bucket has it.
	To            string `protobuf:"bytes,2,opt,name=to,proto3" json:"to,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ForkStoreRequest) Reset() {
	*x = ForkStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[4]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForkStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForkStoreRequest) ProtoMessage() {}

func (x *ForkStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ForkStoreRequest.ProtoReflect.Descriptor instead.
func (*ForkStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{4}
}

func (x *ForkStoreRequest) GetFrom() string {
	if x != nil {
		return x.From
	}
	return ""
}

func (x *ForkStoreRequest) GetTo() string {
	if x != nil {
		return x.To
	}
	return ""
}

// The fork made.
type ForkStoreResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its branch.
	Branch uint64 `protobuf:"varint,1,opt,name=branch,proto3" json:"branch,omitempty"`
	// Its tenant: `from`'s.
	Tenant uint64 `protobuf:"varint,2,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Its cell, a new one.
	Cell          uint64 `protobuf:"varint,3,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ForkStoreResponse) Reset() {
	*x = ForkStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[5]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForkStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForkStoreResponse) ProtoMessage() {}

func (x *ForkStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ForkStoreResponse.ProtoReflect.Descriptor instead.
func (*ForkStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{5}
}

func (x *ForkStoreResponse) GetBranch() uint64 {
	if x != nil {
		return x.Branch
	}
	return 0
}

func (x *ForkStoreResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *ForkStoreResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

// Deletes a store.
type DeleteStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store, or the fork, to delete.
	Path          string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DeleteStoreRequest) Reset() {
	*x = DeleteStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[6]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DeleteStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DeleteStoreRequest) ProtoMessage() {}

func (x *DeleteStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use DeleteStoreRequest.ProtoReflect.Descriptor instead.
func (*DeleteStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{6}
}

func (x *DeleteStoreRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

// The store deleted.
type DeleteStoreResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DeleteStoreResponse) Reset() {
	*x = DeleteStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[7]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DeleteStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DeleteStoreResponse) ProtoMessage() {}

func (x *DeleteStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use DeleteStoreResponse.ProtoReflect.Descriptor instead.
func (*DeleteStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{7}
}

// Deletes this host's copy of a store.
type DropStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store, or the fork, to delete here.
	Path          string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropStoreRequest) Reset() {
	*x = DropStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[8]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropStoreRequest) ProtoMessage() {}

func (x *DropStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use DropStoreRequest.ProtoReflect.Descriptor instead.
func (*DropStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{8}
}

func (x *DropStoreRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

// This host's copy deleted.
type DropStoreResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropStoreResponse) Reset() {
	*x = DropStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[9]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropStoreResponse) ProtoMessage() {}

func (x *DropStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use DropStoreResponse.ProtoReflect.Descriptor instead.
func (*DropStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{9}
}

// Sets a store's capacity.
type ResizeStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store to resize.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// Its new capacity, in bytes.
	Capacity      uint64 `protobuf:"varint,2,opt,name=capacity,proto3" json:"capacity,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ResizeStoreRequest) Reset() {
	*x = ResizeStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[10]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ResizeStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ResizeStoreRequest) ProtoMessage() {}

func (x *ResizeStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ResizeStoreRequest.ProtoReflect.Descriptor instead.
func (*ResizeStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{10}
}

func (x *ResizeStoreRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *ResizeStoreRequest) GetCapacity() uint64 {
	if x != nil {
		return x.Capacity
	}
	return 0
}

// The store resized.
type ResizeStoreResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ResizeStoreResponse) Reset() {
	*x = ResizeStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[11]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ResizeStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ResizeStoreResponse) ProtoMessage() {}

func (x *ResizeStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ResizeStoreResponse.ProtoReflect.Descriptor instead.
func (*ResizeStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{11}
}

// Places a store.
type PlaceStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store, or the fork, to place.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// The placement's fencing token, its boot ID: greater than every earlier placement's.
	Token         uint64 `protobuf:"varint,2,opt,name=token,proto3" json:"token,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *PlaceStoreRequest) Reset() {
	*x = PlaceStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[12]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *PlaceStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*PlaceStoreRequest) ProtoMessage() {}

func (x *PlaceStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use PlaceStoreRequest.ProtoReflect.Descriptor instead.
func (*PlaceStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{12}
}

func (x *PlaceStoreRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *PlaceStoreRequest) GetToken() uint64 {
	if x != nil {
		return x.Token
	}
	return 0
}

// The store placed.
type PlaceStoreResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *PlaceStoreResponse) Reset() {
	*x = PlaceStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[13]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *PlaceStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*PlaceStoreResponse) ProtoMessage() {}

func (x *PlaceStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use PlaceStoreResponse.ProtoReflect.Descriptor instead.
func (*PlaceStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{13}
}

// Makes a version of a store's cell.
type MakeVersionRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store, a VM's or not.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// If not 0, how many versions of the cell to keep, this one the newest: the rest are dropped
	// once the head names it.
	KeepLast uint32 `protobuf:"varint,2,opt,name=keep_last,json=keepLast,proto3" json:"keep_last,omitempty"`
	// If not empty, where to make a copy of the version on this host, which must not exist
	// (ALREADY_EXISTS), for ForkStore to fork.
	Copy          string `protobuf:"bytes,3,opt,name=copy,proto3" json:"copy,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MakeVersionRequest) Reset() {
	*x = MakeVersionRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[14]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakeVersionRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakeVersionRequest) ProtoMessage() {}

func (x *MakeVersionRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use MakeVersionRequest.ProtoReflect.Descriptor instead.
func (*MakeVersionRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{14}
}

func (x *MakeVersionRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *MakeVersionRequest) GetKeepLast() uint32 {
	if x != nil {
		return x.KeepLast
	}
	return 0
}

func (x *MakeVersionRequest) GetCopy() string {
	if x != nil {
		return x.Copy
	}
	return ""
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[15]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakeVersionResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakeVersionResponse) ProtoMessage() {}

func (x *MakeVersionResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use MakeVersionResponse.ProtoReflect.Descriptor instead.
func (*MakeVersionResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{15}
}

func (x *MakeVersionResponse) GetVersion() uint64 {
	if x != nil {
		return x.Version
	}
	return 0
}

// Lists the versions of a store's cell.
type ListVersionsRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store.
	Path          string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListVersionsRequest) Reset() {
	*x = ListVersionsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[16]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListVersionsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListVersionsRequest) ProtoMessage() {}

func (x *ListVersionsRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ListVersionsRequest.ProtoReflect.Descriptor instead.
func (*ListVersionsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{16}
}

func (x *ListVersionsRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[17]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *Version) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*Version) ProtoMessage() {}

func (x *Version) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use Version.ProtoReflect.Descriptor instead.
func (*Version) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{17}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[18]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListVersionsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListVersionsResponse) ProtoMessage() {}

func (x *ListVersionsResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ListVersionsResponse.ProtoReflect.Descriptor instead.
func (*ListVersionsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{18}
}

func (x *ListVersionsResponse) GetVersions() []*Version {
	if x != nil {
		return x.Versions
	}
	return nil
}

// Drops versions of a store's cell.
type DropVersionsRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// The versions to drop.
	Versions      []uint64 `protobuf:"varint,2,rep,packed,name=versions,proto3" json:"versions,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropVersionsRequest) Reset() {
	*x = DropVersionsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[19]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropVersionsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropVersionsRequest) ProtoMessage() {}

func (x *DropVersionsRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use DropVersionsRequest.ProtoReflect.Descriptor instead.
func (*DropVersionsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{19}
}

func (x *DropVersionsRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *DropVersionsRequest) GetVersions() []uint64 {
	if x != nil {
		return x.Versions
	}
	return nil
}

// The versions dropped.
type DropVersionsResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *DropVersionsResponse) Reset() {
	*x = DropVersionsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[20]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *DropVersionsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*DropVersionsResponse) ProtoMessage() {}

func (x *DropVersionsResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use DropVersionsResponse.ProtoReflect.Descriptor instead.
func (*DropVersionsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{20}
}

// Shares a version, or no longer.
type ShareVersionRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store whose cell's version it is.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// The version, uploaded.
	Version uint64 `protobuf:"varint,2,opt,name=version,proto3" json:"version,omitempty"`
	// Whether other tenants' forks of it are made from now on.
	Shared        bool `protobuf:"varint,3,opt,name=shared,proto3" json:"shared,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShareVersionRequest) Reset() {
	*x = ShareVersionRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[21]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShareVersionRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShareVersionRequest) ProtoMessage() {}

func (x *ShareVersionRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShareVersionRequest.ProtoReflect.Descriptor instead.
func (*ShareVersionRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{21}
}

func (x *ShareVersionRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
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

// The version shared, or no longer.
type ShareVersionResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShareVersionResponse) Reset() {
	*x = ShareVersionResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[22]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShareVersionResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShareVersionResponse) ProtoMessage() {}

func (x *ShareVersionResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShareVersionResponse.ProtoReflect.Descriptor instead.
func (*ShareVersionResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{22}
}

// Makes a store a public base.
type MakePublicRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store, which no VM may have open.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// Its branch alone, not those it inherited: a fork pinning a state of a store that goes on being
	// written, to attach to stores (AttachBase), not to fork.
	AttachOnly    bool `protobuf:"varint,2,opt,name=attach_only,json=attachOnly,proto3" json:"attach_only,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MakePublicRequest) Reset() {
	*x = MakePublicRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[23]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakePublicRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakePublicRequest) ProtoMessage() {}

func (x *MakePublicRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use MakePublicRequest.ProtoReflect.Descriptor instead.
func (*MakePublicRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{23}
}

func (x *MakePublicRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *MakePublicRequest) GetAttachOnly() bool {
	if x != nil {
		return x.AttachOnly
	}
	return false
}

// The store made public.
type MakePublicResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *MakePublicResponse) Reset() {
	*x = MakePublicResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[24]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *MakePublicResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*MakePublicResponse) ProtoMessage() {}

func (x *MakePublicResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use MakePublicResponse.ProtoReflect.Descriptor instead.
func (*MakePublicResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{24}
}

// Attaches a fork to a store.
type AttachBaseRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store to attach it to.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// The fork.
	Base          string `protobuf:"bytes,2,opt,name=base,proto3" json:"base,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *AttachBaseRequest) Reset() {
	*x = AttachBaseRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[25]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *AttachBaseRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*AttachBaseRequest) ProtoMessage() {}

func (x *AttachBaseRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use AttachBaseRequest.ProtoReflect.Descriptor instead.
func (*AttachBaseRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{25}
}

func (x *AttachBaseRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *AttachBaseRequest) GetBase() string {
	if x != nil {
		return x.Base
	}
	return ""
}

// Attached, durably.
type AttachBaseResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *AttachBaseResponse) Reset() {
	*x = AttachBaseResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[26]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *AttachBaseResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*AttachBaseResponse) ProtoMessage() {}

func (x *AttachBaseResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use AttachBaseResponse.ProtoReflect.Descriptor instead.
func (*AttachBaseResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{26}
}

// Waits for a store's commits to be in object storage.
type SyncStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store to wait for.
	Path          string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SyncStoreRequest) Reset() {
	*x = SyncStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[27]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SyncStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SyncStoreRequest) ProtoMessage() {}

func (x *SyncStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SyncStoreRequest.ProtoReflect.Descriptor instead.
func (*SyncStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{27}
}

func (x *SyncStoreRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

// The store's commits, as of the call, are in object storage.
type SyncStoreResponse struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *SyncStoreResponse) Reset() {
	*x = SyncStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[28]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SyncStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SyncStoreResponse) ProtoMessage() {}

func (x *SyncStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SyncStoreResponse.ProtoReflect.Descriptor instead.
func (*SyncStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{28}
}

// Asks what a store is, and holds.
type StatStoreRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The store to look at.
	Path          string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *StatStoreRequest) Reset() {
	*x = StatStoreRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[29]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *StatStoreRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*StatStoreRequest) ProtoMessage() {}

func (x *StatStoreRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use StatStoreRequest.ProtoReflect.Descriptor instead.
func (*StatStoreRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{29}
}

func (x *StatStoreRequest) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

// What a store is, and holds.
type StatStoreResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The branch whose extents its guest mints.
	Branch uint64 `protobuf:"varint,1,opt,name=branch,proto3" json:"branch,omitempty"`
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
	// them. Views of followers may keep more, freed: used is at least own and inherited.
	Inherited uint64 `protobuf:"varint,8,opt,name=inherited,proto3" json:"inherited,omitempty"`
	// The commit number of its latest commit: what a follower of it advances to.
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
	// The tenant whose cell's branch it is (TASK-364).
	Tenant uint64 `protobuf:"varint,17,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell whose branch it is.
	Cell uint64 `protobuf:"varint,18,opt,name=cell,proto3" json:"cell,omitempty"`
	// How many extents it holds that its branch wrote: those whose bytes are `own` (TASK-454). What
	// the daemon holds in memory for a store open grows with all it holds, own and inherited: a few
	// hundred bytes an extent (TASK-443.2).
	OwnExtents uint64 `protobuf:"varint,19,opt,name=own_extents,json=ownExtents,proto3" json:"own_extents,omitempty"`
	// How many it holds that it inherited: those whose bytes are `inherited`.
	InheritedExtents uint64 `protobuf:"varint,20,opt,name=inherited_extents,json=inheritedExtents,proto3" json:"inherited_extents,omitempty"`
	// The version of its cell it is a copy of (MakeVersion's `copy`, CreateStore's `version`), or
	// 0 if it is none: what a copy on another host is made of.
	Version       uint64 `protobuf:"varint,21,opt,name=version,proto3" json:"version,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *StatStoreResponse) Reset() {
	*x = StatStoreResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[30]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *StatStoreResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*StatStoreResponse) ProtoMessage() {}

func (x *StatStoreResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use StatStoreResponse.ProtoReflect.Descriptor instead.
func (*StatStoreResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{30}
}

func (x *StatStoreResponse) GetBranch() uint64 {
	if x != nil {
		return x.Branch
	}
	return 0
}

func (x *StatStoreResponse) GetCapacity() uint64 {
	if x != nil {
		return x.Capacity
	}
	return 0
}

func (x *StatStoreResponse) GetUsed() uint64 {
	if x != nil {
		return x.Used
	}
	return 0
}

func (x *StatStoreResponse) GetAvailable() uint64 {
	if x != nil {
		return x.Available
	}
	return 0
}

func (x *StatStoreResponse) GetPublic() bool {
	if x != nil {
		return x.Public
	}
	return false
}

func (x *StatStoreResponse) GetOpen() bool {
	if x != nil {
		return x.Open
	}
	return false
}

func (x *StatStoreResponse) GetOwn() uint64 {
	if x != nil {
		return x.Own
	}
	return 0
}

func (x *StatStoreResponse) GetInherited() uint64 {
	if x != nil {
		return x.Inherited
	}
	return 0
}

func (x *StatStoreResponse) GetCommitted() uint64 {
	if x != nil {
		return x.Committed
	}
	return 0
}

func (x *StatStoreResponse) GetUploading() bool {
	if x != nil {
		return x.Uploading
	}
	return false
}

func (x *StatStoreResponse) GetTakenToken() uint64 {
	if x != nil {
		return x.TakenToken
	}
	return 0
}

func (x *StatStoreResponse) GetTakenEpoch() uint64 {
	if x != nil {
		return x.TakenEpoch
	}
	return 0
}

func (x *StatStoreResponse) GetTakenUnixMs() uint64 {
	if x != nil {
		return x.TakenUnixMs
	}
	return 0
}

func (x *StatStoreResponse) GetUploaded() uint64 {
	if x != nil {
		return x.Uploaded
	}
	return 0
}

func (x *StatStoreResponse) GetPendingUploadBytes() uint64 {
	if x != nil {
		return x.PendingUploadBytes
	}
	return 0
}

func (x *StatStoreResponse) GetOldestPendingMs() uint64 {
	if x != nil {
		return x.OldestPendingMs
	}
	return 0
}

func (x *StatStoreResponse) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *StatStoreResponse) GetCell() uint64 {
	if x != nil {
		return x.Cell
	}
	return 0
}

func (x *StatStoreResponse) GetOwnExtents() uint64 {
	if x != nil {
		return x.OwnExtents
	}
	return 0
}

func (x *StatStoreResponse) GetInheritedExtents() uint64 {
	if x != nil {
		return x.InheritedExtents
	}
	return 0
}

func (x *StatStoreResponse) GetVersion() uint64 {
	if x != nil {
		return x.Version
	}
	return 0
}

// Asks for the stores the daemon serves.
type ListStoresRequest struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListStoresRequest) Reset() {
	*x = ListStoresRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[31]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListStoresRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListStoresRequest) ProtoMessage() {}

func (x *ListStoresRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ListStoresRequest.ProtoReflect.Descriptor instead.
func (*ListStoresRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{31}
}

// The stores the daemon serves, in no order.
type ListStoresResponse struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Each store, and what it is and holds.
	Stores        []*ListedStore `protobuf:"bytes,1,rep,name=stores,proto3" json:"stores,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListStoresResponse) Reset() {
	*x = ListStoresResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[32]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListStoresResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListStoresResponse) ProtoMessage() {}

func (x *ListStoresResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ListStoresResponse.ProtoReflect.Descriptor instead.
func (*ListStoresResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{32}
}

func (x *ListStoresResponse) GetStores() []*ListedStore {
	if x != nil {
		return x.Stores
	}
	return nil
}

// A store the daemon serves.
type ListedStore struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its directory, as the daemon has it: without symbolic links.
	Path string `protobuf:"bytes,1,opt,name=path,proto3" json:"path,omitempty"`
	// What StatStore says of it.
	Stat          *StatStoreResponse `protobuf:"bytes,2,opt,name=stat,proto3" json:"stat,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ListedStore) Reset() {
	*x = ListedStore{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[33]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ListedStore) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ListedStore) ProtoMessage() {}

func (x *ListedStore) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ListedStore.ProtoReflect.Descriptor instead.
func (*ListedStore) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{33}
}

func (x *ListedStore) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *ListedStore) GetStat() *StatStoreResponse {
	if x != nil {
		return x.Stat
	}
	return nil
}

// Asks what the daemon did.
type GetCountersRequest struct {
	state         protoimpl.MessageState `protogen:"open.v1"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetCountersRequest) Reset() {
	*x = GetCountersRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[34]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetCountersRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetCountersRequest) ProtoMessage() {}

func (x *GetCountersRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GetCountersRequest.ProtoReflect.Descriptor instead.
func (*GetCountersRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{34}
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
	// The bytes they read from local stores.
	PageInBytes uint64 `protobuf:"varint,6,opt,name=page_in_bytes,json=pageInBytes,proto3" json:"page_in_bytes,omitempty"`
	// The bytes they fetched from object storage.
	FetchedBytes uint64 `protobuf:"varint,7,opt,name=fetched_bytes,json=fetchedBytes,proto3" json:"fetched_bytes,omitempty"`
	// READ requests.
	Reads uint64 `protobuf:"varint,8,opt,name=reads,proto3" json:"reads,omitempty"`
	// The bytes they read.
	ReadBytes uint64 `protobuf:"varint,9,opt,name=read_bytes,json=readBytes,proto3" json:"read_bytes,omitempty"`
	// Of those, the bytes read straight from local stores or object storage: of open extents, and
	// followers'. Sealed extents' are read through the extent cache, whose page-ins count them.
	ReadStoredBytes uint64 `protobuf:"varint,10,opt,name=read_stored_bytes,json=readStoredBytes,proto3" json:"read_stored_bytes,omitempty"`
	// Units of boot profiles read into memory ahead of forks.
	Prefetches uint64 `protobuf:"varint,11,opt,name=prefetches,proto3" json:"prefetches,omitempty"`
	// Parts of extents stored compressed, decompressed.
	Unpacks uint64 `protobuf:"varint,12,opt,name=unpacks,proto3" json:"unpacks,omitempty"`
	// What the daemon did with its bucket, which it always has (optional, as it once might not).
	ObjectStorage *ObjectStorageStats `protobuf:"bytes,13,opt,name=object_storage,json=objectStorage,proto3,oneof" json:"object_storage,omitempty"`
	// Commits: COMMIT requests, and WRITEs' barriers and commits.
	Commits uint64 `protobuf:"varint,14,opt,name=commits,proto3" json:"commits,omitempty"`
	// The nanoseconds they took, all of them.
	CommitNs uint64 `protobuf:"varint,15,opt,name=commit_ns,json=commitNs,proto3" json:"commit_ns,omitempty"`
	// Of those nanoseconds, committing the store.
	CommitStoreNs uint64 `protobuf:"varint,18,opt,name=commit_store_ns,json=commitStoreNs,proto3" json:"commit_store_ns,omitempty"`
	// Of those, waiting for object storage: for the upload of a commit waiting for it, or for the uploads
	// to come within the lag an ordered one allows.
	CommitWaitNs uint64 `protobuf:"varint,20,opt,name=commit_wait_ns,json=commitWaitNs,proto3" json:"commit_wait_ns,omitempty"`
	// Segments stores closed and framed.
	SegmentCloses uint64 `protobuf:"varint,21,opt,name=segment_closes,json=segmentCloses,proto3" json:"segment_closes,omitempty"`
	// The nanoseconds closing them took.
	SegmentCloseNs uint64 `protobuf:"varint,22,opt,name=segment_close_ns,json=segmentCloseNs,proto3" json:"segment_close_ns,omitempty"`
	unknownFields  protoimpl.UnknownFields
	sizeCache      protoimpl.SizeCache
}

func (x *GetCountersResponse) Reset() {
	*x = GetCountersResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[35]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetCountersResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetCountersResponse) ProtoMessage() {}

func (x *GetCountersResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GetCountersResponse.ProtoReflect.Descriptor instead.
func (*GetCountersResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{35}
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
	WaitMs        uint32 `protobuf:"varint,1,opt,name=wait_ms,json=waitMs,proto3" json:"wait_ms,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetStatsRequest) Reset() {
	*x = GetStatsRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[36]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetStatsRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetStatsRequest) ProtoMessage() {}

func (x *GetStatsRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GetStatsRequest.ProtoReflect.Descriptor instead.
func (*GetStatsRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{36}
}

func (x *GetStatsRequest) GetWaitMs() uint32 {
	if x != nil {
		return x.WaitMs
	}
	return 0
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
	// Its VM processes that opened a store, in the order they connected.
	Vms           []*VmStats `protobuf:"bytes,5,rep,name=vms,proto3" json:"vms,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetStatsResponse) Reset() {
	*x = GetStatsResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[37]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetStatsResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetStatsResponse) ProtoMessage() {}

func (x *GetStatsResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GetStatsResponse.ProtoReflect.Descriptor instead.
func (*GetStatsResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{37}
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

// A VM process, and its guest, as it answered, or as the daemon has it if it didn't.
type VmStats struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its connection's number, the daemon's count of connections as it connected: what tells apart
	// a store's writer and its followers, of one branch.
	Connection uint64 `protobuf:"varint,1,opt,name=connection,proto3" json:"connection,omitempty"`
	// Its store's branch.
	Branch uint64 `protobuf:"varint,2,opt,name=branch,proto3" json:"branch,omitempty"`
	// Its store's directory, without symbolic links.
	Path string `protobuf:"bytes,3,opt,name=path,proto3" json:"path,omitempty"`
	// Whether it follows its store, read-only, or writes it.
	Follower bool `protobuf:"varint,4,opt,name=follower,proto3" json:"follower,omitempty"`
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
	Guest         *GuestStats `protobuf:"bytes,11,opt,name=guest,proto3,oneof" json:"guest,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *VmStats) Reset() {
	*x = VmStats{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[38]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *VmStats) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*VmStats) ProtoMessage() {}

func (x *VmStats) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use VmStats.ProtoReflect.Descriptor instead.
func (*VmStats) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{38}
}

func (x *VmStats) GetConnection() uint64 {
	if x != nil {
		return x.Connection
	}
	return 0
}

func (x *VmStats) GetBranch() uint64 {
	if x != nil {
		return x.Branch
	}
	return 0
}

func (x *VmStats) GetPath() string {
	if x != nil {
		return x.Path
	}
	return ""
}

func (x *VmStats) GetFollower() bool {
	if x != nil {
		return x.Follower
	}
	return false
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[39]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GuestStats) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GuestStats) ProtoMessage() {}

func (x *GuestStats) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GuestStats.ProtoReflect.Descriptor instead.
func (*GuestStats) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{39}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[40]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *Histogram) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*Histogram) ProtoMessage() {}

func (x *Histogram) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use Histogram.ProtoReflect.Descriptor instead.
func (*Histogram) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{40}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[41]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ObjectStorageStats) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ObjectStorageStats) ProtoMessage() {}

func (x *ObjectStorageStats) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ObjectStorageStats.ProtoReflect.Descriptor instead.
func (*ObjectStorageStats) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{41}
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

// Grants a tenant.
type GrantRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// Its ID in the bucket (TASK-364), not the default tenant's, 0 (INVALID_ARGUMENT): the stores in
	// its directory are its cells', under its prefix, and its VM processes open no store of
	// another tenant's cells (PERMISSION_DENIED, EACCES). Its socket is named by it, in base 32
	// (Crockford's digits, lower case, no leading zeros): `<tenant>.sock`.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The directory its stores are in, which must exist: its VM processes name stores in it
	// alone.
	Dir string `protobuf:"bytes,2,opt,name=dir,proto3" json:"dir,omitempty"`
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[42]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GrantRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GrantRequest) ProtoMessage() {}

func (x *GrantRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GrantRequest.ProtoReflect.Descriptor instead.
func (*GrantRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{42}
}

func (x *GrantRequest) GetTenant() uint64 {
	if x != nil {
		return x.Tenant
	}
	return 0
}

func (x *GrantRequest) GetDir() string {
	if x != nil {
		return x.Dir
	}
	return ""
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[43]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GrantResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GrantResponse) ProtoMessage() {}

func (x *GrantResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GrantResponse.ProtoReflect.Descriptor instead.
func (*GrantResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{43}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[44]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *RevokeRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*RevokeRequest) ProtoMessage() {}

func (x *RevokeRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use RevokeRequest.ProtoReflect.Descriptor instead.
func (*RevokeRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{44}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[45]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *RevokeResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*RevokeResponse) ProtoMessage() {}

func (x *RevokeResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use RevokeResponse.ProtoReflect.Descriptor instead.
func (*RevokeResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{45}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[46]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryLimitRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryLimitRequest) ProtoMessage() {}

func (x *SetMemoryLimitRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetMemoryLimitRequest.ProtoReflect.Descriptor instead.
func (*SetMemoryLimitRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{46}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[47]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryLimitResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryLimitResponse) ProtoMessage() {}

func (x *SetMemoryLimitResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetMemoryLimitResponse.ProtoReflect.Descriptor instead.
func (*SetMemoryLimitResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{47}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[48]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryWeightRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryWeightRequest) ProtoMessage() {}

func (x *SetMemoryWeightRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetMemoryWeightRequest.ProtoReflect.Descriptor instead.
func (*SetMemoryWeightRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{48}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[49]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetMemoryWeightResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetMemoryWeightResponse) ProtoMessage() {}

func (x *SetMemoryWeightResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetMemoryWeightResponse.ProtoReflect.Descriptor instead.
func (*SetMemoryWeightResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{49}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[50]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetTenantInFlightRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetTenantInFlightRequest) ProtoMessage() {}

func (x *SetTenantInFlightRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetTenantInFlightRequest.ProtoReflect.Descriptor instead.
func (*SetTenantInFlightRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{50}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[51]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetTenantInFlightResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetTenantInFlightResponse) ProtoMessage() {}

func (x *SetTenantInFlightResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetTenantInFlightResponse.ProtoReflect.Descriptor instead.
func (*SetTenantInFlightResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{51}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[52]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShrinkMemoryRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShrinkMemoryRequest) ProtoMessage() {}

func (x *ShrinkMemoryRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShrinkMemoryRequest.ProtoReflect.Descriptor instead.
func (*ShrinkMemoryRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{52}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[53]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShrinkMemoryResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShrinkMemoryResponse) ProtoMessage() {}

func (x *ShrinkMemoryResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShrinkMemoryResponse.ProtoReflect.Descriptor instead.
func (*ShrinkMemoryResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{53}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[54]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskLimitRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskLimitRequest) ProtoMessage() {}

func (x *SetDiskLimitRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetDiskLimitRequest.ProtoReflect.Descriptor instead.
func (*SetDiskLimitRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{54}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[55]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetDiskLimitResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetDiskLimitResponse) ProtoMessage() {}

func (x *SetDiskLimitResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetDiskLimitResponse.ProtoReflect.Descriptor instead.
func (*SetDiskLimitResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{55}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[56]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetPeersRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetPeersRequest) ProtoMessage() {}

func (x *SetPeersRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetPeersRequest.ProtoReflect.Descriptor instead.
func (*SetPeersRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{56}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[57]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *SetPeersResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*SetPeersResponse) ProtoMessage() {}

func (x *SetPeersResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use SetPeersResponse.ProtoReflect.Descriptor instead.
func (*SetPeersResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{57}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[58]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetTenantDiskRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetTenantDiskRequest) ProtoMessage() {}

func (x *GetTenantDiskRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GetTenantDiskRequest.ProtoReflect.Descriptor instead.
func (*GetTenantDiskRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{58}
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
	// The bytes of the filesystem's blocks its directories' files take, as `du -sx` counts them,
	// with what writes admitted add as the count is made.
	Bytes uint64 `protobuf:"varint,1,opt,name=bytes,proto3" json:"bytes,omitempty"`
	// Its limit, if it has one.
	LimitBytes    *uint64 `protobuf:"varint,2,opt,name=limit_bytes,json=limitBytes,proto3,oneof" json:"limit_bytes,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *GetTenantDiskResponse) Reset() {
	*x = GetTenantDiskResponse{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[59]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *GetTenantDiskResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*GetTenantDiskResponse) ProtoMessage() {}

func (x *GetTenantDiskResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use GetTenantDiskResponse.ProtoReflect.Descriptor instead.
func (*GetTenantDiskResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{59}
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

// Shreds a cell.
type ShredCellRequest struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The cell's tenant, as StatStore names it.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// The cell's ID in its tenant, as StatStore names it.
	Cell          uint64 `protobuf:"varint,2,opt,name=cell,proto3" json:"cell,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShredCellRequest) Reset() {
	*x = ShredCellRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[60]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredCellRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredCellRequest) ProtoMessage() {}

func (x *ShredCellRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShredCellRequest.ProtoReflect.Descriptor instead.
func (*ShredCellRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{60}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[61]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredCellResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredCellResponse) ProtoMessage() {}

func (x *ShredCellResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShredCellResponse.ProtoReflect.Descriptor instead.
func (*ShredCellResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{61}
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
	// The tenant's ID in the bucket, as StatStore names it.
	Tenant uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	// Why, for the daemon's log: e.g. that the KEK is destroyed.
	Why           string `protobuf:"bytes,2,opt,name=why,proto3" json:"why,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ForgetTenantKeysRequest) Reset() {
	*x = ForgetTenantKeysRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[62]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForgetTenantKeysRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForgetTenantKeysRequest) ProtoMessage() {}

func (x *ForgetTenantKeysRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ForgetTenantKeysRequest.ProtoReflect.Descriptor instead.
func (*ForgetTenantKeysRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{62}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[63]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ForgetTenantKeysResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ForgetTenantKeysResponse) ProtoMessage() {}

func (x *ForgetTenantKeysResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ForgetTenantKeysResponse.ProtoReflect.Descriptor instead.
func (*ForgetTenantKeysResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{63}
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
	// The tenant's ID in the bucket, as StatStore names it.
	Tenant        uint64 `protobuf:"varint,1,opt,name=tenant,proto3" json:"tenant,omitempty"`
	unknownFields protoimpl.UnknownFields
	sizeCache     protoimpl.SizeCache
}

func (x *ShredTenantRequest) Reset() {
	*x = ShredTenantRequest{}
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[64]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredTenantRequest) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredTenantRequest) ProtoMessage() {}

func (x *ShredTenantRequest) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShredTenantRequest.ProtoReflect.Descriptor instead.
func (*ShredTenantRequest) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{64}
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[65]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *ShredTenantResponse) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*ShredTenantResponse) ProtoMessage() {}

func (x *ShredTenantResponse) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use ShredTenantResponse.ProtoReflect.Descriptor instead.
func (*ShredTenantResponse) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{65}
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

// What a host forgot of a tenant's or a cell's keys.
type Forgotten struct {
	state protoimpl.MessageState `protogen:"open.v1"`
	// The stores open of them it halted.
	Stores uint32 `protobuf:"varint,1,opt,name=stores,proto3" json:"stores,omitempty"`
	// The stores' keys its fetchers held.
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
	mi := &file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[66]
	ms := protoimpl.X.MessageStateOf(protoimpl.Pointer(x))
	ms.StoreMessageInfo(mi)
}

func (x *Forgotten) String() string {
	return protoimpl.X.MessageStringOf(x)
}

func (*Forgotten) ProtoMessage() {}

func (x *Forgotten) ProtoReflect() protoreflect.Message {
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

// Deprecated: Use Forgotten.ProtoReflect.Descriptor instead.
func (*Forgotten) Descriptor() ([]byte, []int) {
	return file_cladefs_cladefsd_v1_cladefsd_proto_rawDescGZIP(), []int{66}
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
	"\"cladefs/cladefsd/v1/cladefsd.proto\x12\x13cladefs.cladefsd.v1\"\xfb\x01\n" +
	"\x12CreateStoreRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x1a\n" +
	"\bcapacity\x18\x02 \x01(\x04R\bcapacity\x12\x16\n" +
	"\x06branch\x18\x03 \x01(\x04R\x06branch\x12\x19\n" +
	"\x05token\x18\x04 \x01(\x04H\x00R\x05token\x88\x01\x01\x12\x16\n" +
	"\x06tenant\x18\x05 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x06 \x01(\x04R\x04cell\x122\n" +
	"\x04kind\x18\a \x01(\x0e2\x1e.cladefs.cladefsd.v1.VdiskKindR\x04kind\x12\x18\n" +
	"\aversion\x18\b \x01(\x04R\aversionB\b\n" +
	"\x06_token\"Y\n" +
	"\x13CreateStoreResponse\x12\x16\n" +
	"\x06branch\x18\x01 \x01(\x04R\x06branch\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cell\"\x90\x01\n" +
	"\x12ImportImageRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x1a\n" +
	"\bcapacity\x18\x02 \x01(\x04R\bcapacity\x12\x14\n" +
	"\x05image\x18\x03 \x01(\tR\x05image\x12\x1c\n" +
	"\treference\x18\x04 \x01(\tR\treference\x12\x16\n" +
	"\x06tenant\x18\x05 \x01(\x04R\x06tenant\"Y\n" +
	"\x13ImportImageResponse\x12\x16\n" +
	"\x06branch\x18\x01 \x01(\x04R\x06branch\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cell\"H\n" +
	"\x10ForkStoreRequest\x12\x12\n" +
	"\x04from\x18\x01 \x01(\tR\x04from\x12\x0e\n" +
	"\x02to\x18\x02 \x01(\tR\x02toJ\x04\b\x03\x10\x04J\x04\b\x04\x10\x05J\x04\b\x05\x10\x06\"W\n" +
	"\x11ForkStoreResponse\x12\x16\n" +
	"\x06branch\x18\x01 \x01(\x04R\x06branch\x12\x16\n" +
	"\x06tenant\x18\x02 \x01(\x04R\x06tenant\x12\x12\n" +
	"\x04cell\x18\x03 \x01(\x04R\x04cell\"(\n" +
	"\x12DeleteStoreRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\"\x15\n" +
	"\x13DeleteStoreResponse\"&\n" +
	"\x10DropStoreRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\"\x13\n" +
	"\x11DropStoreResponse\"D\n" +
	"\x12ResizeStoreRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x1a\n" +
	"\bcapacity\x18\x02 \x01(\x04R\bcapacity\"\x15\n" +
	"\x13ResizeStoreResponse\"=\n" +
	"\x11PlaceStoreRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x14\n" +
	"\x05token\x18\x02 \x01(\x04R\x05token\"\x14\n" +
	"\x12PlaceStoreResponse\"Y\n" +
	"\x12MakeVersionRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x1b\n" +
	"\tkeep_last\x18\x02 \x01(\rR\bkeepLast\x12\x12\n" +
	"\x04copy\x18\x03 \x01(\tR\x04copy\"/\n" +
	"\x13MakeVersionResponse\x12\x18\n" +
	"\aversion\x18\x01 \x01(\x04R\aversion\")\n" +
	"\x13ListVersionsRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\"M\n" +
	"\aVersion\x12\x0e\n" +
	"\x02id\x18\x01 \x01(\x04R\x02id\x12\x1a\n" +
	"\buploaded\x18\x02 \x01(\bR\buploaded\x12\x16\n" +
	"\x06shared\x18\x03 \x01(\bR\x06shared\"P\n" +
	"\x14ListVersionsResponse\x128\n" +
	"\bversions\x18\x01 \x03(\v2\x1c.cladefs.cladefsd.v1.VersionR\bversions\"E\n" +
	"\x13DropVersionsRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x1a\n" +
	"\bversions\x18\x02 \x03(\x04R\bversions\"\x16\n" +
	"\x14DropVersionsResponse\"[\n" +
	"\x13ShareVersionRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x18\n" +
	"\aversion\x18\x02 \x01(\x04R\aversion\x12\x16\n" +
	"\x06shared\x18\x03 \x01(\bR\x06shared\"\x16\n" +
	"\x14ShareVersionResponse\"H\n" +
	"\x11MakePublicRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x1f\n" +
	"\vattach_only\x18\x02 \x01(\bR\n" +
	"attachOnly\"\x14\n" +
	"\x12MakePublicResponse\";\n" +
	"\x11AttachBaseRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12\x12\n" +
	"\x04base\x18\x02 \x01(\tR\x04base\"\x14\n" +
	"\x12AttachBaseResponse\"&\n" +
	"\x10SyncStoreRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\"\x13\n" +
	"\x11SyncStoreResponse\"&\n" +
	"\x10StatStoreRequest\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\"\x85\x05\n" +
	"\x11StatStoreResponse\x12\x16\n" +
	"\x06branch\x18\x01 \x01(\x04R\x06branch\x12\x1a\n" +
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
	"\x11inherited_extents\x18\x14 \x01(\x04R\x10inheritedExtents\x12\x18\n" +
	"\aversion\x18\x15 \x01(\x04R\aversion\"\x13\n" +
	"\x11ListStoresRequest\"N\n" +
	"\x12ListStoresResponse\x128\n" +
	"\x06stores\x18\x01 \x03(\v2 .cladefs.cladefsd.v1.ListedStoreR\x06stores\"]\n" +
	"\vListedStore\x12\x12\n" +
	"\x04path\x18\x01 \x01(\tR\x04path\x12:\n" +
	"\x04stat\x18\x02 \x01(\v2&.cladefs.cladefsd.v1.StatStoreResponseR\x04stat\"\x14\n" +
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
	"\x0f_object_storageJ\x04\b\x10\x10\x11J\x04\b\x11\x10\x12J\x04\b\x13\x10\x14R\x0fcommit_write_nsR\x14commit_written_bytesR\x0ecommit_seal_ns\"*\n" +
	"\x0fGetStatsRequest\x12\x17\n" +
	"\await_ms\x18\x01 \x01(\rR\x06waitMs\"\x90\x03\n" +
	"\x10GetStatsResponse\x12\"\n" +
	"\rtaken_unix_ns\x18\x01 \x01(\x04R\vtakenUnixNs\x12\x14\n" +
	"\x05build\x18\x02 \x01(\tR\x05build\x12O\n" +
	"\bcounters\x18\x03 \x03(\v23.cladefs.cladefsd.v1.GetStatsResponse.CountersEntryR\bcounters\x12I\n" +
	"\x06gauges\x18\x04 \x03(\v21.cladefs.cladefsd.v1.GetStatsResponse.GaugesEntryR\x06gauges\x12.\n" +
	"\x03vms\x18\x05 \x03(\v2\x1c.cladefs.cladefsd.v1.VmStatsR\x03vms\x1a;\n" +
	"\rCountersEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a9\n" +
	"\vGaugesEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\"\x85\x05\n" +
	"\aVmStats\x12\x1e\n" +
	"\n" +
	"connection\x18\x01 \x01(\x04R\n" +
	"connection\x12\x16\n" +
	"\x06branch\x18\x02 \x01(\x04R\x06branch\x12\x12\n" +
	"\x04path\x18\x03 \x01(\tR\x04path\x12\x1a\n" +
	"\bfollower\x18\x04 \x01(\bR\bfollower\x12\x14\n" +
	"\x05build\x18\x05 \x01(\tR\x05build\x12F\n" +
	"\bcounters\x18\x06 \x03(\v2*.cladefs.cladefsd.v1.VmStats.CountersEntryR\bcounters\x12@\n" +
	"\x06gauges\x18\a \x03(\v2(.cladefs.cladefsd.v1.VmStats.GaugesEntryR\x06gauges\x12@\n" +
	"\x06window\x18\b \x03(\v2(.cladefs.cladefsd.v1.VmStats.WindowEntryR\x06window\x12\x1a\n" +
	"\banswered\x18\t \x01(\bR\banswered\x12\x1b\n" +
	"\tanswer_ns\x18\n" +
	" \x01(\x04R\banswerNs\x12:\n" +
	"\x05guest\x18\v \x01(\v2\x1f.cladefs.cladefsd.v1.GuestStatsH\x00R\x05guest\x88\x01\x01\x1a;\n" +
	"\rCountersEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a9\n" +
	"\vGaugesEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\x1a9\n" +
	"\vWindowEntry\x12\x10\n" +
	"\x03key\x18\x01 \x01(\tR\x03key\x12\x14\n" +
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01B\b\n" +
	"\x06_guest\"\xda\x02\n" +
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
	"\x05value\x18\x02 \x01(\x04R\x05value:\x028\x01\"v\n" +
	"\fGrantRequest\x12\x16\n" +
	"\x06tenant\x18\x01 \x01(\x04R\x06tenant\x12\x10\n" +
	"\x03dir\x18\x02 \x01(\tR\x03dir\x12\x15\n" +
	"\x03uid\x18\x03 \x01(\rH\x00R\x03uid\x88\x01\x01\x12\x15\n" +
	"\x03gid\x18\x04 \x01(\rH\x01R\x03gid\x88\x01\x01B\x06\n" +
	"\x04_uidB\x06\n" +
	"\x04_gid\"'\n" +
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
	"\x14SetDiskLimitResponse\"/\n" +
	"\x0fSetPeersRequest\x12\x1c\n" +
	"\taddresses\x18\x01 \x03(\tR\taddresses\"\x12\n" +
	"\x10SetPeersResponse\">\n" +
	"\x14GetTenantDiskRequest\x12\x1b\n" +
	"\x06tenant\x18\x01 \x01(\x04H\x00R\x06tenant\x88\x01\x01B\t\n" +
	"\a_tenant\"c\n" +
	"\x15GetTenantDiskResponse\x12\x14\n" +
	"\x05bytes\x18\x01 \x01(\x04R\x05bytes\x12$\n" +
	"\vlimit_bytes\x18\x02 \x01(\x04H\x00R\n" +
	"limitBytes\x88\x01\x01B\x0e\n" +
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
	"\tforgotten\x18\x02 \x01(\v2\x1e.cladefs.cladefsd.v1.ForgottenR\tforgotten\"}\n" +
	"\tForgotten\x12\x16\n" +
	"\x06stores\x18\x01 \x01(\rR\x06stores\x12\x14\n" +
	"\x05rings\x18\x02 \x01(\rR\x05rings\x12\x12\n" +
	"\x04kept\x18\x03 \x01(\rR\x04kept\x12\x18\n" +
	"\aextents\x18\x04 \x01(\x04R\aextents\x12\x14\n" +
	"\x05bytes\x18\x05 \x01(\x04R\x05bytes*N\n" +
	"\tVdiskKind\x12\x13\n" +
	"\x0fVDISK_KIND_FXFS\x10\x00\x12\x15\n" +
	"\x11VDISK_KIND_SQLITE\x10\x01\x12\x15\n" +
	"\x11VDISK_KIND_MEMORY\x10\x022\xf4\x16\n" +
	"\bCladefsd\x12`\n" +
	"\vCreateStore\x12'.cladefs.cladefsd.v1.CreateStoreRequest\x1a(.cladefs.cladefsd.v1.CreateStoreResponse\x12`\n" +
	"\vImportImage\x12'.cladefs.cladefsd.v1.ImportImageRequest\x1a(.cladefs.cladefsd.v1.ImportImageResponse\x12Z\n" +
	"\tForkStore\x12%.cladefs.cladefsd.v1.ForkStoreRequest\x1a&.cladefs.cladefsd.v1.ForkStoreResponse\x12`\n" +
	"\vDeleteStore\x12'.cladefs.cladefsd.v1.DeleteStoreRequest\x1a(.cladefs.cladefsd.v1.DeleteStoreResponse\x12Z\n" +
	"\tDropStore\x12%.cladefs.cladefsd.v1.DropStoreRequest\x1a&.cladefs.cladefsd.v1.DropStoreResponse\x12`\n" +
	"\vResizeStore\x12'.cladefs.cladefsd.v1.ResizeStoreRequest\x1a(.cladefs.cladefsd.v1.ResizeStoreResponse\x12]\n" +
	"\n" +
	"PlaceStore\x12&.cladefs.cladefsd.v1.PlaceStoreRequest\x1a'.cladefs.cladefsd.v1.PlaceStoreResponse\x12]\n" +
	"\n" +
	"MakePublic\x12&.cladefs.cladefsd.v1.MakePublicRequest\x1a'.cladefs.cladefsd.v1.MakePublicResponse\x12`\n" +
	"\vMakeVersion\x12'.cladefs.cladefsd.v1.MakeVersionRequest\x1a(.cladefs.cladefsd.v1.MakeVersionResponse\x12c\n" +
	"\fListVersions\x12(.cladefs.cladefsd.v1.ListVersionsRequest\x1a).cladefs.cladefsd.v1.ListVersionsResponse\x12c\n" +
	"\fDropVersions\x12(.cladefs.cladefsd.v1.DropVersionsRequest\x1a).cladefs.cladefsd.v1.DropVersionsResponse\x12c\n" +
	"\fShareVersion\x12(.cladefs.cladefsd.v1.ShareVersionRequest\x1a).cladefs.cladefsd.v1.ShareVersionResponse\x12]\n" +
	"\n" +
	"AttachBase\x12&.cladefs.cladefsd.v1.AttachBaseRequest\x1a'.cladefs.cladefsd.v1.AttachBaseResponse\x12Z\n" +
	"\tSyncStore\x12%.cladefs.cladefsd.v1.SyncStoreRequest\x1a&.cladefs.cladefsd.v1.SyncStoreResponse\x12Z\n" +
	"\tStatStore\x12%.cladefs.cladefsd.v1.StatStoreRequest\x1a&.cladefs.cladefsd.v1.StatStoreResponse\x12]\n" +
	"\n" +
	"ListStores\x12&.cladefs.cladefsd.v1.ListStoresRequest\x1a'.cladefs.cladefsd.v1.ListStoresResponse\x12`\n" +
	"\vGetCounters\x12'.cladefs.cladefsd.v1.GetCountersRequest\x1a(.cladefs.cladefsd.v1.GetCountersResponse\x12W\n" +
	"\bGetStats\x12$.cladefs.cladefsd.v1.GetStatsRequest\x1a%.cladefs.cladefsd.v1.GetStatsResponse\x12N\n" +
	"\x05Grant\x12!.cladefs.cladefsd.v1.GrantRequest\x1a\".cladefs.cladefsd.v1.GrantResponse\x12Q\n" +
	"\x06Revoke\x12\".cladefs.cladefsd.v1.RevokeRequest\x1a#.cladefs.cladefsd.v1.RevokeResponse\x12i\n" +
	"\x0eSetMemoryLimit\x12*.cladefs.cladefsd.v1.SetMemoryLimitRequest\x1a+.cladefs.cladefsd.v1.SetMemoryLimitResponse\x12l\n" +
	"\x0fSetMemoryWeight\x12+.cladefs.cladefsd.v1.SetMemoryWeightRequest\x1a,.cladefs.cladefsd.v1.SetMemoryWeightResponse\x12r\n" +
	"\x11SetTenantInFlight\x12-.cladefs.cladefsd.v1.SetTenantInFlightRequest\x1a..cladefs.cladefsd.v1.SetTenantInFlightResponse\x12c\n" +
	"\fShrinkMemory\x12(.cladefs.cladefsd.v1.ShrinkMemoryRequest\x1a).cladefs.cladefsd.v1.ShrinkMemoryResponse\x12c\n" +
	"\fSetDiskLimit\x12(.cladefs.cladefsd.v1.SetDiskLimitRequest\x1a).cladefs.cladefsd.v1.SetDiskLimitResponse\x12f\n" +
	"\rGetTenantDisk\x12).cladefs.cladefsd.v1.GetTenantDiskRequest\x1a*.cladefs.cladefsd.v1.GetTenantDiskResponse\x12W\n" +
	"\bSetPeers\x12$.cladefs.cladefsd.v1.SetPeersRequest\x1a%.cladefs.cladefsd.v1.SetPeersResponse\x12Z\n" +
	"\tShredCell\x12%.cladefs.cladefsd.v1.ShredCellRequest\x1a&.cladefs.cladefsd.v1.ShredCellResponse\x12o\n" +
	"\x10ForgetTenantKeys\x12,.cladefs.cladefsd.v1.ForgetTenantKeysRequest\x1a-.cladefs.cladefsd.v1.ForgetTenantKeysResponse\x12`\n" +
	"\vShredTenant\x12'.cladefs.cladefsd.v1.ShredTenantRequest\x1a(.cladefs.cladefsd.v1.ShredTenantResponseB9Z7github.com/semistrict/cladefs/go/cladefsd/v1;cladefsdv1b\x06proto3"

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

var file_cladefs_cladefsd_v1_cladefsd_proto_enumTypes = make([]protoimpl.EnumInfo, 1)
var file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes = make([]protoimpl.MessageInfo, 77)
var file_cladefs_cladefsd_v1_cladefsd_proto_goTypes = []any{
	(VdiskKind)(0),                    // 0: cladefs.cladefsd.v1.VdiskKind
	(*CreateStoreRequest)(nil),        // 1: cladefs.cladefsd.v1.CreateStoreRequest
	(*CreateStoreResponse)(nil),       // 2: cladefs.cladefsd.v1.CreateStoreResponse
	(*ImportImageRequest)(nil),        // 3: cladefs.cladefsd.v1.ImportImageRequest
	(*ImportImageResponse)(nil),       // 4: cladefs.cladefsd.v1.ImportImageResponse
	(*ForkStoreRequest)(nil),          // 5: cladefs.cladefsd.v1.ForkStoreRequest
	(*ForkStoreResponse)(nil),         // 6: cladefs.cladefsd.v1.ForkStoreResponse
	(*DeleteStoreRequest)(nil),        // 7: cladefs.cladefsd.v1.DeleteStoreRequest
	(*DeleteStoreResponse)(nil),       // 8: cladefs.cladefsd.v1.DeleteStoreResponse
	(*DropStoreRequest)(nil),          // 9: cladefs.cladefsd.v1.DropStoreRequest
	(*DropStoreResponse)(nil),         // 10: cladefs.cladefsd.v1.DropStoreResponse
	(*ResizeStoreRequest)(nil),        // 11: cladefs.cladefsd.v1.ResizeStoreRequest
	(*ResizeStoreResponse)(nil),       // 12: cladefs.cladefsd.v1.ResizeStoreResponse
	(*PlaceStoreRequest)(nil),         // 13: cladefs.cladefsd.v1.PlaceStoreRequest
	(*PlaceStoreResponse)(nil),        // 14: cladefs.cladefsd.v1.PlaceStoreResponse
	(*MakeVersionRequest)(nil),        // 15: cladefs.cladefsd.v1.MakeVersionRequest
	(*MakeVersionResponse)(nil),       // 16: cladefs.cladefsd.v1.MakeVersionResponse
	(*ListVersionsRequest)(nil),       // 17: cladefs.cladefsd.v1.ListVersionsRequest
	(*Version)(nil),                   // 18: cladefs.cladefsd.v1.Version
	(*ListVersionsResponse)(nil),      // 19: cladefs.cladefsd.v1.ListVersionsResponse
	(*DropVersionsRequest)(nil),       // 20: cladefs.cladefsd.v1.DropVersionsRequest
	(*DropVersionsResponse)(nil),      // 21: cladefs.cladefsd.v1.DropVersionsResponse
	(*ShareVersionRequest)(nil),       // 22: cladefs.cladefsd.v1.ShareVersionRequest
	(*ShareVersionResponse)(nil),      // 23: cladefs.cladefsd.v1.ShareVersionResponse
	(*MakePublicRequest)(nil),         // 24: cladefs.cladefsd.v1.MakePublicRequest
	(*MakePublicResponse)(nil),        // 25: cladefs.cladefsd.v1.MakePublicResponse
	(*AttachBaseRequest)(nil),         // 26: cladefs.cladefsd.v1.AttachBaseRequest
	(*AttachBaseResponse)(nil),        // 27: cladefs.cladefsd.v1.AttachBaseResponse
	(*SyncStoreRequest)(nil),          // 28: cladefs.cladefsd.v1.SyncStoreRequest
	(*SyncStoreResponse)(nil),         // 29: cladefs.cladefsd.v1.SyncStoreResponse
	(*StatStoreRequest)(nil),          // 30: cladefs.cladefsd.v1.StatStoreRequest
	(*StatStoreResponse)(nil),         // 31: cladefs.cladefsd.v1.StatStoreResponse
	(*ListStoresRequest)(nil),         // 32: cladefs.cladefsd.v1.ListStoresRequest
	(*ListStoresResponse)(nil),        // 33: cladefs.cladefsd.v1.ListStoresResponse
	(*ListedStore)(nil),               // 34: cladefs.cladefsd.v1.ListedStore
	(*GetCountersRequest)(nil),        // 35: cladefs.cladefsd.v1.GetCountersRequest
	(*GetCountersResponse)(nil),       // 36: cladefs.cladefsd.v1.GetCountersResponse
	(*GetStatsRequest)(nil),           // 37: cladefs.cladefsd.v1.GetStatsRequest
	(*GetStatsResponse)(nil),          // 38: cladefs.cladefsd.v1.GetStatsResponse
	(*VmStats)(nil),                   // 39: cladefs.cladefsd.v1.VmStats
	(*GuestStats)(nil),                // 40: cladefs.cladefsd.v1.GuestStats
	(*Histogram)(nil),                 // 41: cladefs.cladefsd.v1.Histogram
	(*ObjectStorageStats)(nil),        // 42: cladefs.cladefsd.v1.ObjectStorageStats
	(*GrantRequest)(nil),              // 43: cladefs.cladefsd.v1.GrantRequest
	(*GrantResponse)(nil),             // 44: cladefs.cladefsd.v1.GrantResponse
	(*RevokeRequest)(nil),             // 45: cladefs.cladefsd.v1.RevokeRequest
	(*RevokeResponse)(nil),            // 46: cladefs.cladefsd.v1.RevokeResponse
	(*SetMemoryLimitRequest)(nil),     // 47: cladefs.cladefsd.v1.SetMemoryLimitRequest
	(*SetMemoryLimitResponse)(nil),    // 48: cladefs.cladefsd.v1.SetMemoryLimitResponse
	(*SetMemoryWeightRequest)(nil),    // 49: cladefs.cladefsd.v1.SetMemoryWeightRequest
	(*SetMemoryWeightResponse)(nil),   // 50: cladefs.cladefsd.v1.SetMemoryWeightResponse
	(*SetTenantInFlightRequest)(nil),  // 51: cladefs.cladefsd.v1.SetTenantInFlightRequest
	(*SetTenantInFlightResponse)(nil), // 52: cladefs.cladefsd.v1.SetTenantInFlightResponse
	(*ShrinkMemoryRequest)(nil),       // 53: cladefs.cladefsd.v1.ShrinkMemoryRequest
	(*ShrinkMemoryResponse)(nil),      // 54: cladefs.cladefsd.v1.ShrinkMemoryResponse
	(*SetDiskLimitRequest)(nil),       // 55: cladefs.cladefsd.v1.SetDiskLimitRequest
	(*SetDiskLimitResponse)(nil),      // 56: cladefs.cladefsd.v1.SetDiskLimitResponse
	(*SetPeersRequest)(nil),           // 57: cladefs.cladefsd.v1.SetPeersRequest
	(*SetPeersResponse)(nil),          // 58: cladefs.cladefsd.v1.SetPeersResponse
	(*GetTenantDiskRequest)(nil),      // 59: cladefs.cladefsd.v1.GetTenantDiskRequest
	(*GetTenantDiskResponse)(nil),     // 60: cladefs.cladefsd.v1.GetTenantDiskResponse
	(*ShredCellRequest)(nil),          // 61: cladefs.cladefsd.v1.ShredCellRequest
	(*ShredCellResponse)(nil),         // 62: cladefs.cladefsd.v1.ShredCellResponse
	(*ForgetTenantKeysRequest)(nil),   // 63: cladefs.cladefsd.v1.ForgetTenantKeysRequest
	(*ForgetTenantKeysResponse)(nil),  // 64: cladefs.cladefsd.v1.ForgetTenantKeysResponse
	(*ShredTenantRequest)(nil),        // 65: cladefs.cladefsd.v1.ShredTenantRequest
	(*ShredTenantResponse)(nil),       // 66: cladefs.cladefsd.v1.ShredTenantResponse
	(*Forgotten)(nil),                 // 67: cladefs.cladefsd.v1.Forgotten
	nil,                               // 68: cladefs.cladefsd.v1.GetStatsResponse.CountersEntry
	nil,                               // 69: cladefs.cladefsd.v1.GetStatsResponse.GaugesEntry
	nil,                               // 70: cladefs.cladefsd.v1.VmStats.CountersEntry
	nil,                               // 71: cladefs.cladefsd.v1.VmStats.GaugesEntry
	nil,                               // 72: cladefs.cladefsd.v1.VmStats.WindowEntry
	nil,                               // 73: cladefs.cladefsd.v1.GuestStats.CountersEntry
	nil,                               // 74: cladefs.cladefsd.v1.GuestStats.HistogramsEntry
	nil,                               // 75: cladefs.cladefsd.v1.ObjectStorageStats.RequestsEntry
	nil,                               // 76: cladefs.cladefsd.v1.ObjectStorageStats.UploadedBytesEntry
	nil,                               // 77: cladefs.cladefsd.v1.ObjectStorageStats.FailuresEntry
}
var file_cladefs_cladefsd_v1_cladefsd_proto_depIdxs = []int32{
	0,  // 0: cladefs.cladefsd.v1.CreateStoreRequest.kind:type_name -> cladefs.cladefsd.v1.VdiskKind
	18, // 1: cladefs.cladefsd.v1.ListVersionsResponse.versions:type_name -> cladefs.cladefsd.v1.Version
	34, // 2: cladefs.cladefsd.v1.ListStoresResponse.stores:type_name -> cladefs.cladefsd.v1.ListedStore
	31, // 3: cladefs.cladefsd.v1.ListedStore.stat:type_name -> cladefs.cladefsd.v1.StatStoreResponse
	42, // 4: cladefs.cladefsd.v1.GetCountersResponse.object_storage:type_name -> cladefs.cladefsd.v1.ObjectStorageStats
	68, // 5: cladefs.cladefsd.v1.GetStatsResponse.counters:type_name -> cladefs.cladefsd.v1.GetStatsResponse.CountersEntry
	69, // 6: cladefs.cladefsd.v1.GetStatsResponse.gauges:type_name -> cladefs.cladefsd.v1.GetStatsResponse.GaugesEntry
	39, // 7: cladefs.cladefsd.v1.GetStatsResponse.vms:type_name -> cladefs.cladefsd.v1.VmStats
	70, // 8: cladefs.cladefsd.v1.VmStats.counters:type_name -> cladefs.cladefsd.v1.VmStats.CountersEntry
	71, // 9: cladefs.cladefsd.v1.VmStats.gauges:type_name -> cladefs.cladefsd.v1.VmStats.GaugesEntry
	72, // 10: cladefs.cladefsd.v1.VmStats.window:type_name -> cladefs.cladefsd.v1.VmStats.WindowEntry
	40, // 11: cladefs.cladefsd.v1.VmStats.guest:type_name -> cladefs.cladefsd.v1.GuestStats
	73, // 12: cladefs.cladefsd.v1.GuestStats.counters:type_name -> cladefs.cladefsd.v1.GuestStats.CountersEntry
	74, // 13: cladefs.cladefsd.v1.GuestStats.histograms:type_name -> cladefs.cladefsd.v1.GuestStats.HistogramsEntry
	75, // 14: cladefs.cladefsd.v1.ObjectStorageStats.requests:type_name -> cladefs.cladefsd.v1.ObjectStorageStats.RequestsEntry
	76, // 15: cladefs.cladefsd.v1.ObjectStorageStats.uploaded_bytes:type_name -> cladefs.cladefsd.v1.ObjectStorageStats.UploadedBytesEntry
	77, // 16: cladefs.cladefsd.v1.ObjectStorageStats.failures:type_name -> cladefs.cladefsd.v1.ObjectStorageStats.FailuresEntry
	67, // 17: cladefs.cladefsd.v1.ShredCellResponse.forgotten:type_name -> cladefs.cladefsd.v1.Forgotten
	67, // 18: cladefs.cladefsd.v1.ForgetTenantKeysResponse.forgotten:type_name -> cladefs.cladefsd.v1.Forgotten
	67, // 19: cladefs.cladefsd.v1.ShredTenantResponse.forgotten:type_name -> cladefs.cladefsd.v1.Forgotten
	41, // 20: cladefs.cladefsd.v1.GuestStats.HistogramsEntry.value:type_name -> cladefs.cladefsd.v1.Histogram
	1,  // 21: cladefs.cladefsd.v1.Cladefsd.CreateStore:input_type -> cladefs.cladefsd.v1.CreateStoreRequest
	3,  // 22: cladefs.cladefsd.v1.Cladefsd.ImportImage:input_type -> cladefs.cladefsd.v1.ImportImageRequest
	5,  // 23: cladefs.cladefsd.v1.Cladefsd.ForkStore:input_type -> cladefs.cladefsd.v1.ForkStoreRequest
	7,  // 24: cladefs.cladefsd.v1.Cladefsd.DeleteStore:input_type -> cladefs.cladefsd.v1.DeleteStoreRequest
	9,  // 25: cladefs.cladefsd.v1.Cladefsd.DropStore:input_type -> cladefs.cladefsd.v1.DropStoreRequest
	11, // 26: cladefs.cladefsd.v1.Cladefsd.ResizeStore:input_type -> cladefs.cladefsd.v1.ResizeStoreRequest
	13, // 27: cladefs.cladefsd.v1.Cladefsd.PlaceStore:input_type -> cladefs.cladefsd.v1.PlaceStoreRequest
	24, // 28: cladefs.cladefsd.v1.Cladefsd.MakePublic:input_type -> cladefs.cladefsd.v1.MakePublicRequest
	15, // 29: cladefs.cladefsd.v1.Cladefsd.MakeVersion:input_type -> cladefs.cladefsd.v1.MakeVersionRequest
	17, // 30: cladefs.cladefsd.v1.Cladefsd.ListVersions:input_type -> cladefs.cladefsd.v1.ListVersionsRequest
	20, // 31: cladefs.cladefsd.v1.Cladefsd.DropVersions:input_type -> cladefs.cladefsd.v1.DropVersionsRequest
	22, // 32: cladefs.cladefsd.v1.Cladefsd.ShareVersion:input_type -> cladefs.cladefsd.v1.ShareVersionRequest
	26, // 33: cladefs.cladefsd.v1.Cladefsd.AttachBase:input_type -> cladefs.cladefsd.v1.AttachBaseRequest
	28, // 34: cladefs.cladefsd.v1.Cladefsd.SyncStore:input_type -> cladefs.cladefsd.v1.SyncStoreRequest
	30, // 35: cladefs.cladefsd.v1.Cladefsd.StatStore:input_type -> cladefs.cladefsd.v1.StatStoreRequest
	32, // 36: cladefs.cladefsd.v1.Cladefsd.ListStores:input_type -> cladefs.cladefsd.v1.ListStoresRequest
	35, // 37: cladefs.cladefsd.v1.Cladefsd.GetCounters:input_type -> cladefs.cladefsd.v1.GetCountersRequest
	37, // 38: cladefs.cladefsd.v1.Cladefsd.GetStats:input_type -> cladefs.cladefsd.v1.GetStatsRequest
	43, // 39: cladefs.cladefsd.v1.Cladefsd.Grant:input_type -> cladefs.cladefsd.v1.GrantRequest
	45, // 40: cladefs.cladefsd.v1.Cladefsd.Revoke:input_type -> cladefs.cladefsd.v1.RevokeRequest
	47, // 41: cladefs.cladefsd.v1.Cladefsd.SetMemoryLimit:input_type -> cladefs.cladefsd.v1.SetMemoryLimitRequest
	49, // 42: cladefs.cladefsd.v1.Cladefsd.SetMemoryWeight:input_type -> cladefs.cladefsd.v1.SetMemoryWeightRequest
	51, // 43: cladefs.cladefsd.v1.Cladefsd.SetTenantInFlight:input_type -> cladefs.cladefsd.v1.SetTenantInFlightRequest
	53, // 44: cladefs.cladefsd.v1.Cladefsd.ShrinkMemory:input_type -> cladefs.cladefsd.v1.ShrinkMemoryRequest
	55, // 45: cladefs.cladefsd.v1.Cladefsd.SetDiskLimit:input_type -> cladefs.cladefsd.v1.SetDiskLimitRequest
	59, // 46: cladefs.cladefsd.v1.Cladefsd.GetTenantDisk:input_type -> cladefs.cladefsd.v1.GetTenantDiskRequest
	57, // 47: cladefs.cladefsd.v1.Cladefsd.SetPeers:input_type -> cladefs.cladefsd.v1.SetPeersRequest
	61, // 48: cladefs.cladefsd.v1.Cladefsd.ShredCell:input_type -> cladefs.cladefsd.v1.ShredCellRequest
	63, // 49: cladefs.cladefsd.v1.Cladefsd.ForgetTenantKeys:input_type -> cladefs.cladefsd.v1.ForgetTenantKeysRequest
	65, // 50: cladefs.cladefsd.v1.Cladefsd.ShredTenant:input_type -> cladefs.cladefsd.v1.ShredTenantRequest
	2,  // 51: cladefs.cladefsd.v1.Cladefsd.CreateStore:output_type -> cladefs.cladefsd.v1.CreateStoreResponse
	4,  // 52: cladefs.cladefsd.v1.Cladefsd.ImportImage:output_type -> cladefs.cladefsd.v1.ImportImageResponse
	6,  // 53: cladefs.cladefsd.v1.Cladefsd.ForkStore:output_type -> cladefs.cladefsd.v1.ForkStoreResponse
	8,  // 54: cladefs.cladefsd.v1.Cladefsd.DeleteStore:output_type -> cladefs.cladefsd.v1.DeleteStoreResponse
	10, // 55: cladefs.cladefsd.v1.Cladefsd.DropStore:output_type -> cladefs.cladefsd.v1.DropStoreResponse
	12, // 56: cladefs.cladefsd.v1.Cladefsd.ResizeStore:output_type -> cladefs.cladefsd.v1.ResizeStoreResponse
	14, // 57: cladefs.cladefsd.v1.Cladefsd.PlaceStore:output_type -> cladefs.cladefsd.v1.PlaceStoreResponse
	25, // 58: cladefs.cladefsd.v1.Cladefsd.MakePublic:output_type -> cladefs.cladefsd.v1.MakePublicResponse
	16, // 59: cladefs.cladefsd.v1.Cladefsd.MakeVersion:output_type -> cladefs.cladefsd.v1.MakeVersionResponse
	19, // 60: cladefs.cladefsd.v1.Cladefsd.ListVersions:output_type -> cladefs.cladefsd.v1.ListVersionsResponse
	21, // 61: cladefs.cladefsd.v1.Cladefsd.DropVersions:output_type -> cladefs.cladefsd.v1.DropVersionsResponse
	23, // 62: cladefs.cladefsd.v1.Cladefsd.ShareVersion:output_type -> cladefs.cladefsd.v1.ShareVersionResponse
	27, // 63: cladefs.cladefsd.v1.Cladefsd.AttachBase:output_type -> cladefs.cladefsd.v1.AttachBaseResponse
	29, // 64: cladefs.cladefsd.v1.Cladefsd.SyncStore:output_type -> cladefs.cladefsd.v1.SyncStoreResponse
	31, // 65: cladefs.cladefsd.v1.Cladefsd.StatStore:output_type -> cladefs.cladefsd.v1.StatStoreResponse
	33, // 66: cladefs.cladefsd.v1.Cladefsd.ListStores:output_type -> cladefs.cladefsd.v1.ListStoresResponse
	36, // 67: cladefs.cladefsd.v1.Cladefsd.GetCounters:output_type -> cladefs.cladefsd.v1.GetCountersResponse
	38, // 68: cladefs.cladefsd.v1.Cladefsd.GetStats:output_type -> cladefs.cladefsd.v1.GetStatsResponse
	44, // 69: cladefs.cladefsd.v1.Cladefsd.Grant:output_type -> cladefs.cladefsd.v1.GrantResponse
	46, // 70: cladefs.cladefsd.v1.Cladefsd.Revoke:output_type -> cladefs.cladefsd.v1.RevokeResponse
	48, // 71: cladefs.cladefsd.v1.Cladefsd.SetMemoryLimit:output_type -> cladefs.cladefsd.v1.SetMemoryLimitResponse
	50, // 72: cladefs.cladefsd.v1.Cladefsd.SetMemoryWeight:output_type -> cladefs.cladefsd.v1.SetMemoryWeightResponse
	52, // 73: cladefs.cladefsd.v1.Cladefsd.SetTenantInFlight:output_type -> cladefs.cladefsd.v1.SetTenantInFlightResponse
	54, // 74: cladefs.cladefsd.v1.Cladefsd.ShrinkMemory:output_type -> cladefs.cladefsd.v1.ShrinkMemoryResponse
	56, // 75: cladefs.cladefsd.v1.Cladefsd.SetDiskLimit:output_type -> cladefs.cladefsd.v1.SetDiskLimitResponse
	60, // 76: cladefs.cladefsd.v1.Cladefsd.GetTenantDisk:output_type -> cladefs.cladefsd.v1.GetTenantDiskResponse
	58, // 77: cladefs.cladefsd.v1.Cladefsd.SetPeers:output_type -> cladefs.cladefsd.v1.SetPeersResponse
	62, // 78: cladefs.cladefsd.v1.Cladefsd.ShredCell:output_type -> cladefs.cladefsd.v1.ShredCellResponse
	64, // 79: cladefs.cladefsd.v1.Cladefsd.ForgetTenantKeys:output_type -> cladefs.cladefsd.v1.ForgetTenantKeysResponse
	66, // 80: cladefs.cladefsd.v1.Cladefsd.ShredTenant:output_type -> cladefs.cladefsd.v1.ShredTenantResponse
	51, // [51:81] is the sub-list for method output_type
	21, // [21:51] is the sub-list for method input_type
	21, // [21:21] is the sub-list for extension type_name
	21, // [21:21] is the sub-list for extension extendee
	0,  // [0:21] is the sub-list for field type_name
}

func init() { file_cladefs_cladefsd_v1_cladefsd_proto_init() }
func file_cladefs_cladefsd_v1_cladefsd_proto_init() {
	if File_cladefs_cladefsd_v1_cladefsd_proto != nil {
		return
	}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[0].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[35].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[38].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[42].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[46].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[50].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[52].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[54].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[58].OneofWrappers = []any{}
	file_cladefs_cladefsd_v1_cladefsd_proto_msgTypes[59].OneofWrappers = []any{}
	type x struct{}
	out := protoimpl.TypeBuilder{
		File: protoimpl.DescBuilder{
			GoPackagePath: reflect.TypeOf(x{}).PkgPath(),
			RawDescriptor: unsafe.Slice(unsafe.StringData(file_cladefs_cladefsd_v1_cladefsd_proto_rawDesc), len(file_cladefs_cladefsd_v1_cladefsd_proto_rawDesc)),
			NumEnums:      1,
			NumMessages:   77,
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
