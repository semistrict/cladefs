// SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
// Copyright 2026 The cladefs Authors.
/*
 * Fxfs's VFS glue: the kernel's superblock, inode, directory and file operations, over the
 * filesystem in Rust (cladefs.h).
 *
 * Rust's allocator may reclaim memory, which must not recurse into filesystems, so every call into
 * Rust runs in a NOFS allocation scope.
 *
 * Files' data is in the device's DAX window, if it has one (dax.c), as the host shares it: one copy
 * of it in memory. Otherwise it is in the page cache, which writeback writes to Fxfs, contiguous
 * dirty folios in one write, and reads fill, readahead's folios in one read. The kernel keeps
 * inodes' times, which write_inode writes back.
 */

#include <linux/backing-dev.h>
#include <linux/dax.h>
#include <linux/exportfs.h>
#include <linux/fiemap.h>
#include <linux/sizes.h>
#include <linux/fileattr.h>
#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/fs_parser.h>
#include <linux/init.h>
#include <linux/minmax.h>
#include <linux/module.h>
#include <linux/namei.h>
#include <linux/pagemap.h>
#include <linux/posix_acl.h>
#include <linux/posix_acl_xattr.h>
#include <linux/proc_fs.h>
#include <linux/sched/mm.h>
#include <linux/seq_file.h>
#include <linux/slab.h>
#include <linux/statfs.h>
#include <linux/sysfs.h>
#include <linux/uio.h>
#include <linux/virtio_extent.h>
#include <linux/workqueue.h>
#include <linux/writeback.h>
#include <linux/xattr.h>

#include "device.h"
#include "cladefs.h"
#include "cladefs_trace.h"
#include "rt.h"
#include "vfs.h"

#define CLADEFS_MAGIC 0x46784673 /* "FxFs" */

/* The most bytes a read or write moves to or from Rust at once. */
#define CLADEFS_IO_MAX (1 << 20)


static const struct inode_operations cladefs_vfs_dir_inode_operations;
static const struct inode_operations cladefs_vfs_file_inode_operations;
static const struct inode_operations cladefs_vfs_symlink_inode_operations;
static const struct inode_operations cladefs_vfs_special_inode_operations;
static const struct file_operations cladefs_vfs_dir_operations;
static const struct file_operations cladefs_vfs_file_operations;
static const struct address_space_operations cladefs_vfs_aops;

static struct kmem_cache *cladefs_vfs_inode_cache;

/*
 * The attribute flags (chattr) Fxfs keeps, the bits of its mode it keeps them in, and the kernel's
 * inode flags each sets.
 */
static const struct {
	u32 fs;
	u32 mode;
	unsigned int i_flag;
} cladefs_vfs_flags[] = {
	{ FS_IMMUTABLE_FL, CLADEFS_MODE_IMMUTABLE, S_IMMUTABLE },
	{ FS_APPEND_FL, CLADEFS_MODE_APPEND, S_APPEND },
	{ FS_NOATIME_FL, CLADEFS_MODE_NOATIME, S_NOATIME },
	{ FS_NODUMP_FL, CLADEFS_MODE_NODUMP, 0 },
};
#define CLADEFS_VFS_FL (FS_IMMUTABLE_FL | FS_APPEND_FL | FS_NOATIME_FL | FS_NODUMP_FL)

/* Sets `inode`'s attribute flags to `fsflags` (FS_*_FL), and the kernel's flags they imply. */
static void cladefs_vfs_set_flags(struct inode *inode, u32 fsflags)
{
	unsigned int i_flags = 0;
	size_t i;

	for (i = 0; i < ARRAY_SIZE(cladefs_vfs_flags); i++)
		if (fsflags & cladefs_vfs_flags[i].fs)
			i_flags |= cladefs_vfs_flags[i].i_flag;
	WRITE_ONCE(cladefs_vfs_inode(inode)->fsflags, fsflags);
	inode_set_flags(inode, i_flags, S_IMMUTABLE | S_APPEND | S_NOATIME);
}

/* The attribute flags (FS_*_FL) Fxfs's `mode` keeps. */
static u32 cladefs_vfs_mode_flags(u32 mode)
{
	u32 fsflags = 0;
	size_t i;

	for (i = 0; i < ARRAY_SIZE(cladefs_vfs_flags); i++)
		if (mode & cladefs_vfs_flags[i].mode)
			fsflags |= cladefs_vfs_flags[i].fs;
	return fsflags;
}

/* Fxfs's mode of `inode`: the kernel's, with its attribute flags above. */
static u32 cladefs_vfs_mode(struct inode *inode, u32 fsflags)
{
	u32 mode = inode->i_mode;
	size_t i;

	for (i = 0; i < ARRAY_SIZE(cladefs_vfs_flags); i++)
		if (fsflags & cladefs_vfs_flags[i].fs)
			mode |= cladefs_vfs_flags[i].mode;
	return mode;
}

/*
 * Sets `inode`'s attributes to `attr`, as Fxfs has them. A regular file's size and data's times
 * are the page cache's once its inode is read in, and Fxfs's lag them until writeback: only a new
 * inode takes them (cladefs_vfs_update_attr).
 */
static void cladefs_vfs_set_attr(struct inode *inode, const struct cladefs_attr *attr)
{
	inode->i_mode = attr->mode & (S_IFMT | S_IALLUGO);
	cladefs_vfs_set_flags(inode, cladefs_vfs_mode_flags(attr->mode));
	set_nlink(inode, attr->nlink);
	i_uid_write(inode, attr->uid);
	i_gid_write(inode, attr->gid);
	i_size_write(inode, attr->size);
	inode->i_blocks = attr->blocks;
	inode_set_atime_to_ts(inode, ns_to_timespec64(attr->atime_ns));
	inode_set_mtime_to_ts(inode, ns_to_timespec64(attr->mtime_ns));
	inode_set_ctime_to_ts(inode, ns_to_timespec64(attr->ctime_ns));
}

/* The inode `attr` describes, read in if it is not cached, or cladefs: `created` just now. */
static struct inode *cladefs_vfs_iget_as(struct super_block *sb, const struct cladefs_attr *attr,
				      bool created)
{
	struct inode *inode = iget_locked(sb, attr->ino);

	if (!inode)
		return ERR_PTR(-ENOMEM);
	if (!(inode->i_state & I_NEW))
		return inode;
	cladefs_vfs_set_attr(inode, attr);
	switch (attr->mode & S_IFMT) {
	case S_IFDIR:
		inode->i_op = &cladefs_vfs_dir_inode_operations;
		inode->i_fop = &cladefs_vfs_dir_operations;
		break;
	case S_IFREG:
		inode->i_op = &cladefs_vfs_file_inode_operations;
		inode->i_fop = &cladefs_vfs_file_operations;
		inode->i_mapping->a_ops = &cladefs_vfs_aops;
		/*
		 * Small files too: they share extents, and the window's mappings of them. cladefs:
		 * not those created in this mount (cladefs_dax_set_inode, TASK-164.3).
		 */
		if (cladefs_vfs_info(sb)->dax && cladefs_dax_set_inode(inode, created))
			break;
		/*
		 * In the page cache, in large folios, up to what one write to Fxfs takes
		 * (cladefs_vfs_writepages).
		 */
		mapping_set_folio_order_range(inode->i_mapping, 0, ilog2(CLADEFS_IO_MAX / PAGE_SIZE));
		break;
	case S_IFLNK:
		inode->i_op = &cladefs_vfs_symlink_inode_operations;
		break;
	default:
		inode->i_op = &cladefs_vfs_special_inode_operations;
		init_special_inode(inode, inode->i_mode, new_decode_dev(attr->rdev));
		break;
	}
	unlock_new_inode(inode);
	return inode;
}

/* The inode `attr` describes, read in if it is not cached. */
static struct inode *cladefs_vfs_iget(struct super_block *sb, const struct cladefs_attr *attr)
{
	return cladefs_vfs_iget_as(sb, attr, false);
}

/*
 * Updates `inode`'s attributes after one of Fxfs's operations on names changed them: its link
 * count and change time, and a directory's modification time. The rest are the kernel's once the
 * inode is read in, which changes them (setattr) and writes them back (write_inode).
 */
static void cladefs_vfs_update_attr(struct inode *inode, const struct cladefs_attr *attr)
{
	set_nlink(inode, attr->nlink);
	inode_set_ctime_to_ts(inode, ns_to_timespec64(attr->ctime_ns));
	if (S_ISDIR(inode->i_mode))
		inode_set_mtime_to_ts(inode, ns_to_timespec64(attr->mtime_ns));
}

/*
 * Writes what the kernel keeps of `inode` to Fxfs: its mode, owner and times, and with
 * CLADEFS_SET_SIZE in `valid`, its size.
 */
static int cladefs_vfs_store_attr(struct inode *inode, u32 valid, u32 flags, struct timespec64 ctime)
{
	struct timespec64 atime = inode_get_atime(inode), mtime = inode_get_mtime(inode);
	struct cladefs_setattr set = {
		.valid = valid | CLADEFS_SET_MODE | CLADEFS_SET_UID | CLADEFS_SET_GID | CLADEFS_SET_ATIME |
			 CLADEFS_SET_MTIME | CLADEFS_SET_CTIME,
		.mode = cladefs_vfs_mode(inode, flags),
		.uid = i_uid_read(inode),
		.gid = i_gid_read(inode),
		.size = i_size_read(inode),
		.atime_ns = timespec64_to_ns(&atime),
		.mtime_ns = timespec64_to_ns(&mtime),
		.ctime_ns = timespec64_to_ns(&ctime),
	};

	return cladefs_nofs(cladefs_write_inode(cladefs_vfs_fs(inode->i_sb), inode->i_ino, &set));
}

int cladefs_vfs_write_attr(struct inode *inode, u32 valid)
{
	struct cladefs_vfs_inode *fi = cladefs_vfs_inode(inode);
	int err;

	mutex_lock(&fi->attr_lock);
	err = cladefs_vfs_store_attr(inode, valid, fi->fsflags, inode_get_ctime(inode));
	mutex_unlock(&fi->attr_lock);
	return err;
}

static struct dentry *cladefs_vfs_lookup(struct inode *dir, struct dentry *dentry,
				      unsigned int flags)
{
	struct cladefs_attr attr;
	struct inode *inode = NULL;
	int err;

	/* Which keeps names longer than Linux's out of Fxfs, which has no limit of its own. */
	if (dentry->d_name.len > NAME_MAX)
		return ERR_PTR(-ENAMETOOLONG);
	err = cladefs_nofs(cladefs_lookup(cladefs_vfs_fs(dir->i_sb), dir->i_ino, dentry->d_name.name,
				    dentry->d_name.len, &attr));
	if (!err)
		inode = cladefs_vfs_iget(dir->i_sb, &attr);
	if (err && err != -ENOENT)
		return ERR_PTR(err);
	return d_splice_alias(inode, dentry);
}

/* cladefs: the attribute flags statx says of `inode`, as lsattr does (xfstests generic/424). */
static void cladefs_vfs_stat_flags(struct inode *inode, struct kstat *stat)
{
	u32 fsflags = cladefs_vfs_inode(inode)->fsflags;

	if (fsflags & FS_APPEND_FL)
		stat->attributes |= STATX_ATTR_APPEND;
	if (fsflags & FS_IMMUTABLE_FL)
		stat->attributes |= STATX_ATTR_IMMUTABLE;
	if (fsflags & FS_NODUMP_FL)
		stat->attributes |= STATX_ATTR_NODUMP;
	stat->attributes_mask |= STATX_ATTR_APPEND | STATX_ATTR_IMMUTABLE | STATX_ATTR_NODUMP;
}

/*
 * cladefs: reads a file's blocks again if a commit has been made since they last were: Fxfs's
 * writeback, seals and moves change them, each in a commit, none of which is the kernel's to see.
 * `du` of a file in the inode cache saw what it had as read in: none, of one created here (TASK-138,
 * xfstests generic/615).
 */
static int cladefs_vfs_refresh_blocks(struct inode *inode)
{
	struct cladefs_vfs_inode *fi = cladefs_vfs_inode(inode);
	struct cladefs *fs = cladefs_vfs_fs(inode->i_sb);
	u64 offset = cladefs_journal_offset(fs);
	struct cladefs_attr attr;
	int err;

	if (READ_ONCE(fi->blocks_as_of) == offset)
		return 0;
	err = cladefs_nofs(cladefs_attr_of_open(fs, inode->i_ino, &attr));
	if (err)
		return err;
	inode->i_blocks = attr.blocks;
	WRITE_ONCE(fi->blocks_as_of, offset);
	return 0;
}

/*
 * cladefs: an inode's attributes, the kernel's but for a file's blocks. A file with fewer blocks than
 * bytes may have dirty pages over its holes, which have no blocks until written back, and which take
 * new ones Fxfs alone knows: it looked sparser than it is to whatever goes by its blocks, as a backup
 * does, so it is written back first (xfstests generic/422, as NFS does for a getattr). A file with
 * all its blocks needs no writeback: rewriting pages leaves their number as it is.
 */
static int cladefs_vfs_getattr(struct mnt_idmap *idmap, const struct path *path, struct kstat *stat,
			    u32 request_mask, unsigned int flags)
{
	struct inode *inode = d_inode(path->dentry);

	if (S_ISREG(inode->i_mode) && request_mask & STATX_BLOCKS &&
	    !(flags & AT_STATX_DONT_SYNC)) {
		int err = cladefs_vfs_refresh_blocks(inode);

		if (!err && !IS_DAX(inode) && inode_get_bytes(inode) < i_size_read(inode) &&
		    mapping_tagged(inode->i_mapping, PAGECACHE_TAG_DIRTY)) {
			err = filemap_write_and_wait(inode->i_mapping);
			if (!err)
				err = cladefs_vfs_refresh_blocks(inode);
		}
		if (err)
			return err;
	}
	generic_fillattr(idmap, request_mask, inode, stat);
	cladefs_vfs_stat_flags(inode, stat);
	return 0;
}

/*
 * Instantiates `dentry`, a new name in `dir`, with the inode `attr` describes, and `dir` as
 * `dir_attr` describes it: or, without `dir_attr`, as Fxfs left it, but for its times, which are
 * the kernel's, written back with it (write_inode).
 */
static int cladefs_vfs_instantiate(struct inode *dir, struct dentry *dentry,
				const struct cladefs_attr *attr, const struct cladefs_attr *dir_attr)
{
	struct inode *inode = cladefs_vfs_iget_as(dir->i_sb, attr, true);

	if (dir_attr) {
		cladefs_vfs_update_attr(dir, dir_attr);
	} else {
		inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));
		mark_inode_dirty(dir);
	}
	if (IS_ERR(inode))
		return PTR_ERR(inode);
	d_instantiate(dentry, inode);
	return 0;
}

/* The owner of a new inode in `dir`: the caller, as the mount's `idmap` maps it. */
static u32 cladefs_vfs_new_uid(struct mnt_idmap *idmap, struct inode *dir)
{
	return from_kuid(&init_user_ns, mapped_fsuid(idmap, i_user_ns(dir)));
}

/*
 * The group a new inode in `dir` belongs to, as inode_init_owner has it: in a set-group-ID
 * directory, the directory's, and a new directory is set-group-ID too; otherwise the caller's, as
 * the mount's `idmap` maps it.
 */
static u32 cladefs_vfs_new_gid(struct mnt_idmap *idmap, struct inode *dir, umode_t *mode)
{
	if (!(dir->i_mode & S_ISGID))
		return from_kgid(&init_user_ns, mapped_fsgid(idmap, i_user_ns(dir)));
	if (S_ISDIR(*mode))
		*mode |= S_ISGID;
	return from_kgid(&init_user_ns, dir->i_gid);
}

/* The extended attribute an ACL of `type` is stored as. */
static const char *cladefs_vfs_acl_name(int type)
{
	switch (type) {
	case ACL_TYPE_ACCESS:
		return XATTR_NAME_POSIX_ACL_ACCESS;
	case ACL_TYPE_DEFAULT:
		return XATTR_NAME_POSIX_ACL_DEFAULT;
	default:
		return NULL;
	}
}

/* `inode`'s POSIX ACL of `type`, stored as an extended attribute, as btrfs stores it. */
static struct posix_acl *cladefs_vfs_get_acl(struct inode *inode, int type, bool rcu)
{
	struct cladefs *fs = cladefs_vfs_fs(inode->i_sb);
	const char *name = cladefs_vfs_acl_name(type);
	struct posix_acl *acl;
	ssize_t size;
	void *value;

