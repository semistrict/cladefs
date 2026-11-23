// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Package cladefsdv1 is the Go client of cladefs's extent daemon's control API, cladefs.cladefsd.v1
// (crates/cladefs/proto/cladefs/cladefsd/v1/cladefsd.proto, TASK-219): its messages and gRPC stubs,
// generated from the .proto by go/generate.sh, and [NewClient], which connects to a daemon's
// control socket as cladefs's Rust client does (cladefs::cladefsd::control::Client).
//
// The package at a commit of cladefs is the API of the daemon built from that commit:
//
//	go get github.com/semistrict/cladefs/go@<commit>
package cladefsdv1

import (
	"context"
	"errors"
	"fmt"
	"net"
	"runtime"
	"slices"
	"strconv"
	"syscall"

	mountv1 "github.com/semistrict/cladefs/go/mount/v1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
)

// ErrnoKey is the metadata entry of a failed call's status that holds the errno the daemon failed
// with, a decimal number.
const ErrnoKey = "cladefs-errno"

// MaxMessageBytes is the most bytes an answer of the control API is, as its .proto says: the daemon
// sends none larger (an answer that would be fails with RESOURCE_EXHAUSTED, EMSGSIZE, saying what
// to leave out), and [NewClient] takes answers of this size, where gRPC's default, 4 MiB, failed
// GetStats at some hundreds of VMs (board 8581).
const MaxMessageBytes = 64 << 20

// Client is a client of an extent daemon's control API, of the standard health service
// (grpc.health.v1.Health) the control socket serves too, SERVING while the daemon accepts VM
// processes, NOT_SERVING as it starts and as it stops, and of the Mount service of the daemon's
// FUSE mount of cells (go/mount/v1), which fails UNIMPLEMENTED if the daemon mounts none. All three
// share one connection, and the errnos of failed calls.
//
// A call's error carries the errno the daemon failed with, beside its gRPC status, which
// [status.FromError] finds. The errno is Linux's, the daemon's host's: [ErrnoOf] returns its
// number, which this package's constants name ([ENOENT] and the others), whatever system the
// client runs on, as through a forwarded socket. [errors.Is] and [errors.As] find it as a
// [syscall.Errno] of the client's own system where that system has the same errno, its number
// its own (macOS's ESTALE is 70, Linux's 116); not where it has none:
//
//	_, err := client.StatCell(ctx, &cladefsdv1.StatCellRequest{Path: path})
//	if errors.Is(err, syscall.ENOENT) { ... }
//	if errno, ok := cladefsdv1.ErrnoOf(err); ok && errno == cladefsdv1.ESTALE { ... }
type Client struct {
	CladefsdClient
	Health healthpb.HealthClient
	Mount  mountv1.MountClient
	conn   *grpc.ClientConn
}

// NewClient returns a client of the control API of the extent daemon whose control socket is at
// socket (the daemon's CLADEFSD_GRPC). As [grpc.NewClient], it connects at its first call,
// and again whenever the connection is lost; opts are added to its own.
func NewClient(socket string, opts ...grpc.DialOption) (*Client, error) {
	// The target names no host: the dialer connects to the socket, whatever it asks for, so that
	// any path does, which a unix: URI would have to escape.
	dial := func(ctx context.Context, _ string) (net.Conn, error) {
		var dialer net.Dialer
		return dialer.DialContext(ctx, "unix", socket)
	}
	conn, err := grpc.NewClient("passthrough:///cladefsd", slices.Concat([]grpc.DialOption{
		grpc.WithContextDialer(dial),
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithChainUnaryInterceptor(withErrno),
		grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(MaxMessageBytes)),
	}, opts)...)
	if err != nil {
		return nil, fmt.Errorf("a client of the extent daemon's control socket at %s: %w", socket, err)
	}
	return &Client{
		CladefsdClient: NewCladefsdClient(conn),
		Health:         healthpb.NewHealthClient(conn),
		Mount:          mountv1.NewMountClient(conn),
		conn:           conn,
	}, nil
}

// Close closes the client's connection.
func (c *Client) Close() error {
	return c.conn.Close()
}

// withErrno has a failed call's error carry the errno its status's [ErrnoKey] entry holds.
func withErrno(
	ctx context.Context,
	method string,
	req, reply any,
	cc *grpc.ClientConn,
	invoker grpc.UnaryInvoker,
	opts ...grpc.CallOption,
) error {
	var trailer metadata.MD
	err := invoker(ctx, method, req, reply, cc, append(slices.Clip(opts), grpc.Trailer(&trailer))...)
	if err == nil {
		return nil
	}
	values := trailer.Get(ErrnoKey)
	if len(values) == 0 {
		return err
	}
	// One decimal number, not 0: the daemon says an errno of a failure alone, and an entry of 0,
	// or of two, is of a daemon, proxy or build that is wrong, which takes no errno from it.
	errno, parseErr := strconv.ParseUint(values[0], 10, 32)
	if len(values) > 1 || parseErr != nil || errno == 0 {
		return fmt.Errorf("%w (its %s %q is no errno)", err, ErrnoKey, values)
	}
	return &callError{err: err, errno: uint32(errno)}
}

// callError is a failed call's error, err, and the errno it failed with, Linux's.
type callError struct {
	err   error
	errno uint32
}

func (e *callError) Error() string {
	return e.err.Error()
}

