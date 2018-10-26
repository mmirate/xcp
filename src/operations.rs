use ignore::gitignore::{Gitignore, GitignoreBuilder};
use log::{debug, error, info};
use std::cmp;
use std::fs::{create_dir_all, read_link, File};
use std::io::ErrorKind as IOKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use walkdir::{DirEntry, WalkDir};

use crate::errors::{io_err, Result, XcpError};
use crate::os::copy_file_bytes;
use crate::progress::{
    iprogress_bar, BatchUpdater, NopUpdater, ProgressBar, ProgressUpdater, StatusUpdate, Updater,
    BATCH_DEFAULT,
};
use crate::utils::{FileType, ToFileType};
use crate::Opts;


#[derive(Debug)]
enum Operation {
    Copy(PathBuf, PathBuf),
    Link(PathBuf, PathBuf),
    CreateDir(PathBuf),
    End,
}


fn copy_file(from: &Path, to: &Path, updates: &mut BatchUpdater) -> Result<u64> {
    let infd = File::open(from)?;
    let outfd = File::create(to)?;
    let (perm, len) = {
        let metadata = infd.metadata()?;
        (metadata.permissions(), metadata.len())
    };

    let mut written = 0u64;
    while written < len {
        let bytes_to_copy = cmp::min(len - written, updates.batch_size);
        let result = copy_file_bytes(&infd, &outfd, bytes_to_copy)?;
        written += result;
        updates.update(Ok(result))?;
    }
    outfd.set_permissions(perm)?;
    Ok(written)
}


fn copy_worker(work: mpsc::Receiver<Operation>, mut updates: BatchUpdater) -> Result<()> {
    debug!("Starting copy worker {:?}", thread::current().id());
    for op in work {
        debug!("Received operation {:?}", op);

        // FIXME: If we implement parallel copies (which may
        // improve performance on some SSD configurations) we
        // should also created the parent directory, and the
        // dir-create operation could be out of order.
        match op {
            Operation::Copy(from, to) => {
                info!("Worker: Copy {:?} -> {:?}", from, to);
                let _res = copy_file(&from, &to, &mut updates);
            }

            Operation::Link(from, to) => {
                info!("Worker: Symlink {:?} -> {:?}", from, to);
                let _res = symlink(&from, &to);
            }

            Operation::CreateDir(dir) => {
                info!("Worker: Creating directory: {:?}", dir);
                create_dir_all(&dir)?;
                updates.update(Ok(dir.metadata()?.len()))?;
            }

            Operation::End => {
                info!("Worker received shutdown command.");
                break;
            }
        }
    }
    debug!("Copy worker {:?} shutting down", thread::current().id());
    Ok(())
}


fn ignore_filter(entry: &DirEntry, ignore: &Option<Gitignore>) -> bool {
    match ignore {
        None => true,
        Some(gi) => {
            let path = entry.path();
            let m = gi.matched(&path, path.is_dir());
            !m.is_ignore()
        }
    }
}

fn empty(path: &Path) -> bool {
    *path == PathBuf::new()
}