	if (rcu)
		return ERR_PTR(-ECHILD);
	if (!name)
		return ERR_PTR(-EINVAL);
	size = cladefs_nofs(cladefs_getxattr(fs, inode->i_ino, name, strlen(name), NULL, 0));
	value = size > 0 ? kmalloc(size, GFP_KERNEL) : NULL;
	if (value)
		size = cladefs_nofs(cladefs_getxattr(fs, inode->i_ino, name, strlen(name), value, size));
	if (size == -ENODATA || size == 0) {
		kfree(value);
		return NULL;
	}
	if (size < 0) {
		kfree(value);
		return ERR_PTR(size);
	}
	if (!value)
		return ERR_PTR(-ENOMEM);
	if (size > 0)
		acl = posix_acl_from_xattr(&init_user_ns, value, size);
	else
		acl = size == -ENODATA || size == 0 ? NULL : ERR_PTR(size);
	kfree(value);
	return acl;
}

/*
 * Stores `acl`, or removes it if NULL, as `inode`'s ACL of `type`: in Fxfs at once, as its other
 * extended attributes are (cladefs_setxattr), -ENOSPC if it has no room for it; and marks the inode
 * dirty, its change time to write back.
 */
static int cladefs_vfs_store_acl(struct inode *inode, struct posix_acl *acl, int type)
{
	struct cladefs *fs = cladefs_vfs_fs(inode->i_sb);
	const char *name = cladefs_vfs_acl_name(type);
	size_t size = 0;
	void *value;
	int err;

	if (!name)
		return -EINVAL;
	if (type == ACL_TYPE_DEFAULT && !S_ISDIR(inode->i_mode))
		return acl ? -EACCES : 0;
	if (acl) {
		size = posix_acl_xattr_size(acl->a_count);
		value = kmalloc(size, GFP_KERNEL);
		if (!value)
			return -ENOMEM;
		err = posix_acl_to_xattr(&init_user_ns, acl, value, size);
		if (err >= 0)
			err = cladefs_nofs(cladefs_setxattr(fs, inode->i_ino, name, strlen(name), value,
						      size, 0));
		kfree(value);
	} else {
		err = cladefs_nofs(cladefs_removexattr(fs, inode->i_ino, name, strlen(name)));
		if (err == -ENODATA)
			err = 0;
	}
	if (err)
		return err;
	set_cached_acl(inode, type, acl);
	mark_inode_dirty(inode);
	return 0;
}

static int cladefs_vfs_set_acl(struct mnt_idmap *idmap, struct dentry *dentry, struct posix_acl *acl,
			    int type)
{
	struct inode *inode = d_inode(dentry);
	umode_t old_mode = inode->i_mode;
	int err;

	if (type == ACL_TYPE_ACCESS && acl) {
		err = posix_acl_update_mode(idmap, inode, &inode->i_mode, &acl);
		if (err)
			return err;
	}
	err = cladefs_vfs_store_acl(inode, acl, type);
	if (err) {
		inode->i_mode = old_mode;
		return err;
	}
	inode_set_ctime_current(inode);
	return 0;
}

/*
 * The ACLs a new inode in `dir` inherits, if its default ACL says, and its mode, which they, or
 * the umask without them, restrict (posix_acl_create): with SB_POSIXACL, the VFS leaves the umask
 * to the filesystem.
 */
static int cladefs_vfs_new_acls(struct inode *dir, umode_t *mode, struct posix_acl **default_acl,
			     struct posix_acl **acl)
{
	if (S_ISLNK(*mode)) {
		*default_acl = NULL;
		*acl = NULL;
		return 0;
	}
	return posix_acl_create(dir, mode, default_acl, acl);
}

/* Stores the ACLs a new inode inherited, and releases them. */
static int cladefs_vfs_init_acls(struct inode *inode, struct posix_acl *default_acl,
			      struct posix_acl *acl)
{
	int err = 0;

	if (default_acl) {
		err = cladefs_vfs_store_acl(inode, default_acl, ACL_TYPE_DEFAULT);
		posix_acl_release(default_acl);
	}
	if (acl) {
		if (!err)
			err = cladefs_vfs_store_acl(inode, acl, ACL_TYPE_ACCESS);
		posix_acl_release(acl);
	}
	return err;
}

/*
 * cladefs: `acl` as an extended attribute of an inode of `mode`, in `xattrs[*nr]`, its value in
 * `*value`, which the caller frees: the default ACL for directories alone.
 */
static int cladefs_vfs_acl_xattr(struct posix_acl *acl, int type, umode_t mode,
			      struct cladefs_xattr *xattrs, size_t *nr, void **value)
{
	const char *name = cladefs_vfs_acl_name(type);
	size_t size;
	int err;

	if (!acl || (type == ACL_TYPE_DEFAULT && !S_ISDIR(mode)))
		return 0;
	size = posix_acl_xattr_size(acl->a_count);
	*value = kmalloc(size, GFP_KERNEL);
	if (!*value)
		return -ENOMEM;
	err = posix_acl_to_xattr(&init_user_ns, acl, *value, size);
	if (err < 0)
		return err;
	xattrs[(*nr)++] = (struct cladefs_xattr) {
		.name = name, .name_len = strlen(name), .value = *value, .value_len = size,
	};
	return 0;
}

/*
 * cladefs: a directory's entries, all of them, packed as a cursor's buffer packs them, kept with its
 * inode while the kernel caches it, as ext4's directory blocks are kept in the page cache: a read
 * from the start takes them rather than seeking each of Fxfs's layers and decoding its records,
 * and allocating a cursor's buffer: a 91k-file tree's 6,059 directories, warm, took 108 ms to walk
 * that way against ext4 on virtio-pmem's 83 ms, and 62 ms kept (GCE r904, r905, TASK-230). The
 * kernel holds the directory's lock exclusively as its entries change, and shared as they are
 * read: they are kept as a read under the shared lock found them, and dropped as a change begins
 * (cladefs_vfs_dir_changes).
 */
struct cladefs_vfs_dir_listing {
	refcount_t refs;
	size_t len;
	char entries[];
};

static void cladefs_vfs_dir_put_listing(struct cladefs_vfs_dir_listing *listing)
{
	if (listing && refcount_dec_and_test(&listing->refs))
		kvfree(listing);
}

/* The directory's listing, if it has one, for the caller to put. */
static struct cladefs_vfs_dir_listing *cladefs_vfs_dir_get_listing(struct inode *inode)
{
	struct cladefs_vfs_dir_listing *listing;

	spin_lock(&inode->i_lock);
	listing = cladefs_vfs_inode(inode)->listing;
	if (listing)
		refcount_inc(&listing->refs);
	spin_unlock(&inode->i_lock);
	return listing;
}

/* Drops the directory's listing, as its entries change or it is evicted. */
static void cladefs_vfs_dir_changes(struct inode *inode)
{
	struct cladefs_vfs_dir_listing *listing;

	spin_lock(&inode->i_lock);
	listing = cladefs_vfs_inode(inode)->listing;
	cladefs_vfs_inode(inode)->listing = NULL;
	spin_unlock(&inode->i_lock);
	cladefs_vfs_dir_put_listing(listing);
}

/* Creates `dentry` in `dir` as `mode` says, owned by the caller. */
static int cladefs_vfs_mknod(struct mnt_idmap *idmap, struct inode *dir, struct dentry *dentry,
			  umode_t mode, dev_t rdev)
{
	struct posix_acl *default_acl, *acl;
	struct cladefs_attr attr, dir_attr;
	struct cladefs_xattr xattrs[2];
	void *values[2] = {};
	size_t nr_xattrs = 0;
	u32 gid;
	int err;

	cladefs_vfs_dir_changes(dir);
	err = cladefs_vfs_new_acls(dir, &mode, &default_acl, &acl);
	if (err)
		return err;
	gid = cladefs_vfs_new_gid(idmap, dir, &mode);
	/* cladefs: the ACLs it inherits, made with it (TASK-79.29). */
	err = cladefs_vfs_acl_xattr(default_acl, ACL_TYPE_DEFAULT, mode, xattrs, &nr_xattrs,
				 &values[0]);
	if (!err)
		err = cladefs_vfs_acl_xattr(acl, ACL_TYPE_ACCESS, mode, xattrs, &nr_xattrs, &values[1]);
	if (!err)
		err = cladefs_nofs(cladefs_create(cladefs_vfs_fs(dir->i_sb), dir->i_ino, dentry->d_name.name,
					    dentry->d_name.len, mode, new_encode_dev(rdev),
					    cladefs_vfs_new_uid(idmap, dir), gid, xattrs, nr_xattrs,
					    &attr, &dir_attr));
	kfree(values[0]);
	kfree(values[1]);
	/* A directory's times Fxfs sets as it creates a directory, which counts in its links. */
	if (!err)
		err = cladefs_vfs_instantiate(dir, dentry, &attr, S_ISDIR(mode) ? &dir_attr : NULL);
	if (err) {
		posix_acl_release(default_acl);
		posix_acl_release(acl);
		return err;
	}
	/*
	 * No extended attribute a write would remove (security.capability): its first write needn't
	 * look for one (file_remove_privs). Setting one clears S_NOSEC.
	 */
	inode_has_no_xattr(d_inode(dentry));
	/* cladefs: its ACLs, written with it, cached as cladefs_vfs_store_acl caches them. */
	if (default_acl && S_ISDIR(mode))
		set_cached_acl(d_inode(dentry), ACL_TYPE_DEFAULT, default_acl);
	if (acl)
		set_cached_acl(d_inode(dentry), ACL_TYPE_ACCESS, acl);
	posix_acl_release(default_acl);
	posix_acl_release(acl);
	return 0;
}

/* Creates an unnamed regular file in `dir`, which a link may name later (O_TMPFILE). */
static int cladefs_vfs_tmpfile(struct mnt_idmap *idmap, struct inode *dir, struct file *file,
			    umode_t mode)
{
	struct posix_acl *default_acl, *acl;
	struct cladefs_attr attr;
	struct inode *inode;
	u32 gid;
	int err;

	err = cladefs_vfs_new_acls(dir, &mode, &default_acl, &acl);
	if (err)
		return err;
	gid = cladefs_vfs_new_gid(idmap, dir, &mode);
	err = cladefs_nofs(cladefs_tmpfile(cladefs_vfs_fs(dir->i_sb), dir->i_ino, mode,
				     cladefs_vfs_new_uid(idmap, dir), gid, &attr));
	if (err) {
		posix_acl_release(default_acl);
		posix_acl_release(acl);
		return err;
	}
	inode = cladefs_vfs_iget_as(dir->i_sb, &attr, true);
	if (IS_ERR(inode)) {
		posix_acl_release(default_acl);
		posix_acl_release(acl);
		return PTR_ERR(inode);
	}
	/* Its one reference, the graveyard's, which the kernel doesn't count as a link. */
	d_tmpfile(file, inode);
	/* As cladefs_vfs_mknod's. */
	inode_has_no_xattr(inode);
	err = cladefs_vfs_init_acls(inode, default_acl, acl);
	if (err)
		return err;
	return finish_open_simple(file, 0);
}

static int cladefs_vfs_create(struct mnt_idmap *idmap, struct inode *dir, struct dentry *dentry,
			   umode_t mode, bool excl)
{
	return cladefs_vfs_mknod(idmap, dir, dentry, mode | S_IFREG, 0);
}

/* The directory, in `dentry`: NULL, the dentry made, or the error. */
static struct dentry *cladefs_vfs_mkdir(struct mnt_idmap *idmap, struct inode *dir,
				     struct dentry *dentry, umode_t mode)
{
	return ERR_PTR(cladefs_vfs_mknod(idmap, dir, dentry, mode | S_IFDIR, 0));
}

static int cladefs_vfs_symlink(struct mnt_idmap *idmap, struct inode *dir, struct dentry *dentry,
			    const char *target)
{
	umode_t mode = S_IFLNK | 0777;
	u32 gid = cladefs_vfs_new_gid(idmap, dir, &mode);
	struct cladefs_attr attr, dir_attr;
	int err;

	if (strlen(target) > CLADEFS_SYMLINK_MAX)
		return -ENAMETOOLONG;
	cladefs_vfs_dir_changes(dir);
	err = cladefs_nofs(cladefs_symlink(cladefs_vfs_fs(dir->i_sb), dir->i_ino, dentry->d_name.name,
					 dentry->d_name.len, target, strlen(target),
					 cladefs_vfs_new_uid(idmap, dir), gid, &attr,
					 &dir_attr));

	if (err)
		return err;
	return cladefs_vfs_instantiate(dir, dentry, &attr, &dir_attr);
}

static int cladefs_vfs_link(struct dentry *old_dentry, struct inode *dir, struct dentry *dentry)
{
	struct inode *inode = d_inode(old_dentry);
	struct cladefs_attr attr, dir_attr;
	/* An O_TMPFILE file, linked for the first time, leaves the graveyard. */
	bool unnamed = inode->i_nlink == 0;
	int err;

	cladefs_vfs_dir_changes(dir);
	err = cladefs_nofs(cladefs_link(cladefs_vfs_fs(dir->i_sb), inode->i_ino, dir->i_ino,
				  dentry->d_name.name, dentry->d_name.len, unnamed, &attr, &dir_attr));
	if (err)
		return err;
	cladefs_vfs_update_attr(inode, &attr);
	cladefs_vfs_update_attr(dir, &dir_attr);
	ihold(inode);
	d_instantiate(dentry, inode);
	return 0;
}

/*
 * Accounts for what removing or replacing a name took from `inode`, which `replaced`
 * (CLADEFS_REPLACED_*) says: a link, or a directory, removed. Its inode, unlinked, is freed once the
 * kernel is done with it (cladefs_vfs_evict_inode): until then, its times and extended attributes are
 * written back to Fxfs as any inode's (write_inode), as overlayfs leaves its work directory's.
 */
static void cladefs_vfs_removed(struct inode *inode, int replaced)
{
	if (!inode || replaced == CLADEFS_REPLACED_NONE)
		return;
	if (replaced == CLADEFS_REPLACED_DIRECTORY)
		clear_nlink(inode);
	else
		drop_nlink(inode);
	inode_set_ctime_current(inode);
	/* The ctime is Fxfs's too, for an inode other names still link (write_inode). */
	if (inode->i_nlink)
		mark_inode_dirty(inode);
}

/*
 * Removes `dentry`'s name from `dir`. A file's, symlink's or special file's goes later, in a batch
 * with others (cladefs_unlink_later): the kernel holds `dir`'s lock meanwhile, and has checked what
 * could fail. A directory's goes now: only Fxfs knows whether it is empty.
 */
static int cladefs_vfs_unlink(struct inode *dir, struct dentry *dentry)
{
	struct inode *inode = d_inode(dentry);
	struct cladefs *fs = cladefs_vfs_fs(dir->i_sb);
	struct cladefs_attr dir_attr;
	int replaced;

	cladefs_vfs_dir_changes(dir);
	if (!S_ISDIR(inode->i_mode)) {
		int err = cladefs_nofs(cladefs_unlink_later(fs, dir->i_ino, dentry->d_name.name,
						      dentry->d_name.len, inode->i_ino));

		if (err)
			return err;
		cladefs_vfs_removed(inode, CLADEFS_REPLACED_OBJECT);
		/*
		 * Written back with the directory (write_inode), or a file's fsync in it: the batch
		 * leaves them be. Unlike ext4's, they aren't committed with the name: after a crash the
		 * directory's times may be older than the change.
		 */
		inode_set_mtime_to_ts(dir, inode_set_ctime_current(dir));
		mark_inode_dirty(dir);
		return 0;
	}
	replaced = cladefs_nofs(cladefs_unlink(fs, dir->i_ino, dentry->d_name.name, dentry->d_name.len,
					 inode->i_ino, &dir_attr));
	if (replaced < 0)
		return replaced;
	cladefs_vfs_removed(inode, replaced);
	cladefs_vfs_update_attr(dir, &dir_attr);
	return 0;
}

static int cladefs_vfs_rename(struct mnt_idmap *idmap, struct inode *old_dir,
			   struct dentry *old_dentry, struct inode *new_dir,
			   struct dentry *new_dentry, unsigned int flags)
{
	struct inode *inode = d_inode(old_dentry), *target = d_inode(new_dentry);
	struct cladefs_attr attr, new_attr, dir_attr, new_dir_attr;
	struct cladefs *fs = cladefs_vfs_fs(old_dir->i_sb);
	int replaced, err;

	cladefs_vfs_dir_changes(old_dir);
	cladefs_vfs_dir_changes(new_dir);
	if (flags & RENAME_EXCHANGE) {
		err = cladefs_nofs(cladefs_exchange(fs, old_dir->i_ino, old_dentry->d_name.name,
					      old_dentry->d_name.len, new_dir->i_ino,
					      new_dentry->d_name.name, new_dentry->d_name.len, &attr,
					      &new_attr, &dir_attr, &new_dir_attr));
		if (err)
			return err;
		cladefs_vfs_update_attr(target, &new_attr);
	} else {
		/* The VFS checks RENAME_NOREPLACE; no other flag is Fxfs's. */
		if (flags & ~RENAME_NOREPLACE)
			return -EINVAL;
		replaced = cladefs_nofs(cladefs_rename(fs, old_dir->i_ino, old_dentry->d_name.name,
						 old_dentry->d_name.len, inode->i_ino, new_dir->i_ino,
						 new_dentry->d_name.name, new_dentry->d_name.len,
						 target ? target->i_ino : 0, &attr, &dir_attr,
						 &new_dir_attr));
		if (replaced < 0)
			return replaced;
		cladefs_vfs_removed(target, replaced);
	}
	cladefs_vfs_update_attr(inode, &attr);
	cladefs_vfs_update_attr(old_dir, &dir_attr);
	cladefs_vfs_update_attr(new_dir, &new_dir_attr);
	return 0;
}