// Unwrap returns the call's error, and its errno as this system's, if it has that errno.
func (e *callError) Unwrap() []error {
	if errno, ok := localErrno(e.errno); ok {
		return []error{e.err, errno}
	}
	return []error{e.err}
}

// ErrnoOf returns the errno a call of a [Client] failed with, Linux's number, as the daemon said
// it, whatever system the client runs on; false if err is no such call's error, or one the daemon
// said no errno of.
func ErrnoOf(err error) (uint32, bool) {
	var failed *callError
	if !errors.As(err, &failed) {
		return 0, false
	}
	return failed.errno, true
}

// Linux's errnos, which the daemon fails with (crates/fxfs-posix's Errno, every one): what
// [ErrnoOf] returns, whatever system the client runs on.
const (
	EPERM           uint32 = 1
	ENOENT          uint32 = 2
	EIO             uint32 = 5
	E2BIG           uint32 = 7
	EBADF           uint32 = 9
	EAGAIN          uint32 = 11
	ENOMEM          uint32 = 12
	EACCES          uint32 = 13
	EBUSY           uint32 = 16
	EEXIST          uint32 = 17
	EXDEV           uint32 = 18
	ENOTDIR         uint32 = 20
	EISDIR          uint32 = 21
	EINVAL          uint32 = 22
	EMFILE          uint32 = 24
	EFBIG           uint32 = 27
	ENOSPC          uint32 = 28
	EROFS           uint32 = 30
	EMLINK          uint32 = 31
	EPIPE           uint32 = 32
	ERANGE          uint32 = 34
	ENAMETOOLONG    uint32 = 36
	ENOSYS          uint32 = 38
	ENOTEMPTY       uint32 = 39
	ENODATA         uint32 = 61
	EPROTO          uint32 = 71
	EBADMSG         uint32 = 74
	EOVERFLOW       uint32 = 75
	EMSGSIZE        uint32 = 90
	EPROTONOSUPPORT uint32 = 93
	ENOTSUP         uint32 = 95
	EADDRINUSE      uint32 = 98
	ENETUNREACH     uint32 = 101
	ECONNABORTED    uint32 = 103
	ECONNRESET      uint32 = 104
	ENOTCONN        uint32 = 107
	ETIMEDOUT       uint32 = 110
	ECONNREFUSED    uint32 = 111
	ESTALE          uint32 = 116
	EUCLEAN         uint32 = 117
)

// linuxErrnos are this system's errnos of Linux's that it has too, by Linux's numbers: Linux's
// own on Linux; on another system, as macOS, its own numbers, which differ from 35 on. EUCLEAN,
// which macOS has none of, is Linux's alone.
var linuxErrnos = map[uint32]syscall.Errno{
	EPERM:           syscall.EPERM,
	ENOENT:          syscall.ENOENT,
	EIO:             syscall.EIO,
	E2BIG:           syscall.E2BIG,
	EBADF:           syscall.EBADF,
	EAGAIN:          syscall.EAGAIN,
	ENOMEM:          syscall.ENOMEM,
	EACCES:          syscall.EACCES,
	EBUSY:           syscall.EBUSY,
	EEXIST:          syscall.EEXIST,
	EXDEV:           syscall.EXDEV,
	ENOTDIR:         syscall.ENOTDIR,
	EISDIR:          syscall.EISDIR,
	EINVAL:          syscall.EINVAL,
	EMFILE:          syscall.EMFILE,
	EFBIG:           syscall.EFBIG,
	ENOSPC:          syscall.ENOSPC,
	EROFS:           syscall.EROFS,
	EMLINK:          syscall.EMLINK,
	EPIPE:           syscall.EPIPE,
	ERANGE:          syscall.ERANGE,
	ENAMETOOLONG:    syscall.ENAMETOOLONG,
	ENOSYS:          syscall.ENOSYS,
	ENOTEMPTY:       syscall.ENOTEMPTY,
	ENODATA:         syscall.ENODATA,
	EPROTO:          syscall.EPROTO,
	EBADMSG:         syscall.EBADMSG,
	EOVERFLOW:       syscall.EOVERFLOW,
	EMSGSIZE:        syscall.EMSGSIZE,
	EPROTONOSUPPORT: syscall.EPROTONOSUPPORT,
	ENOTSUP:         syscall.ENOTSUP,
	EADDRINUSE:      syscall.EADDRINUSE,
	ENETUNREACH:     syscall.ENETUNREACH,
	ECONNABORTED:    syscall.ECONNABORTED,
	ECONNRESET:      syscall.ECONNRESET,
	ENOTCONN:        syscall.ENOTCONN,
	ETIMEDOUT:       syscall.ETIMEDOUT,
	ECONNREFUSED:    syscall.ECONNREFUSED,
	ESTALE:          syscall.ESTALE,
}

// localErrno returns Linux's errno linux as this system's, if this system has it: on Linux, any
// number, Linux's being its own; elsewhere, those of [linuxErrnos].
func localErrno(linux uint32) (syscall.Errno, bool) {
	if runtime.GOOS == "linux" {
		return syscall.Errno(linux), true
	}
	errno, ok := linuxErrnos[linux]
	return errno, ok
}

// GRPCStatus returns the call's status, as it is, for [status.FromError] and [status.Code].
func (e *callError) GRPCStatus() *status.Status {
	return status.Convert(e.err)
}
