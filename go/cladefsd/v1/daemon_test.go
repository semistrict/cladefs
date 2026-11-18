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

// daemon starts cladefsd, a host as the Rust tests have it (an in-memory bucket), serving its
// control API in dir until the test ends, and returns a client of it once it is SERVING.
func daemon(t *testing.T, dir string) *cladefsdv1.Client {
	binary := os.Getenv("CLADEFSD")
	if binary == "" {
		t.Fatal("CLADEFSD names no cladefsd to test against")
	}
	control := filepath.Join(dir, "control.sock")
	command := exec.Command(binary, filepath.Join(dir, "vms.sock"))
	command.Env = append(os.Environ(),
		"CLADEFSD_GRPC="+control,
		"CLADEFSD_BUCKET=memory:///cells",
		"CLADEFSD_TOKEN=1",
		// cladefs: every store encrypted, under the bucket KMS of a test master key, known to all
		// (TASK-411.28): a daemon given no KMS refuses to start.
		"CLADEFS_MASTER_KEY=7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e",
	)
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

	store := filepath.Join(dir, "store")
	if _, err := client.CreateCell(ctx, &cladefsdv1.CreateCellRequest{Path: store, Capacity: 1 << 24}); err != nil {
		t.Fatal(err)
	}
	stat, err := client.StatCell(ctx, &cladefsdv1.StatCellRequest{Path: store})
	if err != nil {
		t.Fatal(err)
	}
	if stat.Capacity != 1<<24 {
		t.Errorf("StatCell's capacity = %d, want %d", stat.Capacity, 1<<24)
	}

	_, err = client.StatCell(ctx, &cladefsdv1.StatCellRequest{Path: filepath.Join(dir, "missing")})
	if !errors.Is(err, syscall.ENOENT) {
		t.Errorf("StatCell of no store: errors.Is(%v, ENOENT) = false", err)
	}
	if code := status.Code(err); code != codes.NotFound {
		t.Errorf("StatCell of no store: status.Code(%v) = %v, want NotFound", err, code)
	}

	stores := filepath.Join(dir, "a")
	if err := os.Mkdir(stores, 0o755); err != nil {
		t.Fatal(err)
	}
	// Tenant 42, whose socket is named in base 32, 1a (2a in hexadecimal).
	granted, err := client.Grant(ctx, &cladefsdv1.GrantRequest{Tenant: 42, Dir: stores})
	if err != nil {
		t.Fatal(err)
	}
	if want := filepath.Join(dir, "tenants", "1a.sock"); granted.Socket != want {
		t.Errorf("Grant's socket = %q, want %q", granted.Socket, want)
	}
	// Granting the tenant again, of another directory.
	_, err = client.Grant(ctx, &cladefsdv1.GrantRequest{Tenant: 42, Dir: dir})
	if !errors.Is(err, syscall.EEXIST) {
		t.Errorf("Grant of another directory: errors.Is(%v, EEXIST) = false", err)
	}
	if code := status.Code(err); code != codes.AlreadyExists {
		t.Errorf("Grant of another directory: status.Code(%v) = %v, want AlreadyExists", err, code)
	}
}