/*
 * Sets the attributes `iattr` says, as the VFS has them (setattr_copy: the current time unless the
 * caller gives one, set-group-ID cleared as it must be), on the inode, which writes them back to
 * Fxfs (write_inode), as ext4's journal does: a change of size at once (cladefs_data_resize).
 */
static int cladefs_vfs_setattr(struct mnt_idmap *idmap, struct dentry *dentry, struct iattr *iattr)
{
	struct inode *inode = d_inode(dentry);
	loff_t size = iattr->ia_size;
	int err = setattr_prepare(idmap, dentry, iattr);

	if (err)
		return err;
	if (!(iattr->ia_valid & ATTR_SIZE) || size == i_size_read(inode)) {
		setattr_copy(idmap, inode, iattr);
		mark_inode_dirty(inode);
	} else {
		/* truncate(2) leaves the times to the filesystem, which changing the size changes. */
		if (!(iattr->ia_valid & (ATTR_MTIME | ATTR_CTIME))) {
			iattr->ia_ctime = current_time(inode);
			iattr->ia_mtime = iattr->ia_ctime;
			iattr->ia_valid |= ATTR_MTIME | ATTR_CTIME;
		}
		err = cladefs_data_resize(inode, size, idmap, iattr);
	}
	/*
	 * cladefs: the access ACL's entries follow the mode, as ext4's: the mask, or the group's, as
	 * the group's bits; a chmod left a named user what the mode took away (TASK-79.96).
	 */
	if (!err && iattr->ia_valid & ATTR_MODE)
		err = posix_acl_chmod(idmap, dentry, inode->i_mode);
	return err;
}

/* FS_IOC_GETFLAGS and FS_IOC_FSGETXATTR (lsattr): the inode's attribute flags. */
static int cladefs_vfs_fileattr_get(struct dentry *dentry, struct file_kattr *fa)
{
	fileattr_fill_flags(fa, READ_ONCE(cladefs_vfs_inode(d_inode(dentry))->fsflags));
	return 0;
}

/*
 * FS_IOC_SETFLAGS and FS_IOC_FSSETXATTR (chattr): sets the inode's attribute flags, those Fxfs
 * keeps (CLADEFS_VFS_FL), which the VFS checks the caller may change, and writes them back at once,
 * as a change of size is.
 */
static int cladefs_vfs_fileattr_set(struct mnt_idmap *idmap, struct dentry *dentry,
				 struct file_kattr *fa)
{
	struct inode *inode = d_inode(dentry);
	struct cladefs_vfs_inode *fi = cladefs_vfs_inode(inode);
	struct timespec64 ctime;
	int err;

	if (fileattr_has_fsx(fa) || fa->flags & ~CLADEFS_VFS_FL)
		return -EOPNOTSUPP;
	mutex_lock(&fi->attr_lock);
	ctime = current_time(inode);
	err = cladefs_vfs_store_attr(inode, 0, fa->flags, ctime);
	if (!err) {
		cladefs_vfs_set_flags(inode, fa->flags);
		inode_set_ctime_to_ts(inode, ctime);
	}
	mutex_unlock(&fi->attr_lock);
	return err;
}

/* Limit the number of pieces copied per lookup, irrespective of the distance between them. */
#define CLADEFS_EXTENT_BATCH 256

/*
 * Calls `fn` with each piece of `inode`'s data within `start` to `end`, in order, a batch at a
 * time, until it returns nonzero, which this returns; or 0, or a -errno.
 */
static int cladefs_vfs_each_extent(struct inode *inode, loff_t start, loff_t end,
				int (*fn)(void *arg, const struct cladefs_dax_extent *e), void *arg)
{
	struct cladefs_dax_extent *extents;
	loff_t at = round_down(start, PAGE_SIZE);
	int ret = 0;

	extents = kmalloc_array(CLADEFS_EXTENT_BATCH, sizeof(*extents), GFP_KERNEL);
	if (!extents)
		return -ENOMEM;
	while (!ret && at < end) {
		u64 len = end - at;
		ssize_t count, i;

		count = cladefs_nofs(cladefs_extent_batch(cladefs_vfs_fs(inode->i_sb), inode->i_ino, at, len,
						   extents, CLADEFS_EXTENT_BATCH));
		if (count < 0) {
			ret = count;
			break;
		}
		for (i = 0; i < count && !ret; i++)
			ret = fn(arg, &extents[i]);
		if (count < CLADEFS_EXTENT_BATCH)
			break;
		at = extents[count - 1].offset + extents[count - 1].len;
	}
	kfree(extents);
	return ret;
}

/* SEEK_DATA's or SEEK_HOLE's search from `offset`, which finds `found`. */
struct cladefs_vfs_seek {
	loff_t offset;
	int whence;
	loff_t found;
};

static int cladefs_vfs_seek_extent(void *arg, const struct cladefs_dax_extent *e)
{
	struct cladefs_vfs_seek *seek = arg;
	loff_t start = e->offset, end = e->offset + e->len;

	if (end <= seek->offset)
		return 0;
	if (seek->whence == SEEK_DATA) {
		seek->found = max(start, seek->offset);
		return 1;
	}
	/* Data from `found` on, so far: a gap before this piece is the hole. */
	if (start > seek->found)
		return 1;
	seek->found = max(seek->found, end);
	return 0;
}

/* llseek(2), with SEEK_DATA and SEEK_HOLE from Fxfs's pieces of the file's data. */
loff_t cladefs_vfs_llseek(struct file *file, loff_t offset, int whence)
{
	struct inode *inode = file->f_mapping->host;
	struct cladefs_vfs_seek seek = { .offset = offset, .whence = whence };
	loff_t size;
	int err;

	if (whence != SEEK_DATA && whence != SEEK_HOLE)
		return generic_file_llseek(file, offset, whence);
	inode_lock_shared(inode);
	size = i_size_read(inode);
	if (offset < 0 || offset >= size) {
		inode_unlock_shared(inode);
		return -ENXIO;
	}
	seek.found = whence == SEEK_DATA ? size : offset;
	/*
	 * Data in the page cache has no pieces until written back. cladefs: nor a DAX file's dirty
	 * pages (cladefs_dax_write_back).
	 */
	err = filemap_write_and_wait(inode->i_mapping);
	if (!err)
		err = cladefs_dax_write_back(inode);
	if (!err)
		err = cladefs_vfs_each_extent(inode, offset, size, cladefs_vfs_seek_extent, &seek);
	inode_unlock_shared(inode);
	if (err < 0)
		return err;
	if (whence == SEEK_DATA && !err)
		return -ENXIO;
	return vfs_setpos(file, min(seek.found, size), inode->i_sb->s_maxbytes);
}

/* fiemap's extent being gathered: pieces of one Fxfs extent, contiguous in it and in the file. */
struct cladefs_vfs_fiemap {
	struct fiemap_extent_info *fieinfo;
	bool have;
	u64 logical, len;
	u64 cell, sequence, extent_end;
};

/* Reports the extent gathered, if any, with `flags`: 1 once the caller's array is full. */
static int cladefs_vfs_fiemap_flush(struct cladefs_vfs_fiemap *m, u32 flags)
{
	if (!m->have)
		return 0;
	m->have = false;
	/* An extent store's extents have no address on a device. */
	return fiemap_fill_next_extent(m->fieinfo, m->logical, 0, m->len,
				       FIEMAP_EXTENT_UNKNOWN | flags);
}

static int cladefs_vfs_fiemap_extent(void *arg, const struct cladefs_dax_extent *e)
{
	struct cladefs_vfs_fiemap *m = arg;
	int ret;

	if (m->have && m->logical + m->len == e->offset && m->cell == e->cell &&
	    m->sequence == e->sequence && m->extent_end == e->extent_offset) {
		m->len += e->len;
		m->extent_end += e->len;
		return 0;
	}
	ret = cladefs_vfs_fiemap_flush(m, 0);
	if (ret)
		return ret;
	m->have = true;
	m->logical = e->offset;
	m->len = e->len;
	m->cell = e->cell;
	m->sequence = e->sequence;
	m->extent_end = e->extent_offset + e->len;
	return 0;
}

/* FS_IOC_FIEMAP: the file's data's extents, without addresses (FIEMAP_EXTENT_UNKNOWN). */
static int cladefs_vfs_fiemap(struct inode *inode, struct fiemap_extent_info *fieinfo, u64 start,
			   u64 len)
{
	struct cladefs_vfs_fiemap m = { .fieinfo = fieinfo };
	loff_t size, end;
	int err = fiemap_prep(inode, fieinfo, start, &len, 0);

	if (err)
		return err;
	inode_lock_shared(inode);
	size = i_size_read(inode);
	end = start >= size ? start : min_t(u64, size, start + len);
	err = filemap_write_and_wait(inode->i_mapping);
	/* cladefs: a DAX file's dirty pages too (cladefs_dax_write_back). */
	if (!err)
		err = cladefs_dax_write_back(inode);
	if (!err)
		err = cladefs_vfs_each_extent(inode, start, end, cladefs_vfs_fiemap_extent, &m);
	if (!err)
		err = cladefs_vfs_fiemap_flush(&m, end == size ? FIEMAP_EXTENT_LAST : 0);
	inode_unlock_shared(inode);
	return err < 0 ? err : 0;
}

/* Writes zeros over the `len` bytes of `inode` at `pos`, within a page, to Fxfs. */
static int cladefs_vfs_write_zeros(struct inode *inode, loff_t pos, loff_t len)
{
	void *zeros = kzalloc(len, GFP_KERNEL);
	int err;

	if (!zeros)
		return -ENOMEM;
	err = cladefs_nofs(cladefs_write(cladefs_vfs_fs(inode->i_sb), inode->i_ino, pos, zeros, len));
	kfree(zeros);
	return err;
}

static int cladefs_vfs_xattr_get(const struct xattr_handler *handler, struct dentry *dentry,
			      struct inode *inode, const char *name, void *buf, size_t size)
{
	const char *full = xattr_full_name(handler, name);
	ssize_t ret;

	/* cladefs: into the kernel's buffer, which the VFS copies out once this returns. */
	ret = cladefs_nofs(cladefs_getxattr(cladefs_vfs_fs(inode->i_sb), inode->i_ino, full, strlen(full),
				      buf, size));
	return ret;
}

/* Sets an extended attribute, or removes it, without a value. Its ctime is Fxfs's (refresh). */
static int cladefs_vfs_xattr_set(const struct xattr_handler *handler, struct mnt_idmap *idmap,
			      struct dentry *dentry, struct inode *inode, const char *name,
			      const void *value, size_t size, int flags)
{
	const char *full = xattr_full_name(handler, name);
	struct cladefs *fs = cladefs_vfs_fs(inode->i_sb);
	int err;

	if (value)
		err = cladefs_nofs(cladefs_setxattr(fs, inode->i_ino, full, strlen(full), value, size,
					      flags));
	else
		err = cladefs_nofs(cladefs_removexattr(fs, inode->i_ino, full, strlen(full)));
	if (err)
		return err;
	inode_set_ctime_current(inode);
	mark_inode_dirty(inode);
	return 0;
}

static const struct xattr_handler cladefs_vfs_user_xattr_handler = {
	.prefix = XATTR_USER_PREFIX,
	.get = cladefs_vfs_xattr_get,
	.set = cladefs_vfs_xattr_set,
};

static const struct xattr_handler cladefs_vfs_trusted_xattr_handler = {
	.prefix = XATTR_TRUSTED_PREFIX,
	.get = cladefs_vfs_xattr_get,
	.set = cladefs_vfs_xattr_set,
};

static const struct xattr_handler cladefs_vfs_security_xattr_handler = {
	.prefix = XATTR_SECURITY_PREFIX,
	.get = cladefs_vfs_xattr_get,
	.set = cladefs_vfs_xattr_set,
};

static const struct xattr_handler *const cladefs_vfs_xattr_handlers[] = {
	&cladefs_vfs_user_xattr_handler,
	&cladefs_vfs_trusted_xattr_handler,
	&cladefs_vfs_security_xattr_handler,
	NULL,
};

static ssize_t cladefs_vfs_listxattr(struct dentry *dentry, char *buf, size_t size)
{
	struct inode *inode = d_inode(dentry);
	ssize_t ret;

	ret = cladefs_nofs(cladefs_listxattr(cladefs_vfs_fs(inode->i_sb), inode->i_ino, buf, size));
	return ret;
}

static const char *cladefs_vfs_get_link(struct dentry *dentry, struct inode *inode,
				     struct delayed_call *done)
{
	ssize_t len;
	char *target;

	/* Reading the target may sleep, which a walk under RCU can't. */
	if (!dentry)
		return ERR_PTR(-ECHILD);
	target = kmalloc(PATH_MAX, GFP_KERNEL);
	if (!target)
		return ERR_PTR(-ENOMEM);
	len = cladefs_nofs(cladefs_readlink(cladefs_vfs_fs(inode->i_sb), inode->i_ino, target,
				      PATH_MAX - 1));
	if (len < 0 || len >= PATH_MAX) {
		kfree(target);
		return ERR_PTR(len < 0 ? len : -ENAMETOOLONG);
	}
	target[len] = 0;
	set_delayed_call(done, kfree_link, target);
	return target;
}

/* The most bytes of entries an open directory reads from Fxfs at once (cladefs_vfs_dir_cursor). */
#define CLADEFS_DIR_BUFFER (32 << 10)

/* An entry read from Fxfs, in a cursor's buffer: its name follows, padded to 8 bytes. */
struct cladefs_vfs_dir_entry {
	u64 ino;
	u32 mode;
	u16 len;
	char name[];
};

/*
 * Where reading an open directory got to: the position after the last entry it read, and that
 * entry's name, which the next read continues after, whatever was removed meanwhile. And the
 * entries read ahead of it, from `buffered_pos` on: read from Fxfs into its buffer, each of the
 * directory's reads seeking its trees' layers for where it left off, or the directory's listing.
 */
struct cladefs_vfs_dir_cursor {
	loff_t pos;
	size_t len;
	char name[NAME_MAX + 1];
	/* Its own, for what it reads from Fxfs; and the entries it has, in it or `listing`'s. */
	char *buffer;
	const char *entries;
	struct cladefs_vfs_dir_listing *listing;
	size_t buffered, at;
	loff_t buffered_pos;
	/* Whether the buffer holds the directory's last entries. */
	bool last;
};

static size_t cladefs_vfs_dir_entry_size(size_t len)
{
	return ALIGN(sizeof(struct cladefs_vfs_dir_entry) + len, 8);
}

/* Adds an entry Fxfs read to the cursor's buffer, if it has room: false if not. */
static bool cladefs_vfs_buffer_entry(void *arg, const char *name, size_t len, u64 ino, u32 mode)
{
	struct cladefs_vfs_dir_cursor *cursor = arg;
	struct cladefs_vfs_dir_entry *entry;

	if (len > NAME_MAX || cursor->buffered + cladefs_vfs_dir_entry_size(len) > CLADEFS_DIR_BUFFER)
		return false;
	entry = (struct cladefs_vfs_dir_entry *)(cursor->buffer + cursor->buffered);
	entry->ino = ino;
	entry->mode = mode;
	entry->len = len;
	memcpy(entry->name, name, len);
	cursor->buffered += cladefs_vfs_dir_entry_size(len);
	return true;
}

/*
 * Keeps the entries in the cursor's buffer as the directory's listing: all of them, read from the
 * start, under the directory's lock shared, which no change of them holds meanwhile. Kept if it
 * can be: a read without is only slower.
 */
static void cladefs_vfs_dir_keep(struct inode *inode, struct cladefs_vfs_dir_cursor *cursor)
{
	struct cladefs_vfs_dir_listing *listing;

	listing = kvmalloc(struct_size(listing, entries, cursor->buffered), GFP_KERNEL);
	if (!listing)
		return;
	refcount_set(&listing->refs, 1);
	listing->len = cursor->buffered;
	memcpy(listing->entries, cursor->buffer, cursor->buffered);
	spin_lock(&inode->i_lock);
	/* Another read's from the start may have kept the same entries first. */
	if (!cladefs_vfs_inode(inode)->listing)
		swap(cladefs_vfs_inode(inode)->listing, listing);
	spin_unlock(&inode->i_lock);
	cladefs_vfs_dir_put_listing(listing);
}

