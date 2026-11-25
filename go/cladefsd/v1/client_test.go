// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package cladefsdv1_test

import (
	"context"
	"errors"
	"net"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"

	cladefsdv1 "github.com/semistrict/cladefs/go/cladefsd/v1"
	mountv1 "github.com/semistrict/cladefs/go/mount/v1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/health"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"
)

// fake serves the control API as the daemon answers it: Grant a socket, StatCell failing as the
// daemon fails a call, with a status and its errno, or with an errno entry that is none, GetStats
// and ListCells answers past gRPC's default limit of a message, 4 MiB.
type fake struct {
	cladefsdv1.UnimplementedCladefsdServer
}

// fakeMount serves the FUSE mount's Mount service, its List of one cell.
type fakeMount struct {
	mountv1.UnimplementedMountServer
}

func (fakeMount) List(context.Context, *mountv1.ListRequest) (*mountv1.ListResponse, error) {
	return &mountv1.ListResponse{Cells: []*mountv1.AttachedCell{{Tenant: 1, Cell: 42}}}, nil
}

// large is how large GetStats's and ListCells's answers are, past gRPC's default limit of a
// message, 4 MiB, short of MaxMessageBytes: some 2,600 VM processes' stats, at their 2 KiB each.
const large = 5 << 20

// pad is a string of n bytes.
func pad(n int) string {
	return strings.Repeat("p", n)
}

func (fake) GetStats(context.Context, *cladefsdv1.GetStatsRequest) (*cladefsdv1.GetStatsResponse, error) {
	vms := make([]*cladefsdv1.VmStats, large/(4<<10))
	for i := range vms {
		vms[i] = &cladefsdv1.VmStats{Connection: uint64(i), Build: pad(4 << 10)}
	}
	return &cladefsdv1.GetStatsResponse{Vms: vms}, nil
}

func (fake) ListCells(context.Context, *cladefsdv1.ListCellsRequest) (*cladefsdv1.ListCellsResponse, error) {
	cells := make([]*cladefsdv1.ListedCell, large/(4<<10))
	for i := range cells {
		cells[i] = &cladefsdv1.ListedCell{
			Cell:   uint64(i),
			DiskId: pad(4 << 10),
			Stat:   &cladefsdv1.StatCellResponse{Cell: uint64(i)},
		}
	}
	return &cladefsdv1.ListCellsResponse{Stores: cells}, nil
}

// base32 is id as the daemon names a tenant's socket by it: in base 32, Crockford's digits, lower
// case, no leading zeros (crates/base32_id).
func base32(id uint64) string {
	const digits = "0123456789abcdefghjkmnpqrstvwxyz"
	name := []byte{digits[id%32]}
	for id /= 32; id > 0; id /= 32 {
		name = append([]byte{digits[id%32]}, name...)
	}
	return string(name)
}

func (fake) Grant(_ context.Context, req *cladefsdv1.GrantRequest) (*cladefsdv1.GrantResponse, error) {
	return &cladefsdv1.GrantResponse{Socket: "/run/tenants/" + base32(req.Tenant) + ".sock"}, nil
}

const (
	cellMissing  uint64 = 1
	cellStale    uint64 = 2
	cellGarbled  uint64 = 3
	cellZero     uint64 = 4
	cellNegative uint64 = 5
	cellWide     uint64 = 6
	cellTwo      uint64 = 7
)

func (fake) StatCell(ctx context.Context, req *cladefsdv1.StatCellRequest) (*cladefsdv1.StatCellResponse, error) {
	errnos := map[uint64][]string{
		cellMissing:  {"2"},
		cellStale:    {"116"},
		cellGarbled:  {"ENOENT"},
		cellZero:     {"0"},
		cellNegative: {"-2"},
		cellWide:     {"4294967298"},
		cellTwo:      {"2", "2"},
	}[req.Cell]
	trailer := metadata.MD{}
	trailer.Append(cladefsdv1.ErrnoKey, errnos...)
	if err := grpc.SetTrailer(ctx, trailer); err != nil {
		return nil, err
	}
	return nil, status.Error(codes.NotFound, "ENOENT")
}

// serve serves fake and the health service on a socket of its own until the test ends: the
// socket's path.
func serve(t *testing.T) string {
	// Not t.TempDir(), whose path, with the test's name, may be past a socket's 104 bytes on
	// macOS.
	dir, err := os.MkdirTemp("", "cladefsdv1")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	socket := filepath.Join(dir, "control.sock")
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	server := grpc.NewServer()
	cladefsdv1.RegisterCladefsdServer(server, fake{})
	healthpb.RegisterHealthServer(server, health.NewServer())
	mountv1.RegisterMountServer(server, fakeMount{})
	go server.Serve(listener)
	t.Cleanup(server.Stop)
	return socket
}