fn tree_walker(
    source: PathBuf,
    opts: Opts,
    work_tx: mpsc::Sender<Operation>,
    mut updates: BatchUpdater,
) -> Result<()> {
    debug!("Starting walk worker {:?}", thread::current().id());

    let sourcedir = source.components().last().ok_or(XcpError::InvalidSource {
        msg: "Failed to find source directory name.",
    })?;

    let target_base = if opts.dest.exists() {
        opts.dest.join(sourcedir)
    } else {
        opts.dest.clone()
    };
    debug!("Target base is {:?}", target_base);

    let gitignore = if opts.gitignore {
        let mut builder = GitignoreBuilder::new(&source);
        builder.add(&source.join(".gitignore"));
        let ignore = builder.build()?;
        Some(ignore)
    } else {
        None
    };

    for entry in WalkDir::new(&source).into_iter()
        .filter_entry(|e| ignore_filter(e, &gitignore))
    {
        debug!("Got tree entry {:?}", entry);
        let e = entry?;
        let from = e.into_path();
        let meta = from.symlink_metadata()?;
        let path = from.strip_prefix(&source)?;
        let target = if !empty(&path) {
            target_base.join(&path)
        } else {
            target_base.clone()
        };

        if target.exists() && opts.noclobber {
            work_tx.send(Operation::End)?;
            updates.update(Err(XcpError::DestinationExists {
                msg: "Destination file exists and --no-clobber is set.",
                path: target }.into()))?;
            return Err(XcpError::EarlyShutdown {
                msg: "Path exists and --no-clobber set.",
            }
            .into());
        }

        match meta.file_type().to_enum() {
            FileType::File => {
                debug!("Send copy operation {:?} to {:?}", from, target);
                updates.update(Ok(meta.len()))?;
                work_tx.send(Operation::Copy(from, target))?;
            }

            FileType::Symlink => {
                let lfile = read_link(from)?;
                debug!("Send symlink operation {:?} to {:?}", lfile, target);
                work_tx.send(Operation::Link(lfile, target))?;
            }

            FileType::Dir => {
                debug!("Send create-dir operation {:?} to {:?}", from, target);
                work_tx.send(Operation::CreateDir(target))?;
                updates.update(Ok(from.metadata()?.len()))?;
            }

            FileType::Unknown => {
                error!("Unknown filetype found; this should never happen!");
                work_tx.send(Operation::End)?;
                updates.update(Err(XcpError::UnknownFiletype { path: target }.into()))?;
            }
        };
    }

    work_tx.send(Operation::End)?;
    debug!("Walk-worker finished: {:?}", thread::current().id());
    Ok(())
}

pub fn copy_tree(source: PathBuf, opts: &Opts) -> Result<()> {
    let (work_tx, work_rx) = mpsc::channel();
    let (stat_tx, stat_rx) = mpsc::channel();

    let (pb, batch_size) = if opts.noprogress {
        (ProgressBar::Nop, usize::max_value() as u64)
    } else {
        (iprogress_bar(0), BATCH_DEFAULT)
    };


    let _copy_worker = {
        let copy_stat = BatchUpdater {
            sender: Box::new(stat_tx.clone()),
            stat: StatusUpdate::Copied(0),
            batch_size: batch_size,
        };
        thread::spawn(move || copy_worker(work_rx, copy_stat))
    };
    let _walk_worker = {
        let topts = opts.clone();
        let size_stat = BatchUpdater {
            sender: Box::new(stat_tx),
            stat: StatusUpdate::Size(0),
            batch_size: batch_size,
        };
        thread::spawn(move || tree_walker(source, topts, work_tx, size_stat))
    };

    let mut copied = 0;
    let mut total = 0;

    for stat in stat_rx {
        match stat? {
            StatusUpdate::Size(s) => {
                total += s;
                pb.set_size(total);
            }
            StatusUpdate::Copied(s) => {
                copied += s;
                pb.set_position(copied);
            }
        }
    }
    // FIXME: We should probably join the threads and consume any errors.

    pb.end();
    debug!("Copy-tree complete");

    Ok(())
}


pub fn copy_single_file(source: &PathBuf, opts: &Opts) -> Result<()> {
    let dest = if opts.dest.is_dir() {
        let fname = source.file_name().ok_or(XcpError::UnknownFilename)?;
        opts.dest.join(fname)
    } else {
        opts.dest.clone()
    };

    if dest.is_file() && opts.noclobber {
        return Err(io_err(
            IOKind::AlreadyExists,
            "Destination file exists and --no-clobber is set.",
        ));
    }


    let mut copy_stat = if opts.noprogress {
        BatchUpdater {
            sender: Box::new(NopUpdater {}),
            stat: StatusUpdate::Copied(0),
            batch_size: usize::max_value() as u64,
        }
    } else {
        let size = source.metadata()?.len();
        BatchUpdater {
            sender: Box::new(ProgressUpdater {
                pb: iprogress_bar(size),
                written: 0,
            }),
            stat: StatusUpdate::Copied(0),
            batch_size: BATCH_DEFAULT,
        }
    };

    copy_file(source, &dest, &mut copy_stat)?;

    Ok(())
}