/*
 * Reads the directory's entries from position `pos` on into the cursor's buffer, as many as fit:
 * after the cursor's name if it is the entry before `pos`, as a read continuing does. From the
 * start, the directory's listing if it has one, or what it reads kept as one if that's all of them.
 */
static int cladefs_vfs_dir_fill(struct inode *inode, struct cladefs_vfs_dir_cursor *cursor, loff_t pos)
{
	bool resume = cursor->len && cursor->pos == pos;
	int err;

	cladefs_vfs_dir_put_listing(cursor->listing);
	cursor->listing = NULL;
	cursor->buffered = 0;
	cursor->at = 0;
	cursor->buffered_pos = pos;
	cursor->last = false;
	if (pos == 2 && !resume) {
		cursor->listing = cladefs_vfs_dir_get_listing(inode);
		if (cursor->listing) {
			cursor->entries = cursor->listing->entries;
			cursor->buffered = cursor->listing->len;
			cursor->last = true;
			return 0;
		}
	}
	if (!cursor->buffer) {
		cursor->buffer = kvmalloc(CLADEFS_DIR_BUFFER, GFP_KERNEL);
		if (!cursor->buffer)
			return -ENOMEM;
	}
	cursor->entries = cursor->buffer;
	err = cladefs_nofs(cladefs_readdir(cladefs_vfs_fs(inode->i_sb), inode->i_ino, pos - 2,
				     resume ? cursor->name : NULL, cursor->len,
				     cladefs_vfs_buffer_entry, cursor));
	/* Whatever didn't fit is still to read. */
	cursor->last = !err && cursor->buffered + cladefs_vfs_dir_entry_size(NAME_MAX) <=
			       CLADEFS_DIR_BUFFER;
	if (err)
		cursor->buffered = 0;
	else if (pos == 2 && !resume && cursor->last)
		cladefs_vfs_dir_keep(inode, cursor);
	return err;
}

static int cladefs_vfs_dir_open(struct inode *inode, struct file *file)
{
	file->private_data = kzalloc(sizeof(struct cladefs_vfs_dir_cursor), GFP_KERNEL);
	return file->private_data ? 0 : -ENOMEM;
}

static int cladefs_vfs_dir_release(struct inode *inode, struct file *file)
{
	struct cladefs_vfs_dir_cursor *cursor = file->private_data;

	cladefs_vfs_dir_put_listing(cursor->listing);
	kvfree(cursor->buffer);
	kfree(cursor);
	return 0;
}

/*
 * Positions 0 and 1 are "." and "..", and the filesystem's entries follow, in order. A read
 * continuing where the last left off continues after the last name it read, which removing
 * entries before it doesn't move; another, after a seek, counts entries. Entries come from the
 * cursor's buffer, which a read filled ahead, as long as the read continues where it left off.
 */
static int cladefs_vfs_iterate(struct file *file, struct dir_context *ctx)
{
	struct inode *inode = file_inode(file);
	struct cladefs_vfs_dir_cursor *cursor = file->private_data;
	const struct cladefs_vfs_dir_entry *entry;
	int err;

	if (!dir_emit_dots(file, ctx))
		return 0;
	for (;;) {
		if (cursor->buffered_pos != ctx->pos || cursor->at == cursor->buffered) {
			/* At the end of the buffer, which held the last entries. */
			if (cursor->buffered_pos == ctx->pos && cursor->last && cursor->buffered)
				return 0;
			err = cladefs_vfs_dir_fill(inode, cursor, ctx->pos);
			if (err)
				return err;
			if (!cursor->buffered)
				return 0;
		}
		entry = (const struct cladefs_vfs_dir_entry *)(cursor->entries + cursor->at);
		if (!dir_emit(ctx, entry->name, entry->len, entry->ino,
			      fs_umode_to_dtype(entry->mode)))
			return 0;
		ctx->pos++;
		memcpy(cursor->name, entry->name, entry->len);
		cursor->len = entry->len;
		cursor->pos = ctx->pos;
		cursor->at += cladefs_vfs_dir_entry_size(entry->len);
		cursor->buffered_pos = ctx->pos;
	}
}

/* Fills `folio` with file `inode`'s data, and zeros past its end. */
static int cladefs_vfs_fill_folio(struct inode *inode, struct folio *folio)
{
	size_t size = folio_size(folio);
	void *addr = folio_address(folio);
	ssize_t n;

	n = cladefs_nofs(cladefs_read(cladefs_vfs_fs(inode->i_sb), inode->i_ino, folio_pos(folio), addr,
				size));
	if (n < 0)
		return n;
	memset(addr + n, 0, size - n);
	return 0;
}

static int cladefs_vfs_read_folio(struct file *file, struct folio *folio)
{
	int err = cladefs_vfs_fill_folio(folio->mapping->host, folio);

	if (!err)
		folio_mark_uptodate(folio);
	folio_unlock(folio);
	return err;
}

/*
 * cladefs: a readahead's read: its folios, locked, filled from file `ino` from `pos` on, the parts
 * cladefs_read_into reads them into. Each holds its folios until it unlocks them: truncation and
 * eviction wait for those locks.
 */
struct cladefs_vfs_read {
	struct work_struct work;
	struct cladefs *fs;
	u64 ino;
	loff_t pos;
	unsigned int nr;
	struct folio **folios;
	struct cladefs_read_part parts[];
};

/* Fills `read`'s folios straight from the device, zeros past the file's end, and unlocks them. */
static void cladefs_vfs_read_folios(struct cladefs_vfs_read *read)
{
	ssize_t n = cladefs_nofs(cladefs_read_into(read->fs, read->ino, read->pos, read->parts, read->nr));
	size_t at = 0;
	unsigned int i;

	for (i = 0; i < read->nr; i++) {
		struct folio *folio = read->folios[i];
		size_t size = folio_size(folio);
		size_t have = n > (ssize_t)at ? min_t(size_t, n - at, size) : 0;

		/* A folio left not uptodate is read again, alone (cladefs_vfs_read_folio). */
		if (n >= 0) {
			memset(folio_address(folio) + have, 0, size - have);
			folio_mark_uptodate(folio);
		}
		folio_unlock(folio);
		at += size;
	}
}

static void cladefs_vfs_read_work(struct work_struct *work)
{
	struct cladefs_vfs_read *read = container_of(work, struct cladefs_vfs_read, work);

	cladefs_vfs_read_folios(read);
	kfree(read);
}

/*
 * Reads the folios of `rac` with one read, from the device's transfer buffer straight into them.
 * cladefs: off the reader, on the filesystem's readahead workqueue, which `rac`'s reader then waits
 * for only on reaching a folio it hasn't filled yet: before, each window blocked the reader for
 * a round trip, and two copies through buffers as large as it (TASK-92.9). Folios it can't have
 * the memory for are read one at a time (cladefs_vfs_read_folio).
 */
static void cladefs_vfs_readahead(struct readahead_control *rac)
{
	struct inode *inode = rac->mapping->host;
	struct cladefs_vfs_info *info = cladefs_vfs_info(inode->i_sb);
	unsigned int nr = readahead_count(rac), i = 0;
	struct cladefs_vfs_read *read;
	struct folio *folio;

	/* Folios at least a page each: room for as many folios as pages. */
	read = kmalloc(struct_size(read, parts, nr) + nr * sizeof(*read->folios), GFP_NOFS);
	if (!read)
		return;
	read->fs = info->fs;
	read->ino = inode->i_ino;
	read->pos = readahead_pos(rac);
	read->folios = (struct folio **)&read->parts[nr];
	while ((folio = readahead_folio(rac))) {
		read->folios[i] = folio;
		read->parts[i].buf = folio_address(folio);
		read->parts[i].len = folio_size(folio);
		i++;
	}
	read->nr = i;
	INIT_WORK(&read->work, cladefs_vfs_read_work);
	queue_work(info->filesystem->readahead, &read->work);
}

/*
 * cladefs: which pages of a folio are dirty, so that writeback writes those alone, as iomap's
 * iomap_folio_state has them: a write(2) of a database's page, 8 KiB, dirtied the whole folio,
 * which readahead made as large as a megabyte, and writeback wrote it whole: 13 GB a minute under
 * pgbench (TASK-91.12). And which of them have room reserved in Fxfs, which their writeback
 * writes them with (TASK-80): a buffered write reserved nothing, so never found the filesystem
 * full, and its writeback, failing, dropped the data. Those a write(2), or a store through a
 * mapping (cladefs_vfs_page_mkwrite), dirtied, as they dirtied them; not those dirtied otherwise,
 * whole (cladefs_vfs_dirty_folio), which their writeback finds room for if it can. Reserved pages are
 * dirty, but while a write readies them, under the folio's lock. Attached as the folio's private
 * data, guarded by its lock, as it is first dirtied; without it, a dirty folio is dirty whole, with
 * no room reserved.
 */
struct cladefs_vfs_folio_state {
	spinlock_t lock;
	/* The dirty pages' bits, then the reserved pages' (cladefs_vfs_reserved_bits). */
	unsigned long bits[];
};

/* A folio's pages, at most, that writeback writes some of (the files' folios' largest, super.c). */
#define CLADEFS_VFS_FOLIO_PAGES (CLADEFS_IO_MAX / PAGE_SIZE)

static unsigned long *cladefs_vfs_dirty_bits(struct cladefs_vfs_folio_state *state)
{
	return state->bits;
}

static unsigned long *cladefs_vfs_reserved_bits(struct cladefs_vfs_folio_state *state,
					     struct folio *folio)
{
	return state->bits + BITS_TO_LONGS(folio_nr_pages(folio));
}

/*
 * `folio`'s state, made if `make` and it has none, with every page dirty if the folio is: NULL
 * without memory for it.
 */
static struct cladefs_vfs_folio_state *cladefs_vfs_folio_state(struct folio *folio, bool make)
{
	struct cladefs_vfs_folio_state *state = folio_get_private(folio);
	unsigned long pages = folio_nr_pages(folio);

	if (state || !make || pages > CLADEFS_VFS_FOLIO_PAGES)
		return state;
	state = kzalloc(struct_size(state, bits, 2 * BITS_TO_LONGS(pages)), GFP_NOFS);
	if (!state)
		return NULL;
	spin_lock_init(&state->lock);
	if (folio_test_dirty(folio))
		bitmap_fill(cladefs_vfs_dirty_bits(state), pages);
	folio_attach_private(folio, state);
	return state;
}

/* cladefs: the bytes of the pages `bits`, `pages` of them, has set. */
static size_t cladefs_vfs_pages_bytes(const unsigned long *bits, unsigned long pages)
{
	return (size_t)bitmap_weight(bits, pages) << PAGE_SHIFT;
}

/*
 * cladefs: reserves room in Fxfs for the pages of `folio`, locked, from `offset` for `len` bytes,
 * that have none, which a write or a store then dirties (TASK-80): 0, or -ENOSPC if the filesystem
 * hasn't room for them, or -ENOMEM.
 */
static int cladefs_vfs_reserve_range(struct folio *folio, size_t offset, size_t len)
{
	struct cladefs_vfs_folio_state *state = cladefs_vfs_folio_state(folio, true);
	unsigned long first = offset >> PAGE_SHIFT, last = (offset + len - 1) >> PAGE_SHIFT, i;
	struct inode *inode = folio->mapping->host;
	unsigned long *reserved;
	size_t bytes = 0;
	int err;

	if (!state)
		return -ENOMEM;
	reserved = cladefs_vfs_reserved_bits(state, folio);
	for (i = first; i <= last; i++)
		if (!test_bit(i, reserved))
			bytes += PAGE_SIZE;
	if (!bytes)
		return 0;
	err = cladefs_nofs(cladefs_reserve_dirty(cladefs_vfs_fs(inode->i_sb), inode->i_ino, bytes));
	if (err)
		return err;
	spin_lock(&state->lock);
	bitmap_set(reserved, first, last - first + 1);
	spin_unlock(&state->lock);
	return 0;
}

/*
 * cladefs: releases the room reserved for the pages of `folio`, locked, from `offset` for `len`
 * bytes, that weren't dirtied: a write short of what it readied (cladefs_vfs_reserve_range).
 */
static void cladefs_vfs_unreserve_clean(struct folio *folio, size_t offset, size_t len)
{
	struct cladefs_vfs_folio_state *state = folio_get_private(folio);
	unsigned long first = offset >> PAGE_SHIFT, last = (offset + len - 1) >> PAGE_SHIFT, i;
	struct inode *inode = folio->mapping->host;
	unsigned long *reserved;
	size_t bytes = 0;

	if (!state || !len)
		return;
	reserved = cladefs_vfs_reserved_bits(state, folio);
	spin_lock(&state->lock);
	for (i = first; i <= last; i++) {
		if (test_bit(i, reserved) && !test_bit(i, cladefs_vfs_dirty_bits(state))) {
			__clear_bit(i, reserved);
			bytes += PAGE_SIZE;
		}
	}
	spin_unlock(&state->lock);
	if (bytes)
		cladefs_release_dirty(cladefs_vfs_fs(inode->i_sb), inode->i_ino, bytes);
}

/*
 * cladefs: makes room in the filesystem for a write that found it full: syncs it, writing back its
 * dirty data, which frees what was reserved for it beyond what its writeback took, and the
 * removed files it holds still, as ext4's delayed allocation does (ext4_nonda_switch). Not while
 * it is frozen, or unmounted.
 *
 * Writes that find it full share syncs (TASK-149.25): one begun after this write found no room
 * makes what room this write's own would, so when one has finished by the time this write may
 * begin its own, it doesn't, and the caller tries again. One at a time, a sync each, 128 writers
 * at ENOSPC (xfstests' generic/269) each waited behind the others' syncs, for hours.
 */
void cladefs_vfs_make_room(struct super_block *sb)
{
	struct cladefs_vfs_filesystem *filesystem = cladefs_vfs_info(sb)->filesystem;
	/* The syncs begun before this write found no room, or as it did. */
	u64 begun = READ_ONCE(filesystem->room_syncs_begun);

	atomic64_inc(&filesystem->room_asked);
	if (!down_read_trylock(&sb->s_umount))
		return;
	mutex_lock(&filesystem->room_lock);
	/* They finish in the order they begin: the next one done began after. */
	if (filesystem->room_syncs_done <= begun) {
		WRITE_ONCE(filesystem->room_syncs_begun, filesystem->room_syncs_begun + 1);
		sync_filesystem(sb);
		WRITE_ONCE(filesystem->room_syncs_done, filesystem->room_syncs_done + 1);
	}
	mutex_unlock(&filesystem->room_lock);
	up_read(&sb->s_umount);
}

static int cladefs_vfs_zero_past_end(struct inode *inode, loff_t size, loff_t to);

/*
 * Readies the folio a write of `len` bytes at `pos` copies into, as large as the write, reading
 * what the write doesn't cover of it. cladefs: with room reserved for the pages it dirties, making
 * room if the filesystem has none (cladefs_vfs_make_room), or -ENOSPC (TASK-80).
 */
static int cladefs_vfs_write_begin(const struct kiocb *iocb, struct address_space *mapping,
				loff_t pos, unsigned int len, struct folio **foliop, void **fsdata)
{
	struct inode *inode = mapping->host;
	bool made_room = false;
	struct folio *folio;
	size_t offset;
	int err;

	/* cladefs: what a mapping stored past the end isn't the data between it and the write. */
	if (pos > i_size_read(inode)) {
		err = cladefs_vfs_zero_past_end(inode, i_size_read(inode), pos);
		if (err)
			return err;
	}
again:
	folio = __filemap_get_folio(mapping, pos >> PAGE_SHIFT, FGP_WRITEBEGIN | fgf_set_order(len),
				    mapping_gfp_mask(mapping));
	if (IS_ERR(folio))
		return PTR_ERR(folio);
	offset = offset_in_folio(folio, pos);
	err = cladefs_vfs_reserve_range(folio, offset, min_t(size_t, len, folio_size(folio) - offset));
	if (err == -ENOSPC && !made_room) {
		folio_unlock(folio);
		folio_put(folio);
		cladefs_vfs_make_room(inode->i_sb);
		made_room = true;
		goto again;
	}
	if (err)
		goto out;
	if (!folio_test_uptodate(folio) && (offset || len < folio_size(folio))) {
		if (folio_pos(folio) >= i_size_read(inode)) {
			folio_zero_range(folio, 0, folio_size(folio));
		} else {
			err = cladefs_vfs_fill_folio(inode, folio);
			if (err) {
				cladefs_vfs_unreserve_clean(folio, 0, folio_size(folio));
				goto out;
			}
		}
		folio_mark_uptodate(folio);
	}
	*foliop = folio;
	return 0;
out:
	folio_unlock(folio);
	folio_put(folio);
	return err;
}

