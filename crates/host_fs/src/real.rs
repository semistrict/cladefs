// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: thin inline wrappers around `std::fs` for normal builds (`not(madsim)`), asserting in
//! debug and test builds that slow disk calls never run directly on an async runtime thread
//! (board 10824). In release builds the check compiles out and every call forwards directly.

use crate::blocking::assert_may_block;
use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::fs::{
    FileExt as StdFileExt, MetadataExt as StdMetadataExt, OpenOptionsExt as StdOpenOptionsExt,
};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What a file on the host's disk is: wraps `std::fs::FileType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType(std::fs::FileType);

impl FileType {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        self.0.is_file()
    }

    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.0.is_symlink()
    }
}

/// What `stat` says of a file on the host's disk: wraps `std::fs::Metadata`.
#[derive(Clone, Debug)]
pub struct Metadata(std::fs::Metadata);

impl Metadata {
    #[inline]
    pub fn file_type(&self) -> FileType {
        FileType(self.0.file_type())
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        self.0.is_file()
    }

    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.0.is_symlink()
    }

    #[inline]
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }

    #[inline]
    pub fn modified(&self) -> io::Result<SystemTime> {
        self.0.modified()
    }

    #[inline]
    pub fn permissions(&self) -> std::fs::Permissions {
        self.0.permissions()
    }
}

/// An entry of a directory on the host's disk ([`read_dir`]): wraps `std::fs::DirEntry`.
#[derive(Debug)]
pub struct DirEntry(std::fs::DirEntry);

impl DirEntry {
    #[inline]
    pub fn path(&self) -> PathBuf {
        self.0.path()
    }

    #[inline]
    pub fn file_name(&self) -> OsString {
        self.0.file_name()
    }

    #[inline]
    #[track_caller]
    pub fn file_type(&self) -> io::Result<FileType> {
        assert_may_block("DirEntry::file_type");
        self.0.file_type().map(FileType)
    }

    #[inline]
    #[track_caller]
    pub fn metadata(&self) -> io::Result<Metadata> {
        assert_may_block("DirEntry::metadata");
        self.0.metadata().map(Metadata)
    }
}

/// The entries of a directory on the host's disk ([`read_dir`]): wraps `std::fs::ReadDir`.
#[derive(Debug)]
pub struct ReadDir(std::fs::ReadDir);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    #[inline]
    #[track_caller]
    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        assert_may_block("ReadDir::next");
        self.0.next().map(|entry| entry.map(DirEntry))
    }
}

/// How a file on the host's disk is opened: wraps `std::fs::OpenOptions`.
#[derive(Clone, Debug)]
pub struct OpenOptions(std::fs::OpenOptions);

impl Default for OpenOptions {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    #[inline]
    pub fn new() -> Self {
        Self(std::fs::OpenOptions::new())
    }

    #[inline]
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.0.read(read);
        self
    }

    #[inline]
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.0.write(write);
        self
    }

    #[inline]
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.0.append(append);
        self
    }

    #[inline]
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.0.truncate(truncate);
        self
    }

    #[inline]
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.0.create(create);
        self
    }

    #[inline]
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.0.create_new(create_new);
        self
    }

    #[inline]
    #[track_caller]
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        assert_may_block("OpenOptions::open");
        self.0.open(path).map(File)
    }
}

/// An open file or directory on the host's disk: wraps `std::fs::File`.
#[derive(Debug)]
pub struct File(std::fs::File);

impl File {
    #[inline]
    #[track_caller]
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        assert_may_block("File::open");
        std::fs::File::open(path).map(Self)
    }

    #[inline]
    #[track_caller]
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        assert_may_block("File::create");
        std::fs::File::create(path).map(Self)
    }

    #[inline]
    #[track_caller]
    pub fn create_new(path: impl AsRef<Path>) -> io::Result<Self> {
        assert_may_block("File::create_new");
        std::fs::File::create_new(path).map(Self)
    }

    #[inline]
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    #[inline]
    #[track_caller]
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        assert_may_block("File::set_len");
        self.0.set_len(size)
    }

    #[inline]
    #[track_caller]
    pub fn sync_all(&self) -> io::Result<()> {
        assert_may_block("File::sync_all");
        self.0.sync_all()
    }

    #[inline]
    #[track_caller]
    pub fn sync_data(&self) -> io::Result<()> {
        assert_may_block("File::sync_data");
        self.0.sync_data()
    }

    #[inline]
    #[track_caller]
    pub fn metadata(&self) -> io::Result<Metadata> {
        assert_may_block("File::metadata");
        self.0.metadata().map(Metadata)
    }

    #[inline]
    #[track_caller]
    pub fn set_modified(&self, time: std::time::SystemTime) -> io::Result<()> {
        assert_may_block("File::set_modified");
        self.0.set_modified(time)
    }

    #[inline]
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }
}

