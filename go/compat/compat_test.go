// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Tests of go/compat.sh, which fails a change of the control and mount APIs that breaks a caller
// of the last build's (doc-28, board 10056): in a repository of their own holding this tree's
// crates/cladefs/proto, a commit before and one after.
package compat_test

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// The .proto, relative to this package's directory.
const protos = "../../crates/cladefs/proto"

// repo is a repository whose first commit, base, holds this tree's crates/cladefs/proto.
type repo struct {
	t    *testing.T
	dir  string
	base string
}

func newRepo(t *testing.T) *repo {
	r := &repo{t: t, dir: t.TempDir()}
	r.git("init", "-q", "-b", "main")
	r.git("config", "user.name", "Test")
	r.git("config", "user.email", "test@example.com")
	copied := exec.Command("cp", "-R", protos, filepath.Join(r.dir, "crates/cladefs/proto"))
	if err := os.MkdirAll(filepath.Join(r.dir, "crates/cladefs"), 0o755); err != nil {
		t.Fatal(err)
	}
	if out, err := copied.CombinedOutput(); err != nil {
		t.Fatalf("copying the .proto: %v: %s", err, out)
	}
	r.base = r.commit("base")
	return r
}

func (r *repo) git(args ...string) string {
	r.t.Helper()
	out, err := exec.Command("git", append([]string{"-C", r.dir}, args...)...).CombinedOutput()
	if err != nil {
		r.t.Fatalf("git %v: %v: %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// edit replaces old, which must be there, by new in the .proto at path, under crates/cladefs/proto.
func (r *repo) edit(path, old, new string) {
	r.t.Helper()
	file := filepath.Join(r.dir, "crates/cladefs/proto", path)
	text, err := os.ReadFile(file)
	if err != nil {
		r.t.Fatal(err)
	}
	if !strings.Contains(string(text), old) {
		r.t.Fatalf("%s holds no %q", path, old)
	}
	if err := os.WriteFile(file, []byte(strings.Replace(string(text), old, new, 1)), 0o644); err != nil {
		r.t.Fatal(err)
	}
}

func (r *repo) commit(message string) string {
	r.git("add", "-A")
	r.git("commit", "-q", "--allow-empty", "-m", message)
	return r.git("rev-parse", "HEAD")
}

// check runs go/compat.sh against the base: its exit status and what it said.
func (r *repo) check() (int, string) {
	r.t.Helper()
	script, err := filepath.Abs("../compat.sh")
	if err != nil {
		r.t.Fatal(err)
	}
	ran := exec.Command(script, r.base)
	ran.Dir = r.dir
	out, err := ran.CombinedOutput()
	if exit, ok := err.(*exec.ExitError); ok {
		return exit.ExitCode(), string(out)
	} else if err != nil {
		r.t.Fatalf("running go/compat.sh: %v", err)
	}
	return 0, string(out)
}

const cladefsd = "cladefs/cladefsd/v1/cladefsd.proto"
const mount = "cladefs/mount/v1/mount.proto"

func TestAFieldAddedBreaksNoCaller(t *testing.T) {
	r := newRepo(t)
	r.edit(cladefsd, "  string peer = 2;\n", "  string peer = 2;\n  uint64 deadline = 6;\n")
	r.commit("MigrateCell takes a deadline")
	status, said := r.check()
	if status != 0 || strings.Contains(said, "crates/cladefs/proto") {
		t.Fatalf("status %d: %s", status, said)
	}
}

func TestARenamedOrRenumberedFieldIsRefused(t *testing.T) {
	r := newRepo(t)
	r.edit(cladefsd, "  string peer = 2;\n", "  string target_peer = 2;\n")
	r.edit(mount, "  uint64 capacity = 4;\n", "  uint64 capacity = 99;\n")
	r.commit("fields moved")
	status, said := r.check()
	if status != 1 {
		t.Fatalf("status %d: %s", status, said)
	}
	for _, flagged := range []string{
		`cladefs/cladefsd/v1/cladefsd.proto:`,
		`Field "2" with name "target_peer" on message "MigrateCellRequest" changed option "json_name" from "peer" to "targetPeer".`,
		`cladefs/mount/v1/mount.proto:`,
		`Previously present field "4" with name "capacity" on message "AttachRequest" was deleted.`,
		"crates/cladefs/proto: breaks a caller of " + r.base + "'s, and no commit since changing it names the user's agreement",
	} {
		if !strings.Contains(said, flagged) {
			t.Errorf("%q not in:\n%s", flagged, said)
		}
	}
}

func TestTheUsersAgreementLetsABreakThrough(t *testing.T) {
	r := newRepo(t)
	r.edit(cladefsd, "  string peer = 2;\n", "  string target_peer = 2;\n")
	r.commit("MigrateCell names its destination\n\nBreaks-format:  hotbed question 512")
	r.commit("something else")
	status, said := r.check()
	if status != 0 {
		t.Fatalf("status %d: %s", status, said)
	}
	agreed := "crates/cladefs/proto: breaks a caller of " + r.base +
		"'s, as the user agreed: Breaks-format:  hotbed question 512\n"
	if !strings.HasSuffix(said, agreed) {
		t.Fatalf("%q not the end of:\n%s", agreed, said)
	}
}

// An agreement named by a commit that leaves the .proto as they are, to break a corpus, lets no
// break of them through.
func TestAnAgreementToAnotherBreakLetsNoneThrough(t *testing.T) {
	r := newRepo(t)
	if err := os.WriteFile(filepath.Join(r.dir, "README.md"), []byte("cladefs\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	r.commit("a corpus case changed\n\nBreaks-format: hotbed question 512")
	r.edit(cladefsd, "  string peer = 2;\n", "  string target_peer = 2;\n")
	r.commit("MigrateCell names its destination")
	status, said := r.check()
	if status != 1 || !strings.Contains(said, "no commit since changing it names the user's agreement") {
		t.Fatalf("status %d: %s", status, said)
	}
}

// The peer service is checked by buf breaking too (doc-28, board 10054).
func TestAPeerFieldRenumberedIsRefused(t *testing.T) {
	r := newRepo(t)
	r.edit("cladefs/peer/v1/peer.proto", "  uint64 migration = 1;\n", "  uint64 migration = 999;\n")
	r.commit("peer field renumbered")
	status, said := r.check()
	if status != 1 {
		t.Fatalf("status %d: %s", status, said)
	}
	for _, flagged := range []string{
		`cladefs/peer/v1/peer.proto:`,
		`Previously present field "1" with name "migration" on message "MigratePrepareRequest" was deleted.`,
		"crates/cladefs/proto: breaks a caller of " + r.base + "'s, and no commit since changing it names the user's agreement",
	} {
		if !strings.Contains(said, flagged) {
			t.Errorf("%q not in:\n%s", flagged, said)
		}
	}
}