/*
 * cladefs: dirties the folio, locked, as a write(2) of `len` bytes at `offset` in it does, its room
 * reserved (cladefs_vfs_reserve_range).
 */
static void cladefs_vfs_dirty_range(struct folio *folio, size_t offset, size_t len)
{
	struct cladefs_vfs_folio_state *state = cladefs_vfs_folio_state(folio, true);
	unsigned long first = offset >> PAGE_SHIFT, last = (offset + len - 1) >> PAGE_SHIFT;

	if (state) {
		spin_lock(&state->lock);
		bitmap_set(cladefs_vfs_dirty_bits(state), first, last - first + 1);
		spin_unlock(&state->lock);
	}
	filemap_dirty_folio(folio->mapping, folio);
}

/*
 * cladefs: dirties the folio whole, as anything but a write(2) does: a store through a mapping
 * (cladefs_vfs_page_mkwrite), which writeback write-protects again, so that the next faults to dirty
 * it. Its pages' room is as it was: a store's reserved as it faulted.
 */
static bool cladefs_vfs_dirty_folio(struct address_space *mapping, struct folio *folio)
{
	struct cladefs_vfs_folio_state *state = folio_get_private(folio);

	if (state) {
		spin_lock(&state->lock);
		bitmap_fill(cladefs_vfs_dirty_bits(state), folio_nr_pages(folio));
		spin_unlock(&state->lock);
	}
	return filemap_dirty_folio(mapping, folio);
}

/* cladefs: a clean folio's state goes with it. */
static bool cladefs_vfs_release_folio(struct folio *folio, gfp_t gfp)
{
	if (folio_test_dirty(folio) || folio_test_writeback(folio))
		return false;
	kfree(folio_detach_private(folio));
	return true;
}

/*
 * cladefs: zeroes what the cached folio holding the file's end, `size`, holds past it, up to `to`,
 * as the file grows: writeback writes no more than the file holds, and stores through a mapping
 * past its end became its data as it grew (TASK-79.53). The bytes zeroed are written back, now
 * the file's: with room reserved for them, or -ENOSPC (TASK-80).
 */
static int cladefs_vfs_zero_past_end(struct inode *inode, loff_t size, loff_t to)
{
	bool made_room = false;
	struct folio *folio;
	size_t from, end;
	int err = 0;

again:
	/* The folio holding the byte at `size`: if it starts there, nothing of it is past the end. */
	folio = filemap_lock_folio(inode->i_mapping, size >> PAGE_SHIFT);
	if (IS_ERR(folio))
		return 0;
	from = offset_in_folio(folio, size);
	end = min_t(loff_t, folio_size(folio), to - folio_pos(folio));
	if (from && from < end && folio_test_uptodate(folio)) {
		err = cladefs_vfs_reserve_range(folio, from, end - from);
		if (err == -ENOSPC && !made_room) {
			folio_unlock(folio);
			folio_put(folio);
			cladefs_vfs_make_room(inode->i_sb);
			made_room = true;
			goto again;
		}
		if (!err) {
			folio_zero_range(folio, from, end - from);
			cladefs_vfs_dirty_range(folio, from, end - from);
		}
	}
	folio_unlock(folio);
	folio_put(folio);
	return err;
}

/*
 * cladefs: as does a folio's dropped whole, and the room its dirty pages had reserved (TASK-80);
 * one truncated within keeps its, past the file's end, which its writeback then releases.
 */
static void cladefs_vfs_invalidate_folio(struct folio *folio, size_t offset, size_t len)
{
	struct cladefs_vfs_folio_state *state;
	struct inode *inode = folio->mapping->host;
	size_t reserved;

	if (offset || len != folio_size(folio))
		return;
	state = folio_detach_private(folio);
	if (!state)
		return;
	reserved = cladefs_vfs_pages_bytes(cladefs_vfs_reserved_bits(state, folio), folio_nr_pages(folio));
	if (reserved)
		cladefs_release_dirty(cladefs_vfs_fs(inode->i_sb), inode->i_ino, reserved);
	kfree(state);
}

/*
 * As libfs's simple_write_end: the folio dirty, and the file grown to hold the write. cladefs: the
 * pages it wrote alone dirty (`cladefs_vfs_folio_state`). A folio never read, which the write was to
 * cover whole (cladefs_vfs_write_begin), takes nothing of a short copy, as block_write_end has it:
 * ramfs's zeroed the rest, which on a file's existing data wrote zeros over it (TASK-79.14), and
 * generic_perform_write writes what was copied again, the folio read first.
 */
static int cladefs_vfs_write_end(const struct kiocb *iocb, struct address_space *mapping,
			      loff_t pos, unsigned int len, unsigned int copied,
			      struct folio *folio, void *fsdata)
{
	struct inode *inode = mapping->host;

	if (!folio_test_uptodate(folio)) {
		if (copied < len) {
			copied = 0;
			goto out;
		}
		folio_mark_uptodate(folio);
		cladefs_vfs_dirty_range(folio, 0, folio_size(folio));
	} else if (copied) {
		cladefs_vfs_dirty_range(folio, offset_in_folio(folio, pos), copied);
	}
	/* cladefs: nothing copied, past the end, the file stays as long as it was. */
	if (copied && pos + copied > inode->i_size)
		i_size_write(inode, pos + copied);
out:
	/* cladefs: the room of the pages a short copy didn't dirty (TASK-80). */
	cladefs_vfs_unreserve_clean(folio, offset_in_folio(folio, pos), len);
	folio_unlock(folio);
	folio_put(folio);
	return copied;
}

/*
 * Folios under writeback, contiguous or not, which one call to Fxfs writes, from their own memory:
 * a folio under writeback stays until its writeback ends (TASK-92.10).
 */
struct cladefs_vfs_run {
	struct inode *inode;
	struct folio *folios[CLADEFS_IO_MAX / PAGE_SIZE];
	/* Their data, as much of each as is before the file's end, if any is. */
	struct cladefs_write_part parts[CLADEFS_IO_MAX / PAGE_SIZE];
	/* cladefs: each part's folio, and its pages, `first` to `last`, exclusive (TASK-212). */
	struct cladefs_vfs_run_piece {
		struct folio *folio;
		unsigned long first, last;
	} pieces[CLADEFS_IO_MAX / PAGE_SIZE];
	unsigned int count, nr_parts;
	size_t len;
	/* cladefs: the bytes of the pages it writes whose room is reserved, which pays for them. */
	size_t reserved;
};

/*
 * cladefs: of a run Fxfs had no memory to write, its parts' pages dirty again, in their folios'
 * states and their folios, their room reserved again, what there is of it (the failed
 * cladefs_write_many took it), so that a writeback later writes them (TASK-212): clean, as an I/O
 * error's, they were lost, the store holding what they replaced, which a read after their eviction
 * read, as copy-on-write DAX's private pages were (TASK-207). Before their writeback ends, which a
 * truncation waits for, so that none comes between. A folio with no state, which its state found
 * no memory for, is dirty whole, as a store dirties one.
 */
static void cladefs_vfs_keep(struct cladefs_vfs_run *run)
{
	struct inode *inode = run->inode;
	size_t bytes = 0;
	unsigned int i;

	for (i = 0; i < run->nr_parts; i++) {
		struct cladefs_vfs_run_piece *piece = &run->pieces[i];
		struct cladefs_vfs_folio_state *state = folio_get_private(piece->folio);
		unsigned long page;

		if (state) {
			unsigned long *reserved = cladefs_vfs_reserved_bits(state, piece->folio);

			spin_lock(&state->lock);
			bitmap_set(cladefs_vfs_dirty_bits(state), piece->first,
				   piece->last - piece->first);
			for (page = piece->first; page < piece->last; page++)
				if (!__test_and_set_bit(page, reserved))
					bytes += PAGE_SIZE;
			spin_unlock(&state->lock);
		}
		filemap_dirty_folio(inode->i_mapping, piece->folio);
	}
	if (bytes)
		cladefs_reserve_dirty_what_fits(cladefs_vfs_fs(inode->i_sb), inode->i_ino, bytes);
}

/*
 * Writes `run`'s folios, and ends their writeback. cladefs: with the room reserved for them, which
 * is released with what the writes left of it, or, if it writes nothing, the file's end before
 * them, unused. What an I/O error stopped is clean, the error the file's, as any filesystem's;
 * what Fxfs had no memory to write is dirty still, and the run fails ENOMEM (cladefs_vfs_keep).
 */
static int cladefs_vfs_write_run(struct cladefs_vfs_run *run)
{
	struct cladefs *fs = cladefs_vfs_fs(run->inode->i_sb);
	int err = 0;
	unsigned int i;

	if (run->nr_parts) {
		err = cladefs_write_many(fs, run->inode->i_ino, run->parts, run->nr_parts,
				      run->reserved);
		/* cladefs: in a DAX mount, read in to the page cache until sealed. */
		cladefs_dax_written(run->inode);
	} else if (run->reserved)
		cladefs_release_dirty(fs, run->inode->i_ino, run->reserved);
	if (err == -ENOMEM)
		cladefs_vfs_keep(run);
	else if (err)
		mapping_set_error(run->inode->i_mapping, err);
	for (i = 0; i < run->count; i++)
		folio_end_writeback(run->folios[i]);
	run->count = 0;
	run->nr_parts = 0;
	run->len = 0;
	run->reserved = 0;
	return err;
}

/*
 * Writes back dirty folios, up to CLADEFS_IO_MAX bytes of them, contiguous or not, in one call. cladefs:
 * of a folio, the pages dirty alone (`cladefs_vfs_folio_state`), with the room reserved for them.
 */
static int cladefs_vfs_writepages(struct address_space *mapping, struct writeback_control *wbc)
{
	struct cladefs_vfs_run *run = kzalloc(sizeof(*run), GFP_NOFS);
	unsigned int nofs = memalloc_nofs_save();
	struct folio *folio = NULL;
	int err = 0, run_err;

	if (!run) {
		memalloc_nofs_restore(nofs);
		return -ENOMEM;
	}
	run->inode = mapping->host;
	while ((folio = writeback_iter(mapping, wbc, folio, &err))) {
		struct cladefs_vfs_folio_state *state = folio_get_private(folio);
		loff_t size = i_size_read(run->inode), pos = folio_pos(folio);
		size_t len = pos < size ? min_t(loff_t, folio_size(folio), size - pos) : 0;
		unsigned long pages = folio_nr_pages(folio), first = 0, last = pages;
		DECLARE_BITMAP(dirty, CLADEFS_VFS_FOLIO_PAGES);
		DECLARE_BITMAP(reserved, CLADEFS_VFS_FOLIO_PAGES);
		bool some = false;

		/*
		 * The pages dirty since its last writeback, which writes from now on dirty again, and
		 * reserve room again; with none, or no state, it is written whole.
		 */
		bitmap_zero(reserved, CLADEFS_VFS_FOLIO_PAGES);
		if (state) {
			unsigned long *reserved_bits = cladefs_vfs_reserved_bits(state, folio);

			spin_lock(&state->lock);
			bitmap_copy(dirty, cladefs_vfs_dirty_bits(state), pages);
			bitmap_copy(reserved, reserved_bits, pages);
			bitmap_zero(cladefs_vfs_dirty_bits(state), pages);
			bitmap_zero(reserved_bits, pages);
			spin_unlock(&state->lock);
			some = !bitmap_empty(dirty, pages);
		}
		/* A folio that doesn't fit the run starts another. */
		if (run->count == ARRAY_SIZE(run->folios)) {
			run_err = cladefs_vfs_write_run(run);
			err = err ?: run_err;
		}
		folio_start_writeback(folio);
		for (;;) {
			size_t from, to;

			if (some) {
				first = find_next_bit(dirty, pages, first);
				if (first >= pages)
					break;
				last = find_next_zero_bit(dirty, pages, first);
			}
			from = min_t(size_t, first << PAGE_SHIFT, len);
			to = min_t(size_t, last << PAGE_SHIFT, len);
			/* A piece that doesn't fit the run writes it; the folio's writeback goes on. */
			if (to > from && (run->len + (to - from) > CLADEFS_IO_MAX ||
					  run->nr_parts == ARRAY_SIZE(run->parts))) {
				run_err = cladefs_vfs_write_run(run);
				err = err ?: run_err;
			}
			if (to > from) {
				run->pieces[run->nr_parts] = (struct cladefs_vfs_run_piece){
					.folio = folio, .first = from >> PAGE_SHIFT,
					.last = DIV_ROUND_UP(to, PAGE_SIZE),
				};
				run->parts[run->nr_parts++] = (struct cladefs_write_part){
					.offset = pos + from, .data = folio_address(folio) + from,
					.len = to - from,
				};
				run->len += to - from;
			}
			/* Its pages' room, written by this run: past the file's end, released by it. */
			for (; first < last; first++)
				if (__test_and_clear_bit(first, reserved))
					run->reserved += PAGE_SIZE;
			if (!some)
				break;
		}
		/* Any room of pages not dirty goes with this run too. */
		run->reserved += cladefs_vfs_pages_bytes(reserved, pages);
		/* Its writeback ends with the run that writes its last piece. */
		run->folios[run->count++] = folio;
		folio_unlock(folio);
	}
	run_err = cladefs_vfs_write_run(run);
	err = err ?: run_err;
	kfree(run);
	memalloc_nofs_restore(nofs);
	return err;
}

/*
 * O_DIRECT's reads and writes of a file in the page cache: straight to and from Fxfs, through a
 * buffer, CLADEFS_IO_MAX bytes at a time, once the VFS has written back and dropped the page cache's
 * range (generic_file_read_iter, generic_file_direct_write). They needn't be aligned. A DAX file's
 * reads and writes are direct already (cladefs_dax_file_operations).
 */
static ssize_t cladefs_vfs_direct_IO(struct kiocb *iocb, struct iov_iter *iter)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	struct cladefs *fs = cladefs_vfs_fs(inode->i_sb);
	bool write = iov_iter_rw(iter) == WRITE;
	loff_t pos = iocb->ki_pos, size = i_size_read(inode);
	ssize_t done = 0, err = 0;
	size_t past_end = 0;
	void *buf;

	if (!write) {
		if (pos >= size)
			return 0;
		/*
		 * cladefs: what is past the file's end is given back as it returns, as the caller reverts
		 * the iterator by what it took: past its start, a BUG(), otherwise (TASK-79.15).
		 */
		past_end = iov_iter_count(iter) - min_t(u64, iov_iter_count(iter), size - pos);
		iov_iter_truncate(iter, size - pos);
	}
	buf = kvmalloc(min_t(size_t, iov_iter_count(iter), CLADEFS_IO_MAX), GFP_KERNEL);
	if (!buf) {
		iov_iter_reexpand(iter, iov_iter_count(iter) + past_end);
		return -ENOMEM;
	}
	while (iov_iter_count(iter)) {
		size_t len = min_t(size_t, iov_iter_count(iter), CLADEFS_IO_MAX);
		ssize_t n;

		if (write) {
			if (copy_from_iter(buf, len, iter) != len) {
				err = -EFAULT;
				break;
			}
			err = cladefs_nofs(cladefs_write(fs, inode->i_ino, pos + done, buf, len));
			cladefs_dax_written(inode);
			if (err)
				break;
			n = len;
		} else {
			n = cladefs_nofs(cladefs_read(fs, inode->i_ino, pos + done, buf, len));
			if (n > 0 && copy_to_iter(buf, n, iter) != n)
				n = -EFAULT;
		}
		if (n < 0) {
			err = n;
			break;
		}
		done += n;
		/* Fxfs's file ends here. */
		if ((size_t)n < len)
			break;
	}
	kvfree(buf);
	iov_iter_reexpand(iter, iov_iter_count(iter) + past_end);
	return done ?: err;
}

static const struct address_space_operations cladefs_vfs_aops = {
	.read_folio = cladefs_vfs_read_folio,
	.readahead = cladefs_vfs_readahead,
	.write_begin = cladefs_vfs_write_begin,
	.write_end = cladefs_vfs_write_end,
	.writepages = cladefs_vfs_writepages,
	.dirty_folio = cladefs_vfs_dirty_folio,
	.release_folio = cladefs_vfs_release_folio,
	.invalidate_folio = cladefs_vfs_invalidate_folio,
	.direct_IO = cladefs_vfs_direct_IO,
	/* Compaction moves its folios as any, their dirty pages' state with them (TASK-81). */
	.migrate_folio = filemap_migrate_folio,
};

/*
 * Writes back the file's data and times, and the filesystem's changes, in order (cladefs_fsync),
 * sealing what was written back. And a file's directory's times, which creates
 * and unlinks leave to the kernel (write_inode), as a journaling filesystem's fsync of a file
 * commits the change to its directory that made it: so that a sync of the directory after it has
 * nothing of its own to commit. cladefs: fdatasync writes back neither's times, as data doesn't
 * need them to be read back: only what does (I_DIRTY_DATASYNC), as generic_file_fsync. Fxfs has a
 * file's size already, which its writes of the data change (TASK-41).
 */