impl AsRawFd for File {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for File {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Read for &File {
    #[inline]
    #[track_caller]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        assert_may_block("File::read");
        (&self.0).read(buf)
    }
}

impl Read for File {
    #[inline]
    #[track_caller]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        assert_may_block("File::read");
        self.0.read(buf)
    }
}

impl Write for &File {
    #[inline]
    #[track_caller]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        assert_may_block("File::write");
        (&self.0).write(buf)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }
}

impl Write for File {
    #[inline]
    #[track_caller]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        assert_may_block("File::write");
        self.0.write(buf)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Seek for &File {
    #[inline]
    #[track_caller]
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        assert_may_block("File::seek");
        (&self.0).seek(pos)
    }
}

impl Seek for File {
    #[inline]
    #[track_caller]
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        assert_may_block("File::seek");
        self.0.seek(pos)
    }
}

#[inline]
#[track_caller]
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    assert_may_block("canonicalize");
    std::fs::canonicalize(path)
}

#[inline]
#[track_caller]
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    assert_may_block("copy");
    std::fs::copy(from, to)
}

#[inline]
#[track_caller]
pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("create_dir");
    std::fs::create_dir(path)
}

#[inline]
#[track_caller]
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("create_dir_all");
    std::fs::create_dir_all(path)
}

#[inline]
#[track_caller]
pub fn hard_link(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("hard_link");
    std::fs::hard_link(original, link)
}

#[inline]
#[track_caller]
pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    assert_may_block("metadata");
    std::fs::metadata(path).map(Metadata)
}

#[inline]
#[track_caller]
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    assert_may_block("symlink_metadata");
    std::fs::symlink_metadata(path).map(Metadata)
}

#[inline]
#[track_caller]
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    assert_may_block("read");
    std::fs::read(path)
}

#[inline]
#[track_caller]
pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    assert_may_block("read_dir");
    std::fs::read_dir(path).map(ReadDir)
}

#[inline]
#[track_caller]
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    assert_may_block("read_to_string");
    std::fs::read_to_string(path)
}

#[inline]
#[track_caller]
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("remove_dir");
    std::fs::remove_dir(path)
}

#[inline]
#[track_caller]
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("remove_dir_all");
    std::fs::remove_dir_all(path)
}

#[inline]
#[track_caller]
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("remove_file");
    std::fs::remove_file(path)
}

#[inline]
#[track_caller]
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    assert_may_block("rename");
    std::fs::rename(from, to)
}

#[inline]
#[track_caller]
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    assert_may_block("write");
    std::fs::write(path, contents)
}

/// `std::os::unix::fs::FileExt`'s calls, checked against running on an async runtime thread.
pub trait FileExt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
}

impl FileExt for File {
    #[inline]
    #[track_caller]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        assert_may_block("FileExt::read_at");
        StdFileExt::read_at(&self.0, buf, offset)
    }

    #[inline]
    #[track_caller]
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        assert_may_block("FileExt::read_exact_at");
        StdFileExt::read_exact_at(&self.0, buf, offset)
    }

    #[inline]
    #[track_caller]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        assert_may_block("FileExt::write_at");
        StdFileExt::write_at(&self.0, buf, offset)
    }

    #[inline]
    #[track_caller]
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        assert_may_block("FileExt::write_all_at");
        StdFileExt::write_all_at(&self.0, buf, offset)
    }
}

/// `std::os::unix::fs::MetadataExt`'s calls that cladefsd uses.
pub trait MetadataExt {
    fn dev(&self) -> u64;
    fn ino(&self) -> u64;
    /// Of 512 bytes.
    fn blocks(&self) -> u64;
}

impl MetadataExt for Metadata {
    #[inline]
    fn dev(&self) -> u64 {
        StdMetadataExt::dev(&self.0)
    }

    #[inline]
    fn ino(&self) -> u64 {
        StdMetadataExt::ino(&self.0)
    }

    #[inline]
    fn blocks(&self) -> u64 {
        StdMetadataExt::blocks(&self.0)
    }
}

/// `std::os::unix::fs::OpenOptionsExt`'s calls that cladefsd uses.
pub trait OpenOptionsExt {
    fn custom_flags(&mut self, flags: i32) -> &mut Self;
    fn mode(&mut self, mode: u32) -> &mut Self;
}

impl OpenOptionsExt for OpenOptions {
    #[inline]
    fn custom_flags(&mut self, flags: i32) -> &mut Self {
        StdOpenOptionsExt::custom_flags(&mut self.0, flags);
        self
    }

    #[inline]
    fn mode(&mut self, mode: u32) -> &mut Self {
        StdOpenOptionsExt::mode(&mut self.0, mode);
        self
    }
}