func client(t *testing.T, socket string) *cladefsdv1.Client {
	client, err := cladefsdv1.NewClient(socket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	return client
}

func TestCalls(t *testing.T) {
	client := client(t, serve(t))
	granted, err := client.Grant(t.Context(), &cladefsdv1.GrantRequest{Tenant: 42})
	if err != nil {
		t.Fatal(err)
	}
	if granted.Socket != "/run/tenants/1a.sock" {
		t.Errorf("Grant's socket = %q, want /run/tenants/1a.sock", granted.Socket)
	}
	health, err := client.Health.Check(t.Context(), &healthpb.HealthCheckRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if health.Status != healthpb.HealthCheckResponse_SERVING {
		t.Errorf("health = %v, want SERVING", health.Status)
	}
	// The FUSE mount's Mount service, on the same connection (board 8587).
	listed, err := client.Mount.List(t.Context(), &mountv1.ListRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(listed.Cells) != 1 || listed.Cells[0].Cell != 42 {
		t.Errorf("Mount.List = %v, want cell 42 alone", listed.Cells)
	}
}

// Answers past gRPC's default limit of 4 MiB, as GetStats's and ListCells's of a host of many VMs
// and cells, are taken (board 8581).
func TestLargeAnswers(t *testing.T) {
	client := client(t, serve(t))
	stats, err := client.GetStats(t.Context(), &cladefsdv1.GetStatsRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if size := proto.Size(stats); size < large || size > cladefsdv1.MaxMessageBytes {
		t.Errorf("GetStats's answer is %d bytes, want %d to %d", size, large, cladefsdv1.MaxMessageBytes)
	}
	cells, err := client.ListCells(t.Context(), &cladefsdv1.ListCellsRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if size := proto.Size(cells); size < large || size > cladefsdv1.MaxMessageBytes {
		t.Errorf("ListCells's answer is %d bytes, want %d to %d", size, large, cladefsdv1.MaxMessageBytes)
	}
}

// A failed call's errno is Linux's, whatever system the client runs on: ErrnoOf returns Linux's
// number, and errors.Is finds this system's errno of it (board 8587).
func TestLinuxErrno(t *testing.T) {
	client := client(t, serve(t))
	_, err := client.StatCell(t.Context(), &cladefsdv1.StatCellRequest{Cell: cellStale})
	errno, ok := cladefsdv1.ErrnoOf(err)
	if !ok || errno != cladefsdv1.ESTALE {
		t.Errorf("ErrnoOf(%v) = %d, %v, want 116 (Linux's ESTALE), true", err, errno, ok)
	}
	if !errors.Is(err, syscall.ESTALE) {
		t.Errorf("errors.Is(%v, ESTALE) = false", err)
	}
	if _, ok := cladefsdv1.ErrnoOf(errors.New("no call's")); ok {
		t.Errorf("ErrnoOf found an errno of an error no call's")
	}
}

// A failed call's error is its status, and the errno the daemon failed with.
func TestErrno(t *testing.T) {
	client := client(t, serve(t))
	_, err := client.StatCell(t.Context(), &cladefsdv1.StatCellRequest{Cell: cellMissing})
	if !errors.Is(err, syscall.ENOENT) {
		t.Errorf("errors.Is(%v, ENOENT) = false", err)
	}
	var errno syscall.Errno
	if !errors.As(err, &errno) || errno != syscall.ENOENT {
		t.Errorf("errors.As(%v) = %v, want ENOENT", err, errno)
	}
	if code := status.Code(err); code != codes.NotFound {
		t.Errorf("status.Code(%v) = %v, want NotFound", err, code)
	}
	if err.Error() != "rpc error: code = NotFound desc = ENOENT" {
		t.Errorf("err = %q, the call's status alone", err)
	}
}

// A call failed without an errno is its status alone.
func TestNoErrno(t *testing.T) {
	client := client(t, serve(t))
	_, err := client.SyncCell(t.Context(), &cladefsdv1.SyncCellRequest{Cell: 1})
	var errno syscall.Errno
	if errors.As(err, &errno) {
		t.Errorf("errors.As(%v) found errno %v, want none", err, errno)
	}
	if code := status.Code(err); code != codes.Unimplemented {
		t.Errorf("status.Code(%v) = %v, want Unimplemented", err, code)
	}
}

// An errno entry that is no errno is said, and no errno taken from it: no number, 0, which the
// daemon never says of a failure (board 8587), a negative one, one past 32 bits, two entries.
func TestGarbledErrno(t *testing.T) {
	client := client(t, serve(t))
	for _, tc := range []struct {
		name  string
		cell  uint64
		entry string
	}{
		{"garbled", cellGarbled, `["ENOENT"]`},
		{"zero", cellZero, `["0"]`},
		{"negative", cellNegative, `["-2"]`},
		{"wide", cellWide, `["4294967298"]`},
		{"two", cellTwo, `["2" "2"]`},
	} {
		_, err := client.StatCell(t.Context(), &cladefsdv1.StatCellRequest{Cell: tc.cell})
		want := `rpc error: code = NotFound desc = ENOENT (its cladefs-errno ` + tc.entry + ` is no errno)`
		if err.Error() != want {
			t.Errorf("%s: err = %q, want %q", tc.name, err, want)
		}
		var errno syscall.Errno
		if errors.As(err, &errno) {
			t.Errorf("%s: errors.As(%v) found errno %v, want none", tc.name, err, errno)
		}
		if errno, ok := cladefsdv1.ErrnoOf(err); ok {
			t.Errorf("%s: ErrnoOf(%v) = %d, want none", tc.name, err, errno)
		}
		if code := status.Code(err); code != codes.NotFound {
			t.Errorf("%s: status.Code(%v) = %v, want NotFound", tc.name, err, code)
		}
	}
}