int cladefs_vfs_fsync(struct file *file, loff_t start, loff_t end, int datasync)
{
	struct inode *inode = file_inode(file);
	int err;

	trace_cladefs_fsync_start(inode->i_ino);
	err = file_write_and_wait_range(file, start, end);

	/*
	 * cladefs: a DAX file's dirty pages, which a write(2)'s leave the mapping untagged for, so that
	 * the writeback above may not have been asked for them (doc-13).
	 */
	if (!err)
		err = cladefs_dax_write_back(inode);
	if (!err && (!datasync || (READ_ONCE(inode->i_state) & I_DIRTY_DATASYNC)))
		err = sync_inode_metadata(inode, 1);
	if (!err && !datasync && !S_ISDIR(inode->i_mode)) {
		struct dentry *parent = dget_parent(file_dentry(file));

		err = sync_inode_metadata(d_inode(parent), 1);
		dput(parent);
	}
	if (!err)
		err = cladefs_nofs(cladefs_fsync(cladefs_vfs_fs(inode->i_sb)));
	trace_cladefs_fsync_end(inode->i_ino, err);
	return err;
}

static const struct inode_operations cladefs_vfs_dir_inode_operations = {
	.lookup = cladefs_vfs_lookup,
	.create = cladefs_vfs_create,
	.mkdir = cladefs_vfs_mkdir,
	.mknod = cladefs_vfs_mknod,
	.symlink = cladefs_vfs_symlink,
	.link = cladefs_vfs_link,
	.unlink = cladefs_vfs_unlink,
	.rmdir = cladefs_vfs_unlink,
	.rename = cladefs_vfs_rename,
	.tmpfile = cladefs_vfs_tmpfile,
	.setattr = cladefs_vfs_setattr,
	.getattr = cladefs_vfs_getattr,
	.listxattr = cladefs_vfs_listxattr,
	.get_inode_acl = cladefs_vfs_get_acl,
	.set_acl = cladefs_vfs_set_acl,
	.fileattr_get = cladefs_vfs_fileattr_get,
	.fileattr_set = cladefs_vfs_fileattr_set,
};

static const struct inode_operations cladefs_vfs_file_inode_operations = {
	.setattr = cladefs_vfs_setattr,
	.getattr = cladefs_vfs_getattr,
	.listxattr = cladefs_vfs_listxattr,
	.fiemap = cladefs_vfs_fiemap,
	.get_inode_acl = cladefs_vfs_get_acl,
	.set_acl = cladefs_vfs_set_acl,
	.fileattr_get = cladefs_vfs_fileattr_get,
	.fileattr_set = cladefs_vfs_fileattr_set,
};

static const struct inode_operations cladefs_vfs_symlink_inode_operations = {
	.get_link = cladefs_vfs_get_link,
	.setattr = cladefs_vfs_setattr,
	.getattr = cladefs_vfs_getattr,
	.listxattr = cladefs_vfs_listxattr,
};

static const struct inode_operations cladefs_vfs_special_inode_operations = {
	.setattr = cladefs_vfs_setattr,
	.getattr = cladefs_vfs_getattr,
	.listxattr = cladefs_vfs_listxattr,
	.get_inode_acl = cladefs_vfs_get_acl,
	.set_acl = cladefs_vfs_set_acl,
};

static long cladefs_vfs_dir_ioctl(struct file *file, unsigned int cmd, unsigned long arg);

static const struct file_operations cladefs_vfs_dir_operations = {
	.open = cladefs_vfs_dir_open,
	.release = cladefs_vfs_dir_release,
	.llseek = generic_file_llseek,
	.read = generic_read_dir,
	.iterate_shared = cladefs_vfs_iterate,
	.fsync = cladefs_vfs_fsync,
	.unlocked_ioctl = cladefs_vfs_dir_ioctl,
	.compat_ioctl = compat_ptr_ioctl,
};

/*
 * cladefs: a store through a shared mapping to a folio of a file in the page cache, which dirties it
 * whole (cladefs_vfs_dirty_folio), as filemap_page_mkwrite: with room reserved for its pages that
 * have none, making room if the filesystem has none (cladefs_vfs_make_room), or SIGBUS, as ext4's
 * delayed allocation has it (TASK-80).
 */
static vm_fault_t cladefs_vfs_page_mkwrite(struct vm_fault *vmf)
{
	struct inode *inode = file_inode(vmf->vma->vm_file);
	struct folio *folio = page_folio(vmf->page);
	vm_fault_t ret = VM_FAULT_LOCKED;
	bool made_room = false;
	int err;

	sb_start_pagefault(inode->i_sb);
	file_update_time(vmf->vma->vm_file);
again:
	folio_lock(folio);
	if (folio->mapping != inode->i_mapping) {
		folio_unlock(folio);
		ret = VM_FAULT_NOPAGE;
		goto out;
	}
	err = cladefs_vfs_reserve_range(folio, 0, folio_size(folio));
	if (err == -ENOSPC && !made_room) {
		folio_unlock(folio);
		cladefs_vfs_make_room(inode->i_sb);
		made_room = true;
		goto again;
	}
	if (err) {
		folio_unlock(folio);
		ret = vmf_fs_error(err);
		goto out;
	}
	/* Dirty already, so that writeback while freezing write-protects it again. */
	folio_mark_dirty(folio);
	folio_wait_stable(folio);
out:
	sb_end_pagefault(inode->i_sb);
	return ret;
}

static const struct vm_operations_struct cladefs_vfs_vm_ops = {
	.fault = filemap_fault,
	.map_pages = filemap_map_pages,
	.page_mkwrite = cladefs_vfs_page_mkwrite,
};

static int cladefs_vfs_mmap(struct file *file, struct vm_area_struct *vma)
{
	int err = generic_file_mmap(file, vma);

	if (!err)
		vma->vm_ops = &cladefs_vfs_vm_ops;
	return err;
}

/* cladefs: a page-cache file's size changes: what its cached page held past the end, zeroed. */
static int cladefs_vfs_cache_resize_begin(struct inode *inode, loff_t size)
{
	if (size <= i_size_read(inode))
		return 0;
	return cladefs_vfs_zero_past_end(inode, i_size_read(inode), size);
}

/*
 * cladefs: a hole punched: its range written back, and its folios gone whole, clean, not zeroed in
 * part: a partial page's zeros, in the cache before Fxfs has them, stayed there if it had no room,
 * what the file held coming back when the cache let them go. Its partial pages are zeroed in Fxfs
 * (cladefs_vfs_write_zeros).
 */
static int cladefs_vfs_cache_punch_begin(struct inode *inode, loff_t start, loff_t end)
{
	int err = filemap_write_and_wait_range(inode->i_mapping, start, end - 1);

	if (!err)
		err = invalidate_inode_pages2_range(inode->i_mapping, start >> PAGE_SHIFT,
						    (end - 1) >> PAGE_SHIFT);
	return err;
}

/*
 * cladefs: as every growth past the end: what the cached page held past it, up to the clone at
 * `pos`, is no data, stores through a mapping there for one (TASK-79.53, TASK-153.4). A clone from
 * before the end replaces the last block whole. Written back, as the VFS wrote back the clone's
 * range, so that its folio, which may reach into that range, goes clean.
 */
static int cladefs_vfs_cache_remap_begin(struct inode *inode, loff_t pos, loff_t len, loff_t size)
{
	loff_t old = i_size_read(inode);
	int err;

	if (pos <= old)
		return 0;
	err = cladefs_vfs_zero_past_end(inode, old, pos);
	if (!err)
		err = filemap_write_and_wait_range(inode->i_mapping, old, pos - 1);
	return err;
}

/*
 * Folios of a destination's range go whole, clean as the VFS left them: truncating a large one
 * partly would zero the part, which a read would find, rather than the clone's data.
 */
static int cladefs_vfs_cache_remapped(struct inode *inode, loff_t pos, loff_t len)
{
	return invalidate_inode_pages2_range(inode->i_mapping, pos >> PAGE_SHIFT,
					     (pos + len - 1) >> PAGE_SHIFT);
}

/* cladefs: a regular file in the page cache: what its data's changes do there (data.c). */
static const struct cladefs_data_ops cladefs_vfs_cache_data = {
	.resize_begin = cladefs_vfs_cache_resize_begin,
	.punch_begin = cladefs_vfs_cache_punch_begin,
	.zero = cladefs_vfs_write_zeros,
	.remap_begin = cladefs_vfs_cache_remap_begin,
	.remapped = cladefs_vfs_cache_remapped,
};

static const struct file_operations cladefs_vfs_file_operations = {
	.open = generic_file_open,
	.llseek = cladefs_vfs_llseek,
	.read_iter = generic_file_read_iter,
	.write_iter = generic_file_write_iter,
	.mmap = cladefs_vfs_mmap,
	.splice_read = filemap_splice_read,
	.splice_write = iter_file_splice_write,
	.fsync = cladefs_vfs_fsync,
	.fallocate = cladefs_data_fallocate,
	.remap_file_range = cladefs_data_remap_file_range,
};

/*
 * Closes the inode's handles in Fxfs, and frees an inode no name links: a file, or a directory
 * removed.
 */
static void cladefs_vfs_evict_inode(struct inode *inode)
{
	unsigned int nofs;

	/* No DAX entries are left for truncate_inode_pages_final, which has none to find. */
	dax_break_layout_final(inode);
	truncate_inode_pages_final(&inode->i_data);
	cladefs_dax_evict(inode);
	cladefs_vfs_dir_changes(inode);
	clear_inode(inode);
	nofs = memalloc_nofs_save();
	cladefs_evict(cladefs_vfs_fs(inode->i_sb), inode->i_ino, !inode->i_nlink);
	memalloc_nofs_restore(nofs);
}

/*
 * cladefs: fault injection (TASK-199): the next write-back of the inode this numbers fails (-EIO),
 * and it is 0 again; 0, none.
 */
static unsigned long cladefs_vfs_fail_write_inode;
module_param_named(fail_write_inode, cladefs_vfs_fail_write_inode, ulong, 0644);
MODULE_PARM_DESC(fail_write_inode, "Fail the next write-back of this inode's attributes (fault injection)");

/*
 * Writes back the attributes the kernel keeps (cladefs_vfs_write_attr). cladefs: failing, the inode is
 * dirty again, for the next write-back to retry, and the next fsync to report until one works: the
 * VFS took it for clean before the call, and the flusher's and sync's, which nothing reports, left
 * the next fsync nothing to write and 0 to return, the change lost at eviction (TASK-199).
 */
static int cladefs_vfs_write_inode(struct inode *inode, struct writeback_control *wbc)
{
	unsigned long ino = inode->i_ino;
	int err;

	if (READ_ONCE(cladefs_vfs_fail_write_inode) == ino &&
	    cmpxchg(&cladefs_vfs_fail_write_inode, ino, 0) == ino)
		err = -EIO;
	else
		/* cladefs: a DAX file's dirty pages first, which Fxfs takes its size from (doc-13). */
		err = cladefs_dax_write_back(inode) ?: cladefs_vfs_write_attr(inode, 0);
	if (err && !(READ_ONCE(inode->i_state) & (I_WILL_FREE | I_FREEING)))
		mark_inode_dirty_sync(inode);
	return err;
}

/*
 * cladefs: makes what the volume of `sb` holds durable: the window's dirty pages written back before
 * the sync seals them, which sync's writepages may not have been asked for; then one durable
 * commit of the device, a round trip to object storage (TASK-371).
 */
static int cladefs_vfs_sync(struct super_block *sb)
{
	if (cladefs_vfs_info(sb)->dax) {
		int err = cladefs_dax_settle(cladefs_vfs_info(sb)->dax);

		if (err)
			return err;
	}
	return cladefs_nofs(cladefs_sync(cladefs_vfs_fs(sb)));
}

static int cladefs_vfs_sync_fs(struct super_block *sb, int wait)
{
	/* Frozen, it was made durable as it froze, and nothing has changed since. */
	if (sb->s_writers.frozen == SB_FREEZE_COMPLETE)
		return 0;
	/*
	 * cladefs: once, as the VFS asks waiting: it asks without first, then waiting (sync, syncfs,
	 * freeze_super's sync_filesystem), and each durable commit waits for object storage
	 * (TASK-371). Freezing, the freeze makes it durable (cladefs_vfs_freeze_fs).
	 */
	if (!wait || sb->s_writers.frozen == SB_FREEZE_FS)
		return 0;
	return cladefs_vfs_sync(sb);
}

/*
 * cladefs: fault injection (TASK-200): the next thaw of a filesystem fails before its step this
 * names, 1 telling the device, 2 rotating the journal's span, which is 0 again then
 * (cladefs_set_fail_thaw_at): it stays frozen, for the next thaw to go on from that step.
 */
static int cladefs_vfs_fail_thaw_at_set(const char *val, const struct kernel_param *kp)
{
	unsigned int step;
	int err = kstrtouint(val, 0, &step);

	if (err)
		return err;
	cladefs_set_fail_thaw_at(step);
	return 0;
}

static int cladefs_vfs_fail_thaw_at_get(char *buffer, const struct kernel_param *kp)
{
	return sysfs_emit(buffer, "%u\n", cladefs_fail_thaw_at());
}

static const struct kernel_param_ops cladefs_vfs_fail_thaw_at_ops = {
	.set = cladefs_vfs_fail_thaw_at_set,
	.get = cladefs_vfs_fail_thaw_at_get,
};
module_param_cb(fail_thaw_at, &cladefs_vfs_fail_thaw_at_ops, NULL, 0644);
MODULE_PARM_DESC(fail_thaw_at, "Fail the next thaw before this step: 1 telling the device, 2 rotating the journal's span (fault injection)");

/*
 * cladefs: whether `filesystem` is frozen, read from its single authoritative owner in Rust
 * (`cladefs_is_frozen`): false while closed (`filesystem->filesystem == NULL`). With `lock` held.
 */
static bool cladefs_vfs_filesystem_is_frozen(const struct cladefs_vfs_filesystem *filesystem)
{
	return filesystem->filesystem && cladefs_is_frozen(filesystem->filesystem);
}

/*
 * Freezing (fsfreeze, FIFREEZE): the VFS stops a volume's writers and syncs it, and once all the
 * filesystem's volumes are frozen, read-only ones too (cladefs_vfs_settle_frozen), the filesystem is
 * made durable, and holds its background work, compacting and freeing, which writes nothing until
 * it thaws: nothing is open on the device, the point a snapshot of the VM is taken at (cladefs's
 * TASK-39.1). With the first volume to thaw, it learns the device's cell, which a VM restored
 * from the snapshot has its own of. With the lock held.
 */
static int cladefs_vfs_filesystem_freeze(struct cladefs_vfs_filesystem *filesystem)
{
	struct cladefs_dax *dax = filesystem->dax;
	int err;

	lockdep_assert_held(&filesystem->lock);
	if (cladefs_vfs_filesystem_is_frozen(filesystem) ||
	    filesystem->volumes_frozen < filesystem->volumes)
		return 0;
	if (dax) {
		cladefs_dax_quiesce(dax);
		/*
		 * cladefs: settle any DAX dirty pages dirtied after SB_FREEZE_WRITE (e.g. mmap
		 * write faults before SB_FREEZE_PAGEFAULT, or pages kept dirty after -ENOMEM)
		 * before cladefs_freeze closes the device gate (board 12113).
		 */
		err = cladefs_dax_settle(dax);
		if (err) {
			cladefs_dax_thaw(dax);
			return err;
		}
	}
	err = cladefs_nofs(cladefs_freeze(filesystem->filesystem));
	if (err) {
		if (dax)
			cladefs_dax_thaw(dax);
		return err;
	}
	/* cladefs: the window's private pages rebound, for the snapshot (doc-13). */
	if (dax)
		cladefs_dax_frozen(dax);
	return 0;
}

/*
 * cladefs: thaws the filesystem, if it is frozen. A thaw that fails leaves it frozen, its DAX
 * quiesced, as the VFS leaves a superblock whose unfreeze_fs failed: the next thaw goes on from the
 * step this one stopped at (cladefs_thaw), where taking the filesystem for thawed had writers pass
 * the VFS to wait for ever at the device Fxfs still held closed (TASK-200). With the lock held.
 */
static int cladefs_vfs_filesystem_thaw(struct cladefs_vfs_filesystem *filesystem)
{
	int err;

	lockdep_assert_held(&filesystem->lock);
	if (!cladefs_vfs_filesystem_is_frozen(filesystem))
		return 0;
	err = cladefs_nofs(cladefs_thaw(filesystem->filesystem));
	if (err)
		return err;
	if (filesystem->dax)
		cladefs_dax_thaw(filesystem->dax);
	/*
	 * cladefs: a store resized while the VM was away, snapshotted, told it nothing: what the device
	 * says now it takes as if told, before anything thawed sees the filesystem's size.
	 */
	schedule_work(&filesystem->resize);
	flush_work(&filesystem->resize);
	return 0;
}

