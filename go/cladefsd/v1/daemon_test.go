// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//go:build cladefsd

// The client against the daemon itself, cladefsd, which CLADEFSD names, built from the
// same commit: Linux's alone, as the daemon is (scripts/test.sh's go part).
//
//	CLADEFSD=target/debug/cladefsd go test -tags cladefsd ./...

package cladefsdv1_test

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	cladefsdv1 "github.com/semistrict/cladefs/go/cladefsd/v1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/backoff"
	"google.golang.org/grpc/codes"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/status"
)

// daemon starts cladefsd from a TOML config file (an in-memory bucket + one managed disk in dir),
// serving its control API until the test ends, and returns a client of it once it is SERVING.
func daemon(t *testing.T, dir string) *cladefsdv1.Client {
	binary := os.Getenv("CLADEFSD")
	if binary == "" {
		t.Fatal("CLADEFSD names no cladefsd to test against")
	}
	control := filepath.Join(dir, "control.sock")
	disk0 := filepath.Join(dir, "disk0")
	if err := os.MkdirAll(disk0, 0o755); err != nil {
		t.Fatal(err)
	}
	configPath := filepath.Join(dir, "cladefs.toml")
	configToml := "[daemon]\n" +
		"vm_socket = " + `"` + filepath.Join(dir, "vms.sock") + `"` + "\n" +
		"control_socket = " + `"` + control + `"` + "\n" +
		"token = 1\n" +
		"\n[bucket]\n" +
		`url = "memory:///cells"` + "\n" +
		"\n[kms]\n" +
		`master_key = "7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e"` + "\n" +
		"\n[memory]\n" +
		`bytes = "64 MiB"` + "\n" +
		"\n[[disks]]\n" +
		`id = "disk0"` + "\n" +
		"path = " + `"` + disk0 + `"` + "\n"
	if err := os.WriteFile(configPath, []byte(configToml), 0o600); err != nil {
		t.Fatal(err)
	}
	command := exec.Command(binary, "--config", configPath)
	command.Stderr = os.Stderr
	if err := command.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		command.Process.Kill()
		command.Wait()
	})
	// Until the socket is bound, the client reconnects: soon, rather than backing off to minutes.
	retry := backoff.DefaultConfig
	retry.MaxDelay = 100 * time.Millisecond
	client, err := cladefsdv1.NewClient(control, grpc.WithConnectParams(grpc.ConnectParams{Backoff: retry}))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	watch, err := client.Health.Watch(ctx, &healthpb.HealthCheckRequest{}, grpc.WaitForReady(true))
	if err != nil {
		t.Fatal(err)
	}
	for {
		health, err := watch.Recv()
		if err != nil {
			t.Fatalf("the daemon never served: %v", err)
		}
		if health.Status == healthpb.HealthCheckResponse_SERVING {
			return client
		}
	}
}

func TestDaemon(t *testing.T) {
	dir, err := os.MkdirTemp("", "cladefsdv1")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	client := daemon(t, dir)
	ctx := t.Context()

	created, err := client.CreateCell(ctx, &cladefsdv1.CreateCellRequest{Capacity: 1 << 24})
	if err != nil {
		t.Fatal(err)
	}
	stat, err := client.StatCell(ctx, &cladefsdv1.StatCellRequest{Tenant: created.Tenant, Cell: created.Cell})
	if err != nil {
		t.Fatal(err)
	}
	if stat.Capacity != 1<<24 {
		t.Errorf("StatCell's capacity = %d, want %d", stat.Capacity, 1<<24)
	}

	_, err = client.StatCell(ctx, &cladefsdv1.StatCellRequest{Tenant: 0, Cell: 999999})
	if !errors.Is(err, syscall.ENOENT) {
		t.Errorf("StatCell of no cell: errors.Is(%v, ENOENT) = false", err)
	}
	if code := status.Code(err); code != codes.NotFound {
		t.Errorf("StatCell of no cell: status.Code(%v) = %v, want NotFound", err, code)
	}

	// Tenant 42, whose socket is named in base 32, 1a (2a in hexadecimal).
	granted, err := client.Grant(ctx, &cladefsdv1.GrantRequest{Tenant: 42})
	if err != nil {
		t.Fatal(err)
	}
	if want := filepath.Join(dir, "tenants", "1a.sock"); granted.Socket != want {
		t.Errorf("Grant's socket = %q, want %q", granted.Socket, want)
	}
	// Granting the tenant again is idempotent.
	grantedAgain, err := client.Grant(ctx, &cladefsdv1.GrantRequest{Tenant: 42})
	if err != nil {
		t.Fatal(err)
	}
	if grantedAgain.Socket != granted.Socket {
		t.Errorf("re-Grant socket = %q, want %q", grantedAgain.Socket, granted.Socket)
	}

	// ListDisks & SetDiskGovernance.
	disks, err := client.ListDisks(ctx, &cladefsdv1.ListDisksRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(disks.Disks) != 1 || disks.Disks[0].Id != "disk0" {
		t.Fatalf("ListDisks = %v, want 1 disk named disk0", disks.Disks)
	}
	capBytes := uint64(4 << 30)
	drain := cladefsdv1.DiskPlacementMode_DISK_PLACEMENT_DRAIN
	if _, err := client.SetDiskGovernance(ctx, &cladefsdv1.SetDiskGovernanceRequest{
		Id:        "disk0",
		CapBytes:  &capBytes,
		Placement: &drain,
	}); err != nil {
		t.Fatal(err)
	}
	disks, err = client.ListDisks(ctx, &cladefsdv1.ListDisksRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if disks.Disks[0].CapBytes != capBytes || disks.Disks[0].Placement != drain {
		t.Errorf("disk0 after SetDiskGovernance = cap %d placement %v", disks.Disks[0].CapBytes, disks.Disks[0].Placement)
	}
}