/* cladefs: the volume of `info` frozen, and its filesystem with it, if it was the last. */
static int cladefs_vfs_volume_freeze(struct cladefs_vfs_info *info)
{
	struct cladefs_vfs_filesystem *filesystem = info->filesystem;
	int err;

	lockdep_assert_held(&filesystem->lock);
	if (WARN_ON_ONCE(info->frozen))
		return 0;
	info->frozen = true;
	filesystem->volumes_frozen++;
	err = cladefs_vfs_filesystem_freeze(filesystem);
	if (err) {
		info->frozen = false;
		filesystem->volumes_frozen--;
	}
	return err;
}

/*
 * cladefs: the volume of `info` thawed, and its filesystem, if frozen, first: a failed thaw leaves
 * both frozen.
 */
static int cladefs_vfs_volume_thaw(struct cladefs_vfs_info *info)
{
	struct cladefs_vfs_filesystem *filesystem = info->filesystem;
	int err;

	lockdep_assert_held(&filesystem->lock);
	/* A count that would wrap, as a thaw retried once did (TASK-200). */
	if (WARN_ON_ONCE(!info->frozen || !filesystem->volumes_frozen))
		return 0;
	err = cladefs_vfs_filesystem_thaw(filesystem);
	if (err)
		return err;
	info->frozen = false;
	filesystem->volumes_frozen--;
	return 0;
}

/*
 * cladefs: what the VFS synced as it froze the volume made durable, once (cladefs_vfs_sync_fs leaves
 * it to the freeze, TASK-371): by the filesystem's freeze, if this volume is its last to freeze,
 * else by a sync of its own, before the volume counts as frozen, which a sync failing leaves it
 * not.
 */
static int cladefs_vfs_freeze_fs(struct super_block *sb)
{
	struct cladefs_vfs_info *info = cladefs_vfs_info(sb);
	struct cladefs_vfs_filesystem *filesystem = info->filesystem;
	int err = 0;

	mutex_lock(&filesystem->lock);
	if (filesystem->volumes_frozen + 1 < filesystem->volumes)
		err = cladefs_vfs_sync(sb);
	if (!err)
		err = cladefs_vfs_volume_freeze(info);
	mutex_unlock(&filesystem->lock);
	return err;
}

static int cladefs_vfs_unfreeze_fs(struct super_block *sb)
{
	struct cladefs_vfs_info *info = cladefs_vfs_info(sb);
	int err;

	mutex_lock(&info->filesystem->lock);
	err = cladefs_vfs_volume_thaw(info);
	mutex_unlock(&info->filesystem->lock);
	return err;
}

/*
 * cladefs: a read-only superblock, frozen or thawed as the VFS says, taken so by its filesystem: the
 * VFS freezes and thaws one without a call to the filesystem (freeze_super calls no freeze_fs for
 * it), whose freeze, waiting for every superblock, would otherwise never come, and its VM never be
 * snapshotted, though each fsfreeze returned 0 (TASK-202). A writable one's freeze_fs and
 * unfreeze_fs keep. Returns 1 if it froze or thawed the volume, 0 if not, or a -errno: a thaw that failed leaves the
 * filesystem frozen, for the next to go on. With the superblock's s_umount held, which keeps what
 * the VFS says of it.
 */
static int cladefs_vfs_settle_frozen(struct super_block *sb)
{
	struct cladefs_vfs_info *info = cladefs_vfs_info(sb);
	struct cladefs_vfs_filesystem *filesystem = info->filesystem;
	bool frozen = sb->s_writers.frozen == SB_FREEZE_COMPLETE;
	int err = 0;

	lockdep_assert_held(&sb->s_umount);
	if (!sb_rdonly(sb))
		return 0;
	mutex_lock(&filesystem->lock);
	if (frozen && !info->frozen)
		err = cladefs_vfs_volume_freeze(info) ?: 1;
	else if (!frozen && info->frozen)
		err = cladefs_vfs_volume_thaw(info) ?: 1;
	mutex_unlock(&filesystem->lock);
	return err;
}

/*
 * cladefs: FIFREEZE's, and the kernel's, freeze of a superblock (freeze_super), then its
 * filesystem's of a read-only one (cladefs_vfs_settle_frozen): failing, the superblock is thawed
 * again, as a writable one stays thawed if its freeze_fs fails.
 */
static int cladefs_vfs_freeze_super(struct super_block *sb, enum freeze_holder who,
				 const void *owner)
{
	int err = freeze_super(sb, who, owner);

	if (err)
		return err;
	down_read(&sb->s_umount);
	err = cladefs_vfs_settle_frozen(sb);
	up_read(&sb->s_umount);
	if (err < 0) {
		thaw_super(sb, who, owner);
		return err;
	}
	return 0;
}

/*
 * cladefs: FITHAW's, and the kernel's, thaw of a superblock (thaw_super), then its filesystem's of a
 * read-only one (cladefs_vfs_settle_frozen). One whose filesystem failed to thaw is thawed already
 * for the VFS, which refuses it a second thaw (-EINVAL): that retries the filesystem's.
 */
static int cladefs_vfs_thaw_super(struct super_block *sb, enum freeze_holder who, const void *owner)
{
	int err = thaw_super(sb, who, owner), settled;

	down_read(&sb->s_umount);
	settled = cladefs_vfs_settle_frozen(sb);
	up_read(&sb->s_umount);
	if (settled < 0)
		return settled;
	return settled ? 0 : err;
}

static int cladefs_vfs_statfs(struct dentry *dentry, struct kstatfs *buf)
{
	struct cladefs_statfs statfs;
	int err;

	err = cladefs_nofs(cladefs_statfs(cladefs_vfs_fs(dentry->d_sb), &statfs));
	if (err)
		return err;
	buf->f_type = CLADEFS_MAGIC;
	buf->f_bsize = statfs.block_size;
	buf->f_blocks = statfs.blocks;
	buf->f_bfree = statfs.free_blocks;
	buf->f_bavail = statfs.free_blocks;
	buf->f_files = statfs.files;
	/* Objects take space as they need it: as many more as blocks are free. */
	buf->f_ffree = statfs.free_blocks;
	buf->f_namelen = NAME_MAX;
	return 0;
}

static struct inode *cladefs_vfs_alloc_inode(struct super_block *sb)
{
	struct cladefs_vfs_inode *fi = alloc_inode_sb(sb, cladefs_vfs_inode_cache, GFP_KERNEL);

	if (!fi)
		return NULL;
	/* cladefs: in the page cache, unless it is made a DAX inode (cladefs_dax_set_inode). */
	fi->data = &cladefs_vfs_cache_data;
	cladefs_dax_inode_init(&fi->dax);
	fi->listing = NULL;
	mutex_init(&fi->attr_lock);
	fi->fsflags = 0;
	fi->blocks_as_of = 0;
	return &fi->vfs;
}

static void cladefs_vfs_free_inode(struct inode *inode)
{
	kmem_cache_free(cladefs_vfs_inode_cache, cladefs_vfs_inode(inode));
}

static void cladefs_vfs_inode_init_once(void *fi)
{
	inode_init_once(&((struct cladefs_vfs_inode *)fi)->vfs);
}

/*
 * What the mount is, as /proc/mounts shows it: the volume, if it isn't the default (volume), the
 * one option Fxfs takes; and files map their data from the device's DAX window, if it has one
 * (dax).
 */
static int cladefs_vfs_show_options(struct seq_file *m, struct dentry *root)
{
	struct cladefs_vfs_info *info = cladefs_vfs_info(root->d_sb);

	if (strcmp(info->volume, CLADEFS_DEFAULT_VOLUME))
		seq_show_option(m, "volume", info->volume);
	if (info->dax)
		seq_puts(m, ",dax");
	return 0;
}

/*
 * Fxfs's cached records, which the superblock's shrinker counts, with its dentries and inodes, and
 * frees its share of under memory pressure (and drop_caches' 2).
 */
static long cladefs_vfs_nr_cached_objects(struct super_block *sb, struct shrink_control *sc)
{
	return min_t(u64, cladefs_cached_objects(cladefs_vfs_fs(sb)), LONG_MAX);
}

static long cladefs_vfs_free_cached_objects(struct super_block *sb, struct shrink_control *sc)
{
	return cladefs_nofs(cladefs_free_cached_objects(cladefs_vfs_fs(sb), sc->nr_to_scan));
}

static const struct super_operations cladefs_vfs_super_operations = {
	.show_options = cladefs_vfs_show_options,
	.alloc_inode = cladefs_vfs_alloc_inode,
	.free_inode = cladefs_vfs_free_inode,
	.statfs = cladefs_vfs_statfs,
	.evict_inode = cladefs_vfs_evict_inode,
	.write_inode = cladefs_vfs_write_inode,
	.sync_fs = cladefs_vfs_sync_fs,
	.freeze_fs = cladefs_vfs_freeze_fs,
	.unfreeze_fs = cladefs_vfs_unfreeze_fs,
	.freeze_super = cladefs_vfs_freeze_super,
	.thaw_super = cladefs_vfs_thaw_super,
	.nr_cached_objects = cladefs_vfs_nr_cached_objects,
	.free_cached_objects = cladefs_vfs_free_cached_objects,
};

/*
 * File handles (name_to_handle_at, open_by_handle_at, NFS): an inode's 64-bit number, generation 0,
 * as Fxfs never reuses object IDs. Without parent pointers in Fxfs, a directory's handle can't be
 * reconnected to its path.
 */
static int cladefs_vfs_encode_fh(struct inode *inode, u32 *fh, int *max_len, struct inode *parent)
{
	struct fid *fid = (struct fid *)fh;

	if (parent)
		return FILEID_INVALID;
	if (*max_len < 3) {
		*max_len = 3;
		return FILEID_INVALID;
	}
	fid->i64.ino = inode->i_ino;
	fid->i64.gen = 0;
	*max_len = 3;
	return FILEID_INO64_GEN;
}

static struct dentry *cladefs_vfs_fh_to_dentry(struct super_block *sb, struct fid *fid, int fh_len,
					    int fh_type)
{
	struct cladefs_attr attr;
	struct inode *inode;
	int err;

	if (fh_type != FILEID_INO64_GEN || fh_len < 3)
		return NULL;
	inode = ilookup(sb, fid->i64.ino);
	if (!inode) {
		err = cladefs_nofs(cladefs_getattr(cladefs_vfs_fs(sb), fid->i64.ino, &attr));
		inode = err ? ERR_PTR(err == -ENOENT ? -ESTALE : err) : cladefs_vfs_iget(sb, &attr);
		if (IS_ERR(inode))
			return ERR_CAST(inode);
	}
	return d_obtain_alias(inode);
}

static const struct export_operations cladefs_vfs_export_operations = {
	.encode_fh = cladefs_vfs_encode_fh,
	.fh_to_dentry = cladefs_vfs_fh_to_dentry,
};

/* cladefs: takes the size the host resized the store to. */
static void cladefs_vfs_resize_work(struct work_struct *work)
{
	struct cladefs_vfs_filesystem *filesystem = container_of(work, struct cladefs_vfs_filesystem, resize);
	int err = cladefs_nofs(cladefs_resize(filesystem->filesystem));

	if (err)
		pr_warn("cladefs: resizing: %d\n", err);
}

/*
 * cladefs: the host resized the store, or asks the guest to shrink: from the device's interrupt. The
 * work finds what changed, if anything did, in the device's configuration.
 */
static void cladefs_vfs_changed(void *arg)
{
	struct cladefs_vfs_filesystem *filesystem = arg;

	schedule_work(&filesystem->resize);
}

/* cladefs: a directory's ioctls (vfs.h). */
static long cladefs_vfs_dir_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct super_block *sb = file_inode(file)->i_sb;
	u64 cached;

	switch (cmd) {
	case CLADEFS_IOC_CACHED_OBJECTS:
		cached = cladefs_cached_objects(cladefs_vfs_fs(sb));
		return copy_to_user((void __user *)arg, &cached, sizeof(cached)) ? -EFAULT : 0;
	default:
		return -ENOTTY;
	}
}

/* cladefs: /proc/fs/cladefs, where each filesystem says what it waited on, by its device's tag. */
static struct proc_dir_entry *cladefs_vfs_proc;

/*
 * cladefs: /proc/fs/cladefs/build, the commit the filesystem was built of (`commit COMMIT`), as
 * scripts/build-commit.sh said (the Makefile's CLADEFS_BUILD_COMMIT, TASK-111.7). A device tagged
 * `build` keeps no stats there.
 */
static int cladefs_vfs_build_show(struct seq_file *m, void *v)
{
	seq_printf(m, "commit %s\n", CLADEFS_BUILD_COMMIT);
	return 0;
}

/*
 * cladefs: what the filesystem `m` is of waited on, and its device's and window's requests took, as
 * lines of a name and a number, all told since it opened (TASK-91.7): Fxfs waits on the device in
 * the kernel, which counts it as idle, not iowait, so that nothing else tells those waits apart.
 */
static int cladefs_vfs_stats_show(struct seq_file *m, void *v)
{
	struct cladefs_vfs_filesystem *filesystem = m->private;
	char *buf = kmalloc(PAGE_SIZE, GFP_KERNEL);
	size_t len;

	if (!buf)
		return -ENOMEM;
	len = cladefs_stats(filesystem->filesystem, buf, PAGE_SIZE);
	seq_write(m, buf, min_t(size_t, len, PAGE_SIZE));
	kfree(buf);
	cladefs_device_show_stats(cladefs_dax_device(filesystem->filesystem), m);
	seq_printf(m, "vfs_room_asked %lld\nvfs_room_syncs %llu\n",
		   atomic64_read(&filesystem->room_asked), READ_ONCE(filesystem->room_syncs_done));
	if (filesystem->dax)
		cladefs_dax_show_stats(filesystem->dax, m);
	return 0;
}

/*
 * cladefs: what the filesystem `arg` is of says to the host's asks for its stats (the driver's
 * F_STATS, TASK-111.4), in the `len` bytes at `buf`: what /proc/fs/cladefs/<its tag> says, and its
 * build, as /proc/fs/cladefs/build says it. One show function writes both, here through a seq_file
 * over `buf`, which writes nothing past it: -EOVERFLOW if it would have.
 */
static ssize_t cladefs_vfs_stats_text(void *arg, char *buf, size_t len)
{
	struct seq_file m = { .buf = buf, .size = len, .private = arg };
	int err = cladefs_vfs_stats_show(&m, NULL);

	if (err)
		return err;
	/* It can't fail: its result is the seq_file's, whose overflow counts. */
	(void)cladefs_vfs_build_show(&m, NULL);
	return seq_has_overflowed(&m) ? -EOVERFLOW : m.count;
}

/* The filesystems known, which outlive their mounts (`struct cladefs_vfs_filesystem`). */
static LIST_HEAD(cladefs_vfs_filesystems);
static DEFINE_MUTEX(cladefs_vfs_filesystems_lock);

/*
 * The filesystem of the device `vext`, new if none was mounted on it: NULL without the memory. It
 * only knows `vext` by its address: the superblocks hold the device while it is open, and one
 * bound at that address later finds it closed.
 */
static struct cladefs_vfs_filesystem *cladefs_vfs_filesystem_of(struct virtio_extent *vext)
{
	struct cladefs_vfs_filesystem *filesystem;

	mutex_lock(&cladefs_vfs_filesystems_lock);
	list_for_each_entry(filesystem, &cladefs_vfs_filesystems, list) {
		if (filesystem->vext == vext)
			goto out;
	}
	filesystem = kzalloc(sizeof(*filesystem), GFP_KERNEL);
	if (filesystem) {
		filesystem->vext = vext;
		mutex_init(&filesystem->lock);
		mutex_init(&filesystem->room_lock);
		INIT_WORK(&filesystem->resize, cladefs_vfs_resize_work);
		list_add(&filesystem->list, &cladefs_vfs_filesystems);
	}
out:
	mutex_unlock(&cladefs_vfs_filesystems_lock);
	return filesystem;
}

/* Closes the filesystem, whose last volume went, or as much of it as opened. */
static void cladefs_vfs_filesystem_close(struct cladefs_vfs_filesystem *filesystem)
{
	unsigned int nofs;

	/* Once its readers are done: cladefs: the host's asks for its stats', too (TASK-111.4). */
	virtio_extent_set_stats(filesystem->vext, NULL, NULL);
	proc_remove(filesystem->stats);
	filesystem->stats = NULL;
	if (filesystem->filesystem) {
		/* No resize comes once closing. */
		cladefs_device_set_changed(cladefs_dax_device(filesystem->filesystem), NULL, NULL);
		cancel_work_sync(&filesystem->resize);
	}
	/* Its volumes' inodes evicted, their folios' reads unlocked them: what's left is its free. */
	if (filesystem->readahead)
		destroy_workqueue(filesystem->readahead);
	filesystem->readahead = NULL;
	if (filesystem->filesystem) {
		nofs = memalloc_nofs_save();
		cladefs_close(filesystem->filesystem);
		memalloc_nofs_restore(nofs);
	}
	filesystem->filesystem = NULL;
	/* Once closed: the last seals call the DAX's `sealed`. */
	if (filesystem->dax)
		cladefs_dax_destroy(filesystem->dax);
	filesystem->dax = NULL;
}

/* Opens the filesystem on the extent-store device the mount's source names by its tag. */
static int cladefs_vfs_filesystem_open(struct cladefs_vfs_filesystem *filesystem, struct fs_context *fc)
{
	struct cladefs_filesystem *fs;
	struct cladefs_dax *dax;
	int err;

	err = cladefs_nofs(cladefs_open(fc->source, strlen(fc->source),
				  (u64)totalram_pages() << PAGE_SHIFT, fc->sb_flags & SB_RDONLY,
				  &fs));
	if (err)
		goto err_close;
	filesystem->filesystem = fs;
	cladefs_device_set_changed(cladefs_dax_device(fs), cladefs_vfs_changed, filesystem);
	/* cladefs: readahead off the reader (cladefs_vfs_readahead). */
	filesystem->readahead = alloc_workqueue("cladefs-read", WQ_UNBOUND, 0);
	if (!filesystem->readahead) {
		err = -ENOMEM;
		goto err_close;
	}
	dax = cladefs_dax_create(fs);
	if (IS_ERR(dax)) {
		err = PTR_ERR(dax);
		goto err_close;
	}
	filesystem->dax = dax;
	/* Without it, the filesystem works as well, unobserved: a tag a path can't hold has none. */
	if (cladefs_vfs_proc && !strchr(fc->source, '/'))
		filesystem->stats = proc_create_single_data(fc->source, 0444, cladefs_vfs_proc,
							    cladefs_vfs_stats_show, filesystem);
	/* cladefs: and the host's asks for its stats have it say them (TASK-111.4). */
	virtio_extent_set_stats(filesystem->vext, cladefs_vfs_stats_text, filesystem);
	return 0;
err_close:
	cladefs_vfs_filesystem_close(filesystem);
	return err;
}

/* The volume of the superblock `info` is of joins its device's filesystem, opened if it isn't. */
static int cladefs_vfs_filesystem_join(struct cladefs_vfs_info *info, struct fs_context *fc)
{
	struct cladefs_vfs_filesystem *filesystem = cladefs_vfs_filesystem_of(info->vext);
	int err = 0;

	if (!filesystem)
		return -ENOMEM;
	mutex_lock(&filesystem->lock);
	if (cladefs_vfs_filesystem_is_frozen(filesystem))
		err = -EBUSY;
	else if (!filesystem->volumes)
		err = cladefs_vfs_filesystem_open(filesystem, fc);
	if (!err) {
		filesystem->volumes++;
		info->filesystem = filesystem;
		info->dax = filesystem->dax;
	}
	mutex_unlock(&filesystem->lock);
	return err;
}

/*
 * The volume of `info`, its inodes evicted, leaves its filesystem, which closes with the last. The
 * volumes left may all be frozen by then, which freezes the filesystem. A frozen filesystem has
 * none leave: a frozen superblock stays until it thaws. cladefs: but a read-only one an emergency
 * thaw thawed (do_thaw_all, which calls no thaw_super of a filesystem's), which writes nothing; the
 * last to leave thaws the filesystem, which then closes.
 */
static void cladefs_vfs_filesystem_leave(struct cladefs_vfs_info *info)
{
	struct cladefs_vfs_filesystem *filesystem = info->filesystem;
	unsigned int nofs;
	int err;

	mutex_lock(&filesystem->lock);
	WARN_ON_ONCE(cladefs_vfs_filesystem_is_frozen(filesystem) && !info->frozen);
	if (filesystem->volumes == 1 && cladefs_vfs_filesystem_is_frozen(filesystem)) {
		err = cladefs_vfs_filesystem_thaw(filesystem);
		if (err)
			pr_warn("cladefs: thawing as the last volume left: %d\n", err);
	}
	if (info->frozen) {
		info->frozen = false;
		filesystem->volumes_frozen--;
	}
	filesystem->volumes--;
	/*
	 * cladefs: the budget's worker, which cladefs_vfs_kill_sb stopped for the volume's eviction:
	 * the volume goes from the DAX with it stopped, and that stop is balanced for the volumes
	 * left. The last's holds until cladefs_dax_destroy. Stops are counted: another volume's
	 * eviction, or a freeze, holds the worker stopped still (board 12453).
	 */
	if (info->dax && filesystem->volumes) {
		if (info->dax_volume)
			cladefs_dax_remove_volume(info->dax, info->dax_volume);
		cladefs_dax_start(info->dax);
	}
	if (info->fs) {
		nofs = memalloc_nofs_save();
		cladefs_unmount(info->fs);
		memalloc_nofs_restore(nofs);
	}
	if (!filesystem->volumes) {
		cladefs_vfs_filesystem_close(filesystem);
	} else {
		err = cladefs_vfs_filesystem_freeze(filesystem);
		if (err)
			pr_warn("cladefs: freezing as a volume left: %d\n", err);
	}
	mutex_unlock(&filesystem->lock);
}

static int cladefs_vfs_fill_super(struct super_block *sb, struct fs_context *fc)
{
	struct cladefs_vfs_info *info = cladefs_vfs_info(sb);
	struct cladefs_filesystem *filesystem;
	struct cladefs_attr attr;
	struct inode *root;
	struct cladefs *fs;
	int err;

	/* Writeback for the page cache: the default backing device has none. */
	err = super_setup_bdi(sb);
	if (err)
		return err;
	/*
	 * cladefs: readahead as far as one call to Fxfs reads: at the default 128 KiB, a cold
	 * sequential read waited for one round trip each 128 KiB (TASK-92.9).
	 */
	sb->s_bdi->ra_pages = CLADEFS_IO_MAX / PAGE_SIZE;
	sb->s_bdi->io_pages = CLADEFS_IO_MAX / PAGE_SIZE;
	err = cladefs_vfs_filesystem_join(info, fc);
	if (err)
		return err;
	filesystem = info->filesystem->filesystem;
	err = cladefs_nofs(cladefs_mount(filesystem, info->volume, strlen(info->volume),
				   fc->sb_flags & SB_RDONLY, &fs, &attr));
	if (err)
		return err;
	info->fs = fs;
	/* cladefs: read-only, it writes nothing. */
	if (cladefs_read_only(fs))
		sb->s_flags |= SB_RDONLY;
	if (info->dax) {
		struct cladefs_dax_volume *volume = cladefs_dax_add_volume(info->dax, fs);

		if (IS_ERR(volume))
			return PTR_ERR(volume);
		info->dax_volume = volume;
	}
	sb->s_magic = CLADEFS_MAGIC;
	sb->s_op = &cladefs_vfs_super_operations;
	sb->s_export_op = &cladefs_vfs_export_operations;
	sb->s_xattr = cladefs_vfs_xattr_handlers;
	sb->s_blocksize = PAGE_SIZE;
	sb->s_blocksize_bits = PAGE_SHIFT;
	/*
	 * cladefs: a DAX file's offsets stop a chunk short: its ranges reckon in chunks' ends, which
	 * at the very end overflow a loff_t: a write spun, and a read saw a hole for what was written
	 * (xfstests generic/303, 525, 564).
	 */
	sb->s_maxbytes = info->dax ? round_down(MAX_LFS_FILESIZE, SZ_2M) : MAX_LFS_FILESIZE;
	sb->s_time_gran = 1;
	/*
	 * cladefs: 64 bits of nanoseconds, signed: the VFS clamps a time to these as it sets it, or the
	 * inode would keep one its next read in finds clamped (timespec64_to_ns, TASK-79.55).
	 */
	sb->s_time_min = KTIME_SEC_MIN + 1;
	sb->s_time_max = KTIME_SEC_MAX - 1;
	/*
	 * A write looks at an inode's security attributes once, not each time, until they change,
	 * as block devices' filesystems get from get_tree_bdev(): each look is a lookup in Fxfs.
	 */
	sb->s_flags |= SB_NOSEC;
	/* ACLs, as extended attributes (cladefs_vfs_get_acl). */
	sb->s_flags |= SB_POSIXACL;

	root = cladefs_vfs_iget(sb, &attr);
	if (IS_ERR(root))
		return PTR_ERR(root);
	sb->s_root = d_make_root(root);
	if (!sb->s_root)
		return -ENOMEM;
	return 0;
}

/* Whether `sb` is of the volume `fc` mounts, which the info `fc` holds for a new one names. */
static int cladefs_vfs_test_super(struct super_block *sb, struct fs_context *fc)
{
	struct cladefs_vfs_info *mounted = cladefs_vfs_info(sb), *info = fc->s_fs_info;

	return mounted->vext == info->vext && !strcmp(mounted->volume, info->volume);
}

/*
 * Readies a new superblock, which sget_fc gives the info `fc` holds. It runs under sb_lock, a
 * spinlock: what it needs is allocated before (cladefs_vfs_get_tree).
 */
static int cladefs_vfs_set_super(struct super_block *sb, struct fs_context *fc)
{
	return set_anon_super_fc(sb, fc);
}

/* Frees `info`, letting go of the device it holds. */
static void cladefs_vfs_info_free(struct cladefs_vfs_info *info)
{
	virtio_extent_put(info->vext);
	kfree(info->volume);
	kfree(info);
}

/*
 * One superblock per volume of a device: mounting a volume mounted already shares its superblock,
 * as get_tree_keyed has it. Mounted without `volume`, the default.
 */
static int cladefs_vfs_get_tree(struct fs_context *fc)
{
	const char *volume = fc->fs_private ?: CLADEFS_DEFAULT_VOLUME;
	struct virtio_extent *vext;
	struct cladefs_vfs_info *info;
	struct super_block *sb;
	int err;

	if (!fc->source)
		return invalf(fc, "cladefs: the source must be an extent store's tag");
	vext = virtio_extent_find(fc->source);
	if (!vext)
		return -ENOENT;
	info = kzalloc(sizeof(*info), GFP_KERNEL);
	if (!info) {
		virtio_extent_put(vext);
		return -ENOMEM;
	}
	info->vext = vext;
	info->volume = kstrdup(volume, GFP_KERNEL);
	if (!info->volume) {
		cladefs_vfs_info_free(info);
		return -ENOMEM;
	}
	info->vext = vext;
	fc->s_fs_info = info;
	sb = sget_fc(fc, cladefs_vfs_test_super, cladefs_vfs_set_super);
	/* A new superblock took it, and the device with it; a shared one, or none, did not. */
	if (fc->s_fs_info)
		cladefs_vfs_info_free(fc->s_fs_info);
	fc->s_fs_info = NULL;
	if (IS_ERR(sb))
		return PTR_ERR(sb);
	if (!sb->s_root) {
		err = cladefs_vfs_fill_super(sb, fc);
		if (err) {
			deactivate_locked_super(sb);
			return err;
		}
		sb->s_flags |= SB_ACTIVE;
	}
	fc->root = dget(sb->s_root);
	return 0;
}

/*
 * cladefs: a filesystem mounted read-only stays so, and a volume itself.
 * A volume remounted read-only is synced first, as ext4's and XFS's are, leaving nothing dirty: the
 * VFS freezes a read-only superblock without syncing it, and its filesystem's freeze waits for
 * nothing of it (TASK-202). One remounted writable that its filesystem counts frozen, which an
 * emergency thaw leaves (cladefs_vfs_filesystem_leave), thaws its filesystem first.
 */
static int cladefs_vfs_reconfigure(struct fs_context *fc)
{
	struct super_block *sb = fc->root->d_sb;
	bool read_only = fc->sb_flags_mask & SB_RDONLY ? fc->sb_flags & SB_RDONLY : sb_rdonly(sb);
	int err;

	if (!read_only && cladefs_read_only(cladefs_vfs_fs(sb)))
		return invalf(fc, "cladefs: mounted read-only: it stays so");
	if (fc->fs_private && strcmp(fc->fs_private, cladefs_vfs_info(sb)->volume))
		return invalf(fc, "cladefs: a mount's volume stays its own");
	if (read_only && !sb_rdonly(sb))
		return sync_filesystem(sb);
	if (!read_only && sb_rdonly(sb)) {
		err = cladefs_vfs_settle_frozen(sb);
		if (err < 0)
			return err;
	}
	return 0;
}

enum {
	CLADEFS_VFS_VOLUME,
};

static const struct fs_parameter_spec cladefs_vfs_parameters[] = {
	fsparam_string("volume", CLADEFS_VFS_VOLUME),
	{}
};

/* The one option: `volume`, the name of the volume to mount, which the context keeps. */
static int cladefs_vfs_parse_param(struct fs_context *fc, struct fs_parameter *param)
{
	struct fs_parse_result result;
	int opt = fs_parse(fc, cladefs_vfs_parameters, param, &result);

	if (opt < 0)
		return opt;
	kfree(fc->fs_private);
	fc->fs_private = param->string;
	param->string = NULL;
	return 0;
}

static void cladefs_vfs_free_context(struct fs_context *fc)
{
	kfree(fc->fs_private);
}

static const struct fs_context_operations cladefs_vfs_context_operations = {
	.parse_param = cladefs_vfs_parse_param,
	.free = cladefs_vfs_free_context,
	.get_tree = cladefs_vfs_get_tree,
	.reconfigure = cladefs_vfs_reconfigure,
};

static int cladefs_vfs_init_fs_context(struct fs_context *fc)
{
	fc->ops = &cladefs_vfs_context_operations;
	return 0;
}

static void cladefs_vfs_kill_sb(struct super_block *sb)
{
	struct cladefs_vfs_info *info = cladefs_vfs_info(sb);

	/*
	 * cladefs: the DAX's budget's worker holds no inode of the volume as the VFS evicts them all:
	 * the volume's leave balances this stop, if others stay (cladefs_vfs_filesystem_leave).
	 */
	if (info->dax)
		cladefs_dax_stop(info->dax);
	kill_anon_super(sb);
	if (info->filesystem)
		cladefs_vfs_filesystem_leave(info);
	cladefs_vfs_info_free(info);
}

static struct file_system_type cladefs_vfs_type = {
	.owner = THIS_MODULE,
	.name = "cladefs",
	/*
	 * Idmapped mounts, as container runtimes make: new inodes' owners are the caller's as the
	 * mount maps them (cladefs_vfs_new_uid), and the VFS maps the rest. Multigrain timestamps: a
	 * change's ctime is fine-grained once stat read the one before, which is then the change
	 * attribute too (nfsd's, without SB_I_VERSION: Fxfs keeps no change counter).
	 */
	.fs_flags = FS_ALLOW_IDMAP | FS_MGTIME,
	.parameters = cladefs_vfs_parameters,
	.init_fs_context = cladefs_vfs_init_fs_context,
	.kill_sb = cladefs_vfs_kill_sb,
};

static int __init cladefs_vfs_init(void)
{
	int err;

	cladefs_vfs_inode_cache = kmem_cache_create("cladefs_inode", sizeof(struct cladefs_vfs_inode), 0,
						 SLAB_RECLAIM_ACCOUNT | SLAB_ACCOUNT,
						 cladefs_vfs_inode_init_once);
	if (!cladefs_vfs_inode_cache)
		return -ENOMEM;
	err = cladefs_rt_init();
	if (err) {
		kmem_cache_destroy(cladefs_vfs_inode_cache);
		return err;
	}
	cladefs_init();
	cladefs_vfs_proc = proc_mkdir("fs/cladefs", NULL);
	if (cladefs_vfs_proc)
		proc_create_single("build", 0444, cladefs_vfs_proc, cladefs_vfs_build_show);
	err = register_filesystem(&cladefs_vfs_type);
	if (err) {
		proc_remove(cladefs_vfs_proc);
		cladefs_rt_exit();
		kmem_cache_destroy(cladefs_vfs_inode_cache);
	}
	return err;
}

static void __exit cladefs_vfs_exit(void)
{
	struct cladefs_vfs_filesystem *filesystem, *next;

	unregister_filesystem(&cladefs_vfs_type);
	list_for_each_entry_safe(filesystem, next, &cladefs_vfs_filesystems, list)
		kfree(filesystem);
	proc_remove(cladefs_vfs_proc);
	cladefs_rt_exit();
	rcu_barrier();
	kmem_cache_destroy(cladefs_vfs_inode_cache);
}

module_init(cladefs_vfs_init);
module_exit(cladefs_vfs_exit);

MODULE_DESCRIPTION("Fxfs, in Rust");
MODULE_LICENSE("Dual BSD/GPL");
MODULE_ALIAS_FS("cladefs");
